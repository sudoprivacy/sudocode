#![cfg(feature = "mailbox")]

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;

use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::{Kernel, OperationContext};
use runtime::{FsBackend, KernelFsBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn call(kernel: &Kernel, method: &str, payload: Value) -> Value {
    let ctx = OperationContext::new("test-owner", "root", false, Some("operator"), false);
    let bytes = kernel
        .dispatch_rust_call(
            "managed_agent",
            method,
            payload.to_string().as_bytes(),
            &ctx,
        )
        .expect("managed-agent installed")
        .expect("managed call succeeds");
    serde_json::from_slice(&bytes).unwrap()
}

fn send(mailbox: &SessionMailbox<Kernel>, message: Value) {
    mailbox.send(SessionPayload::Rpc { message }).unwrap();
}

fn until(
    mailbox: &SessionMailbox<Kernel>,
    predicate: impl Fn(&Value) -> bool,
) -> (Value, Vec<Value>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match mailbox.receive(100).unwrap() {
            Some(SessionPayload::Rpc { message }) => {
                if predicate(&message) {
                    return (message, seen);
                }
                assert!(
                    message.get("error").is_none(),
                    "unexpected RPC failure: {message}"
                );
                seen.push(message);
            }
            Some(SessionPayload::Closed { reason }) => panic!("session closed: {reason}"),
            None => {}
        }
    }
    panic!("timed out waiting for session message; seen: {seen:?}")
}

#[test]
fn cohost_waits_for_permission_cancels_and_runs_the_next_turn() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let service = rt
        .block_on(mock_anthropic_service::MockAnthropicService::spawn())
        .unwrap();
    let config_home = tempfile::tempdir().unwrap();
    std::fs::write(config_home.path().join("sudocode.json"), json!({
        "auth_modes":{"api-key":{"anthropic":{"baseUrl":"nexus:///model","apiKey":"test-cohost-key"}}},
        "models":{
            "scripted":{"alias":"scripted","name":"Scripted", "input":["text"],"providers":{"api-key":{"provider":"anthropic","model":"claude-sonnet-4-6"}}},
            "claude-sonnet":{"alias":"claude-sonnet","name":"Scripted child", "input":["text"],"providers":{"api-key":{"provider":"anthropic","model":"claude-sonnet-4-6"}}}
        }
    }).to_string()).unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config_home.path());
    let kernel = Arc::new(Kernel::new());
    managed_agent::install_managed_agent_with_spawn(
        &kernel,
        Arc::new(engine_acp::managed_agent::SudoCodeSpawnAdapter),
    )
    .unwrap();
    common::mount_agent_world(&kernel);
    let _model = common::mount_model(&kernel, "anthropic", &service.base_url(), "test-cohost-key");
    a2a::install_a2a_stamp_hook(&kernel, true).unwrap();
    let path = a2a::conversation_transcript_path(&a2a::conversation_id("worker", "operator"));
    common::provision_stream_transcript(&kernel, &path);
    let started = call(
        &kernel,
        "start_session_v1",
        json!({"agent_id":"worker","model":"scripted"}),
    );
    let endpoint: SessionEndpoint =
        serde_json::from_value(started["session_endpoint"].clone()).unwrap();
    let ctx = OperationContext::new("test-owner", "root", false, Some("operator"), false);
    let mailbox =
        SessionMailbox::open(Arc::clone(&kernel), ctx, endpoint, SessionSide::Controller).unwrap();
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
    );
    until(&mailbox, |message| message["id"] == "init");
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"new","method":"session/new","params":{"cwd":"/","mcpServers":[]}}),
    );
    let (created, _) = until(&mailbox, |message| message["id"] == "new");
    assert!(created.get("error").is_none(), "{created}");
    let sid = created["result"]["sessionId"].as_str().unwrap();
    assert_eq!(started["durable_session_id"], sid);
    let fs = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &AgentDescriptor {
            pid: format!("pid-{}", started["session_id"].as_str().unwrap()),
            name: "worker".to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..AgentDescriptor::default()
        },
        format!(
            "/proc/{}/workspace",
            started["session_id"].as_str().unwrap()
        ),
    );
    for (turn, cancelled) in [("cancelled-turn", true), ("approved-turn", false)] {
        send(
            &mailbox,
            json!({"jsonrpc":"2.0","id":turn,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"PARITY_SCENARIO:write_file_allowed"}]}}),
        );
        let (approval, _) = until(&mailbox, |message| {
            message["method"] == "session/request_permission"
        });
        assert!(
            fs.read_to_string("generated/output.txt").is_err(),
            "tool must wait for approval"
        );
        let state = call(
            &kernel,
            "get_session_v1",
            json!({"session_id":started["session_id"]}),
        );
        assert_eq!(state["state"], "awaiting_input");
        if cancelled {
            send(
                &mailbox,
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}}),
            );
        } else {
            let option = approval["params"]["options"]
                .as_array()
                .unwrap()
                .iter()
                .find(|option| option["kind"] == "allow_once")
                .unwrap()["optionId"]
                .clone();
            send(
                &mailbox,
                json!({"jsonrpc":"2.0","id":approval["id"],"result":{"outcome":{"outcome":"selected","optionId":option}}}),
            );
        }
        let (terminal, events) = until(&mailbox, |message| message["id"] == turn);
        assert_eq!(
            terminal["result"]["stopReason"],
            if cancelled { "cancelled" } else { "end_turn" },
            "{terminal}"
        );
        if cancelled {
            assert!(
                fs.read_to_string("generated/output.txt").is_err(),
                "cancelled tool must not run"
            );
        } else {
            assert!(
                events
                    .iter()
                    .any(|event| event["params"]["update"]["status"] == "completed"),
                "approved tool must complete: {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|event| event.to_string().contains("write_file succeeded")),
                "tool output must stream through the mailbox: {events:?}"
            );
        }
    }
    assert_eq!(
        fs.read_to_string("generated/output.txt").unwrap(),
        "created by mock service\n"
    );
    let persisted = fs
        .read_to_string(&format!("/sessions/{sid}/transcript.jsonl"))
        .unwrap();
    assert!(persisted.contains("write_file succeeded"));
    // A peer text message enters the same driver and asks the bound controller.
    let peer_path = a2a::conversation_transcript_path(&a2a::conversation_id("worker", "peer"));
    common::provision_stream_transcript(&kernel, &peer_path);
    let peer_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &AgentDescriptor {
            pid: "pid-peer".to_string(),
            name: "peer".to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..AgentDescriptor::default()
        },
        "/".to_string(),
    ));
    let peer = Arc::new(runtime::mailbox::Mailbox::daemon_absolute(
        peer_fs,
        "peer".into(),
    ));
    peer.sender()("worker", "PARITY_SCENARIO:write_file_denied").unwrap();
    let (approval, events) = until(&mailbox, |message| {
        message["method"] == "session/request_permission"
    });
    assert!(events
        .iter()
        .any(|event| event["params"]["update"]["sessionUpdate"] == "user_message_chunk"));
    let deny = approval["params"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["kind"] == "reject_once")
        .unwrap()["optionId"]
        .clone();
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":approval["id"],"result":{"outcome":{"outcome":"selected","optionId":deny}}}),
    );
    let (_, events) = until(&mailbox, |message| {
        message
            .to_string()
            .contains("write_file denied as expected")
    });
    assert!(events
        .iter()
        .any(|event| event["params"]["update"]["status"] == "failed"));
    assert!(fs.read_to_string("generated/denied.txt").is_err());
    let reply = common::wait_for_agent_reply(
        &kernel,
        &peer_path,
        &OperationContext::new("test-owner", "root", false, Some("peer"), false),
        "worker",
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(reply["kind"], "auto_reply");
    assert!(reply["body"]
        .as_str()
        .unwrap()
        .contains("write_file denied as expected"));
    // Reverse questions and cancellation travel on the same channel as permissions.
    for (turn, cancelled) in [("cancel-question", true), ("answer-question", false)] {
        send(
            &mailbox,
            json!({"jsonrpc":"2.0","id":turn,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"PARITY_SCENARIO:ask_user_question_roundtrip"}]}}),
        );
        // Prompt mode also approves the question tool before it asks the user.
        let (first, _) = until(&mailbox, |m| {
            m["method"] == "session/request_permission" || m["method"] == "_scode/ask_user_question"
        });
        let question = if first["method"] == "session/request_permission" {
            let option = first["params"]["options"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["kind"] == "allow_once")
                .unwrap()["optionId"]
                .clone();
            send(
                &mailbox,
                json!({"jsonrpc":"2.0","id":first["id"],"result":{"outcome":{"outcome":"selected","optionId":option}}}),
            );
            until(&mailbox, |m| m["method"] == "_scode/ask_user_question").0
        } else {
            first
        };
        assert_eq!(question["params"]["sessionId"], sid);
        assert_eq!(question["params"]["questions"][0]["id"], "q1");
        if cancelled {
            send(
                &mailbox,
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}}),
            );
        } else {
            send(
                &mailbox,
                json!({"jsonrpc":"2.0","id":question["id"],"result":{"answers":[{"id":"q1","value":"blue","label":"Blue"}]}}),
            );
        }
        let (terminal, events) = until(&mailbox, |m| m["id"] == turn);
        assert_eq!(
            terminal["result"]["stopReason"],
            if cancelled { "cancelled" } else { "end_turn" }
        );
        if !cancelled {
            assert!(
                events.iter().any(|e| e.to_string().contains("blue")),
                "{events:?}"
            );
        }
    }
    // Waiting for a reverse question must not block a sibling tool's updates.
    // This exercises the same concurrent reply handling as the stdio ACP driver.
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"mode","method":"session/setPermissionMode",
        "params":{"sessionId":sid,"permissionMode":"danger-full-access"}}),
    );
    let (mode, _) = until(&mailbox, |m| m["id"] == "mode");
    assert!(mode.get("error").is_none(), "{mode}");
    fs.write("parallel-read.txt", b"MAILBOX_PARALLEL_READ_RESULT")
        .unwrap();
    let calls = json!([
        {"id":"question", "name":"AskUserQuestion", "input":{"question":"MAILBOX_PARALLEL_QUESTION"}},
        {"id":"read", "name":"Read", "input":{"path":"parallel-read.txt"}}
    ]);
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"parallel","method":"session/prompt",
        "params":{"sessionId":sid,"prompt":[{"type":"text","text":format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")}]}}),
    );
    let (question, events) = until(&mailbox, |m| m["method"] == "_scode/ask_user_question");
    if !events
        .iter()
        .any(|m| m.to_string().contains("MAILBOX_PARALLEL_READ_RESULT"))
    {
        until(&mailbox, |m| {
            m["method"] == "session/update"
                && m.to_string().contains("MAILBOX_PARALLEL_READ_RESULT")
        });
    }
    let state = call(
        &kernel,
        "get_session_v1",
        json!({"session_id":started["session_id"]}),
    );
    assert_eq!(
        state["state"], "awaiting_input",
        "question is still unanswered"
    );
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":question["id"],
        "result":{"answers":[{"id":"q1","value":"ack","label":"ack"}]}}),
    );
    let (terminal, _) = until(&mailbox, |m| m["id"] == "parallel");
    assert_eq!(terminal["result"]["stopReason"], "end_turn", "{terminal}");

    // A spawned worker retains Prompt mode and uses this controller for its
    // own file approval. Exercise a denial before an approval, using fresh
    // file contents and the actual child request rather than its canned reply.
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"child-mode","method":"session/setPermissionMode",
        "params":{"sessionId":sid,"permissionMode":"prompt"}}),
    );
    let (mode, _) = until(&mailbox, |m| m["id"] == "child-mode");
    assert!(mode.get("error").is_none(), "{mode}");
    let child_path = mock_anthropic_service::SUBAGENT_CHILD_READ_PATH;
    for (turn, denied) in [("child-denied", true), ("child-approved", false)] {
        let nonce = format!(
            "CHILD_PERMISSION_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs.write(child_path, nonce.as_bytes()).unwrap();
        let first_request = rt.block_on(service.captured_requests()).len();
        send(
            &mailbox,
            json!({"jsonrpc":"2.0","id":turn,"method":"session/prompt",
            "params":{"sessionId":sid,"prompt":[{"type":"text","text":
            "Delegate reading the child's notes. PARITY_SCENARIO:subagent_events_sync"}]}}),
        );
        let mut child_approvals = 0;
        loop {
            let (message, _) = until(&mailbox, |message| {
                message["method"] == "session/request_permission" || message["id"] == turn
            });
            if message["id"] == turn {
                assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
                break;
            }
            let input: Value =
                serde_json::from_str(message["params"]["toolCall"]["rawInput"].as_str().unwrap())
                    .unwrap();
            let child_read = input["path"] == child_path;
            assert!(
                child_read || input["prompt"].is_string(),
                "unexpected approval: {message}"
            );
            if child_read {
                child_approvals += 1;
                let state = call(
                    &kernel,
                    "get_session_v1",
                    json!({"session_id":started["session_id"]}),
                );
                assert_eq!(state["state"], "awaiting_input", "{state}");
                let requests = rt.block_on(service.captured_requests());
                assert!(
                    requests[first_request..]
                        .iter()
                        .all(|request| !request.raw_body.contains(&nonce)),
                    "the child read its file before approval"
                );
            }
            send(
                &mailbox,
                json!({"jsonrpc":"2.0","id":message["id"],
                "result":{"outcome":{"outcome":"selected","optionId":
                if child_read && denied { "reject_once" } else { "allow_once" }}}}),
            );
        }
        assert_eq!(
            child_approvals, 1,
            "child approval never reached the controller; turn={turn}"
        );
        let requests = rt.block_on(service.captured_requests());
        let results: Vec<Value> = requests[first_request..]
            .iter()
            .map(|request| serde_json::from_str::<Value>(&request.raw_body).unwrap())
            .filter(|request| {
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["role"] == "user"
                            && message["content"].as_array().unwrap().iter().any(|block| {
                                block["type"] == "text"
                                    && block["text"]
                                        .as_str()
                                        .unwrap()
                                        .contains("PARITY_SCENARIO:subagent_tool_child")
                            })
                    })
            })
            .flat_map(|request| request["messages"].as_array().unwrap().clone())
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == "tool_result")
            .collect();
        assert!(
            !results.is_empty(),
            "no child tool result reached the model"
        );
        assert!(
            results
                .iter()
                .all(|result| result["is_error"].as_bool().unwrap_or(false) == denied),
            "child approval was not honored: {results:?}"
        );
        assert_eq!(
            results
                .iter()
                .any(|result| result.to_string().contains(&nonce)),
            !denied,
            "child result did not reflect the actual approval and fresh file"
        );
    }

    mailbox
        .send(SessionPayload::Closed {
            reason: "controller disconnected".into(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = call(
            &kernel,
            "get_session_v1",
            json!({"session_id":started["session_id"]}),
        );
        if state["state"] == "terminated" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "channel closure must terminate the process: {state}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
