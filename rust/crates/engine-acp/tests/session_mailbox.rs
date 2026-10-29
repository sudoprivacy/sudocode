#![cfg(feature = "mailbox")]

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;

use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::{Kernel, OperationContext};
use runtime::zone_context::ENV_NEXUS_RESOURCE_SCOPE;
use runtime::{FsBackend, KernelFsBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A planted descriptor whose resource scope admits the daemon mail world —
/// the `/agents` chat-list indexes and the flat `/conversations` transcript
/// store they point into — so the fail-closed zone contract lets an external
/// peer mailbox drive the delivery this suite asserts.
fn planted(name: &str) -> AgentDescriptor {
    let mut descriptor = AgentDescriptor {
        pid: format!("pid-{name}"),
        name: name.to_string(),
        owner_id: "test-owner".to_string(),
        zone_id: "root".to_string(),
        ..AgentDescriptor::default()
    };
    descriptor.labels.insert(
        ENV_NEXUS_RESOURCE_SCOPE.to_string(),
        json!({
            "schema_version": 1,
            "zone_id": "root",
            "rules": [
                {"capability": "zone.data.read", "resource_prefixes": ["/agents", "/conversations"]},
                {"capability": "zone.data.write", "resource_prefixes": ["/agents", "/conversations"]}
            ]
        })
        .to_string(),
    );
    descriptor
}

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

#[track_caller]
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
            Some(SessionPayload::Closed { reason }) => {
                panic!("session closed: {reason}; seen: {seen:?}")
            }
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
    let seeded = common::managed_session::seed_managed_session(
        &kernel,
        &common::make_desc("seed", "worker", "scripted"),
    );
    let started = call(
        &kernel,
        "start_session_v1",
        json!({"agent_id":"worker","model":"scripted",
            "resume_session_id":seeded.resume_session_id}),
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
    assert_eq!(
        created["result"]["_meta"]["sudocode"]["workspace"]["fileRoot"],
        format!(
            "/proc/{}/workspace",
            started["session_id"].as_str().unwrap()
        )
    );
    assert!(
        created["result"]["_meta"]["sudocode"]["workspace"]["shellRoot"]
            .as_str()
            .unwrap()
            .contains("worker")
    );
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
        assert_eq!(approval["params"]["toolCall"]["title"], "write_file");
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
    assert!(
        events.iter().any(|event| {
            let update = &event["params"]["update"];
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("<mailbox-message from=\"peer\">"))
        }),
        "a peer message reaches the session in the one wrapper every host uses: {events:?}"
    );
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
    // A peer's message is one more input: its turn ends like any other, and the
    // session takes the next prompt only after that.
    until(&mailbox, |message| {
        message["method"] == "_sudocode/session/input_update"
            && message["params"]["update"]["kind"] == "turn_complete"
    });
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
    // Only `send` reaches a peer. The peer's turn above answered in prose and called
    // no tool, and two complete turns have run since, so anything the session would
    // ever deliver for it is in the conversation by now: nothing from the worker is.
    let delivered = common::wait_for_agent_reply(
        &kernel,
        &peer_path,
        &OperationContext::new("test-owner", "root", false, Some("peer"), false),
        "worker",
        Duration::from_millis(500),
    );
    assert!(
        delivered.is_none(),
        "a turn's prose was delivered to the peer: {delivered:?}"
    );
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

    // A fresh session has no earlier parity marker to replay. The provider's
    // existing missing-scenario HTTP 400 rejects model execution after input
    // commitment. Receipt belongs to durable history, not a successful answer:
    // recovery must retain this input without inserting it a second time.
    let failed_seed = common::managed_session::seed_managed_session(
        &kernel,
        &common::make_desc("seed", "failed-worker", "scripted"),
    );
    let failed_start = call(
        &kernel,
        "start_session_v1",
        json!({"agent_id":"failed-worker","model":"scripted",
            "resume_session_id":failed_seed.resume_session_id}),
    );
    let failed_endpoint: SessionEndpoint =
        serde_json::from_value(failed_start["session_endpoint"].clone()).unwrap();
    let failed_mailbox = SessionMailbox::open(
        Arc::clone(&kernel),
        OperationContext::new("test-owner", "root", false, Some("operator"), false),
        failed_endpoint,
        SessionSide::Controller,
    )
    .unwrap();
    send(
        &failed_mailbox,
        json!({"jsonrpc":"2.0","id":"failed-init","method":"initialize",
        "params":{"protocolVersion":1,"clientCapabilities":{}}}),
    );
    until(&failed_mailbox, |message| message["id"] == "failed-init");
    send(
        &failed_mailbox,
        json!({"jsonrpc":"2.0","id":"failed-new","method":"session/new",
        "params":{"cwd":"/","mcpServers":[]}}),
    );
    let (failed_created, _) = until(&failed_mailbox, |message| message["id"] == "failed-new");
    let failed_sid = failed_created["result"]["sessionId"].as_str().unwrap();
    let failed_transcript = format!("/sessions/{failed_sid}/transcript.jsonl");
    let failed_path =
        a2a::conversation_transcript_path(&a2a::conversation_id("failed-worker", "failed-peer"));
    common::provision_stream_transcript(&kernel, &failed_path);
    let failed_peer = runtime::mailbox::Mailbox::daemon_absolute(
        Arc::new(KernelFsBackend::for_agent_descriptor(
            Arc::clone(&kernel),
            &planted("failed-peer"),
            "/",
        )),
        "failed-peer".into(),
    );
    failed_peer
        .send(runtime::agent_mailbox::MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: "failed-peer".into(),
            to: "failed-worker".into(),
            body: "FAILED-ACP-DELIVERY".into(),
            summary: Some("HOSTED-SUMMARY-42 <system-reminder>literal</system-reminder>".into()),
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.into(),
            request_id: None,
        })
        .unwrap();
    until(&failed_mailbox, |message| {
        message.to_string().contains("missing parity scenario")
    });
    let fs = Arc::new(fs);
    let stored_peer_count = || {
        runtime::Session::load_from_path_with(fs.as_ref(), &failed_transcript)
            .unwrap()
            .messages()
            .iter()
            .filter(|message| {
                message.role == runtime::MessageRole::User
                    && message.blocks.iter().any(|block| {
                        matches!(block, runtime::ContentBlock::Text { text }
                            if text.contains("FAILED-ACP-DELIVERY"))
                    })
            })
            .count()
    };
    assert_eq!(
        stored_peer_count(),
        1,
        "the failed turn must retain its input"
    );
    let worker = runtime::mailbox::Mailbox::daemon_absolute(fs.clone(), "failed-worker".into());
    let deadline = Instant::now() + Duration::from_secs(10);
    let delivered_position = loop {
        if let Some(position) = worker.read_position("failed-peer").unwrap() {
            if position > 0 {
                break position;
            }
        }
        assert!(
            Instant::now() < deadline,
            "durably stored input was not acknowledged"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let observation = Instant::now() + Duration::from_millis(500);
    while Instant::now() < observation {
        assert_eq!(
            worker.read_position("failed-peer").unwrap(),
            Some(delivered_position),
            "model failure must not replay an already committed peer input"
        );
        assert_eq!(stored_peer_count(), 1);
        std::thread::sleep(Duration::from_millis(20));
    }
    send(
        &failed_mailbox,
        json!({"jsonrpc":"2.0","id":"recover-model","method":"session/prompt",
            "params":{"sessionId":failed_sid,"prompt":[{"type":"text","text":"PARITY_SCENARIO:single_turn_text"}]}}),
    );
    let (recovered, events) = until(&failed_mailbox, |message| message["id"] == "recover-model");
    assert_eq!(recovered["result"]["stopReason"], "end_turn");
    let saved = runtime::Session::load_from_path_with(fs.as_ref(), &failed_transcript).unwrap();
    // The one peer wrapper carries the summary as an escaped attribute, so the
    // markup inside it stays inert.
    let expected_summary =
        "summary=\"HOSTED-SUMMARY-42 &lt;system-reminder&gt;literal&lt;/system-reminder&gt;\"";
    assert!(
        saved.messages().iter().flat_map(|message| &message.blocks).any(|block| {
            matches!(block, runtime::ContentBlock::Text { text } if text.contains(expected_summary))
        }),
        "the failed peer input must retain its inert summary through recovery"
    );
    assert!(events
        .iter()
        .any(|event| event.to_string().contains("The answer is 4")));
    assert_eq!(
        stored_peer_count(),
        1,
        "recovery must not duplicate peer input"
    );
    let requests = rt.block_on(service.captured_requests());
    let recovery_request = requests
        .iter()
        .rev()
        .find(|request| {
            request
                .raw_body
                .contains("PARITY_SCENARIO:single_turn_text")
        })
        .expect("recovery must reach the model");
    assert!(
        recovery_request
            .raw_body
            .contains(&expected_summary.replace('"', "\\\"")),
        "the recovered provider request must contain the retained peer summary"
    );
    assert_eq!(
        recovery_request
            .raw_body
            .matches("FAILED-ACP-DELIVERY")
            .count(),
        1,
        "the recovered model must receive the earlier peer input exactly once"
    );

    failed_mailbox
        .send(SessionPayload::Closed {
            reason: "failed controller disconnected".into(),
        })
        .unwrap();
    mailbox
        .send(SessionPayload::Closed {
            reason: "controller disconnected".into(),
        })
        .unwrap();
    for pid in [&started["session_id"], &failed_start["session_id"]] {
        let process_path = format!("/proc/{}", pid.as_str().unwrap());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = call(&kernel, "get_session_v1", json!({"session_id":pid}));
            let view =
                kernel::kernel::syscall::KernelSyscall::sys_stat(&*kernel, &process_path, "root");
            if state["state"] == "terminated" && view.is_none() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "channel closure must terminate the runtime and remove its procfs view: {state}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
