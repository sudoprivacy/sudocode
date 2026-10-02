#![cfg(feature = "mailbox")]

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;

use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
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
        "models":{"scripted":{"alias":"scripted","name":"Scripted", "input":["text"],"providers":{"api-key":{"provider":"anthropic","model":"claude-sonnet-4-6"}}}}
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
    let fs = KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        "worker",
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
    let peer_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        "peer",
        "/",
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
