//! Discovered send addresses and durable peer delivery through a real PTY.
mod common;

use std::time::Duration;

use common::TestEnv;
use mock_anthropic_service::UNIFIED_SEND_RECIPIENT;
use runtime::mailbox::{local_pair_root_in, Mailbox};
use serde_json::{json, Value};

const BODY: &str = "hello from the discovered agent address";

#[test]
fn discovered_agent_list_address_delivers_to_the_peer() {
    let env = TestEnv::new("agent-directory-address");
    let peer = Mailbox::workspace_local(
        &local_pair_root_in(env.config_home()),
        UNIFIED_SEND_RECIPIENT.into(),
    );
    peer.ensure_presence().unwrap();
    let mut cli = env.spawn(&["--permission-mode", "danger-full-access"]);
    cli.resize(50, 120).unwrap();
    common::expect_input_line_cleared(&cli, env.timeout(), "directory ready");
    if env.is_mock() {
        // The existing batch fixture drives real tools. Copy the address from
        // the CLI's actual result before requesting delivery in the next turn.
        complete_turn(
            &mut cli,
            &format!(
                "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
                json!([{"id": "directory-list", "name": "agent_list", "input": {}}]),
            ),
        );
        let result = captured_tool_result(&env, "directory-list");
        let listing: Value = serde_json::from_str(&result).unwrap();
        let peer_row = listing["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == "peer" && row["name"] == UNIFIED_SEND_RECIPIENT)
            .expect("discovery must return the fixture peer");
        let address = peer_row["send_to"].as_str().unwrap();
        assert_eq!(address, UNIFIED_SEND_RECIPIENT);
        complete_turn(
            &mut cli,
            &format!(
                "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
                json!([{"id": "directory-send", "name": "send", "input": {
                    "to": address, "message": BODY, "summary": "directory address roundtrip",
                }}]),
            ),
        );
        captured_tool_result(&env, "directory-send");
    } else {
        // A real model performs discovery and copies the result without a
        // fixture supplying either the address or the tool calls.
        complete_turn(
            &mut cli,
            &format!("Discover the reachable Agents using agent_list. Copy the send_to address returned for {UNIFIED_SEND_RECIPIENT} and use send to send it exactly this message: {BODY}. Use summary: directory address roundtrip. Do not use a shell or create files."),
        );
    }
    assert_cards(&cli, env.timeout());
    let conversations = peer.list_conversations().unwrap();
    if conversations.is_empty() && env.is_mock() {
        for body in env.captured_message_bodies() {
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            eprintln!("Model messages: {}", body["messages"]);
        }
    }
    assert_eq!(
        conversations.len(),
        1,
        "send must address one discovered peer"
    );
    let (messages, _) = peer.poll_conversation(&conversations[0], 0, 0).unwrap();
    assert!(messages.iter().any(|message| message.body == BODY));
    assert!(messages
        .iter()
        .all(|message| message.to == UNIFIED_SEND_RECIPIENT));
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let path = assert_saved_send(&env);
    let mut resumed = env.spawn(&[
        "--resume",
        path.to_str().unwrap(),
        "--permission-mode",
        "read-only",
    ]);
    resumed.resize(70, 120).unwrap();
    common::expect_input_line_cleared(&resumed, env.timeout(), "restored directory ready");
    assert_cards(&resumed, env.timeout());
    resumed.send("/exit").unwrap();
    common::expect_input_line(&resumed, "/exit", env.timeout(), "resumed exit typed");
    resumed.send("\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);
}

fn assert_saved_send(env: &TestEnv) -> std::path::PathBuf {
    let path = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        .expect("the actual conversation was saved");
    let saved = runtime::Session::load_from_path(&path).unwrap();
    let raw_result = saved
        .messages()
        .iter()
        .flat_map(|message| &message.blocks)
        .find_map(|block| match block {
            runtime::ContentBlock::ToolResult {
                tool_name,
                output,
                is_error: false,
                ..
            } if tool_name == "send" => Some(output.as_str()),
            _ => None,
        })
        .expect("the live model must choose send, with a successful real result");
    let result: Value = serde_json::from_str(raw_result).unwrap();
    assert_eq!(
        result["routing"]["content"], BODY,
        "presentation never clips model context"
    );
    assert!(
        result["routing"]["mailbox_path"].is_string(),
        "retain the full source result"
    );
    if env.is_mock() {
        assert_eq!(
            raw_result,
            captured_tool_result(env, "directory-send"),
            "the provider and saved transcript retain the same result bytes"
        );
    }
    path
}

fn assert_cards(cli: &pty_expect::PtySession, timeout: Duration) {
    let screen = common::expect_screen_settled(
        cli,
        |screen| screen.contains("AgentList") && screen.contains("Written to inbox"),
        timeout,
        "discovery and delivery cards",
    );
    for expected in [
        "peer  ",
        UNIFIED_SEND_RECIPIENT,
        "Send →",
        "Summary: directory address roundtrip",
        BODY,
    ] {
        assert!(screen.contains(expected), "missing {expected}: {screen}");
    }
    assert!(
        !screen.contains("routing:"),
        "nested receipt JSON should not be rendered: {screen}"
    );
    assert!(
        !screen.contains("mailbox_path:"),
        "physical paths should stay in the source result"
    );
}

fn complete_turn(cli: &mut pty_expect::PtySession, prompt: &str) {
    let before = common::turn_status_marker(cli);
    cli.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_input_line(
        cli,
        &prompt[..prompt.len().min(60)],
        common::DEFAULT_TIMEOUT,
        "directory input",
    );
    cli.send("\r").unwrap();
    common::expect_turn_complete_after(cli, &before, Duration::from_secs(90), "discovery and send");
}

fn captured_tool_result(env: &TestEnv, call_id: &str) -> String {
    let bodies = env.captured_message_bodies();
    let request: Value = serde_json::from_str(bodies.last().unwrap()).unwrap();
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == call_id)
        .expect("the model must receive the real tool result");
    assert_ne!(result["is_error"], true, "tool failed: {result}");
    result["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect()
}
