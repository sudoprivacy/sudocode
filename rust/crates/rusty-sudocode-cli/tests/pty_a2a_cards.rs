//! No-API PTY acceptance for exact historical shapes, widths and both REPLs.
//! Live tool selection and durable delivery are covered by `pty_agent_directory`.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};

fn add(session: &mut Session, id: &str, name: &str, input: &Value, output: &Value, error: bool) {
    session
        .push_user_text("Review the mailbox result.")
        .unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: input.to_string(),
                thought_signature: None,
            },
        ]))
        .unwrap();
    session
        .push_message(ConversationMessage::tool_result(
            id,
            name,
            output
                .as_str()
                .map_or_else(|| output.to_string(), str::to_string),
            error,
        ))
        .unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: format!("Recorded {id}."),
        }]))
        .unwrap();
}

fn replay(env: &TestEnv, source: &Session, marker: &str, check: impl Fn(&str)) {
    // Historical JSON is still supported. The real delivery journey also
    // exercises the current append log, so both restoration formats are covered.
    let path = env.workspace_root().join("a2a-history.json");
    std::fs::write(&path, source.to_json().unwrap().render()).unwrap();
    let original = Session::load_from_path(&path).unwrap().messages().to_vec();
    for mode in ["off", "queue"] {
        for width in [52, 120] {
            let mut cli = env.spawn_with_env(
                &[
                    "--resume",
                    path.to_str().unwrap(),
                    "--permission-mode",
                    "read-only",
                ],
                &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)],
            );
            cli.resize(180, width).unwrap();
            let screen = common::expect_screen_settled(
                &cli,
                |screen| screen.contains(marker),
                env.timeout(),
                "replayed A2A cards",
            );
            // Rejoin the common left-frame's wrapped rows when checking exact addresses.
            let copy = screen
                .lines()
                .map(|line| line.trim().strip_prefix("│ ").unwrap_or(line.trim()))
                .collect::<Vec<_>>()
                .join("");
            check(&copy);
            assert!(!screen.contains('�'), "UTF-8 was damaged: {screen}");
            assert!(
                !screen.contains("\"routing\""),
                "raw routing JSON leaked: {screen}"
            );
            common::expect_input_line_cleared(&cli, env.timeout(), "replay ready");
            cli.send("/exit").unwrap();
            common::expect_input_line(&cli, "/exit", env.timeout(), "exit typed");
            cli.send("\r").unwrap();
            assert_eq!(cli.expect_eof().unwrap(), 0);
            let restored = Session::load_from_path(&path).unwrap();
            assert_eq!(
                restored.messages(),
                original,
                "display must preserve full source blocks"
            );
            assert_eq!(
                env.captured_message_count(),
                0,
                "replay never calls the model"
            );
        }
    }
}

#[test]
fn send_replay_keeps_receipts_addresses_errors_and_bounded_previews() {
    let env = TestEnv::new_mock("send-card-history");
    let mut source = Session::new().with_workspace_root(env.workspace_root());
    let address = "reviewer-具体地址-1234567890123456789012345678901234567890";
    let message = "验证完成，保留完整原文。\nSecond preview line.\nThird preview line.\nHIDDEN-TAIL-ONLY-IN-SESSION";
    add(
        &mut source,
        "plain",
        "send",
        &json!({"to":address,"summary":"验证结果摘要","message":message}),
        &json!({"success":true,"message":"Message sent to the inbox", "routing":{
            "target":format!("@{address}"),"summary":"验证结果摘要","content":message,
            "mailbox_path":"HIDDEN-MAILBOX-PATH",
        }}),
        false,
    );
    add(
        &mut source,
        "empty-broadcast",
        "send",
        &json!({"to":"*","summary":"Empty team","message":"Nobody is registered"}),
        &json!({"success":true,"message":"No teammates to broadcast to","recipients":[]}),
        false,
    );
    add(
        &mut source,
        "broadcast",
        "send",
        &json!({"to":"*","summary":"Team note","message":"Review complete"}),
        &json!({"success":true,"recipients":["peer-one","peer-two"]}),
        false,
    );
    add(
        &mut source,
        "structured",
        "send",
        &json!({"to":"worker-42","message":{"type":"plan_approval_response","approve":false,
            "feedback":"Please include the missing validation.","request_id":"plan-42"}}),
        &json!({"success":true,"request_id":"plan-42"}),
        false,
    );
    add(
        &mut source,
        "failed",
        "send",
        &json!({"to":address,"summary":"Failure","message":"retry"}),
        &json!("Mailbox enumeration failed: permission denied"),
        true,
    );
    add(
        &mut source,
        "unfamiliar",
        "send",
        &json!({"to":"future-peer","message":"future"}),
        &json!({"future_receipt":"UNFAMILIAR-RESULT-RETAINED"}),
        false,
    );
    replay(&env, &source, "Recorded unfamiliar.", |screen| {
        for expected in [
            "Send →",
            address,
            "Summary: 验证结果摘要",
            "Written to inbox",
            "Third preview line.",
            "full output preserved in session",
            "No recipients · no inboxes written",
            "Written to 2 inboxes: peer-one, peer-two",
            "plan_approval_response · rejected",
            "Request: plan-42",
            "permission denied",
            "UNFAMILIAR-RESULT-RETAINED",
        ] {
            assert!(
                common::screen_contains(screen, expected),
                "missing {expected}: {screen}"
            );
        }
        assert!(!screen.contains("HIDDEN-TAIL-ONLY-IN-SESSION"));
        assert!(!screen.contains("HIDDEN-MAILBOX-PATH"));
    });
}

#[test]
fn agent_list_replay_distinguishes_unknown_liveness_empty_and_truncated_lists() {
    let env = TestEnv::new_mock("agent-list-card-history");
    let mut source = Session::new().with_workspace_root(env.workspace_root());
    add(
        &mut source,
        "directory",
        "agent_list",
        &json!({}),
        &json!({"count":3,"agents":[
            {"kind":"peer","name":"peer-未知状态","send_to":"peer-未知状态"},
            {"kind":"subagent","name":"reviewer","send_to":"worker-exact-address-42",
                "active":true,"role":"review","task_id":"task-42"},
            {"kind":"subagent","name":"writer","send_to":"worker-43","active":false},
        ]}),
        false,
    );
    add(
        &mut source,
        "empty",
        "agent_list",
        &json!({"active_only":true}),
        &json!({"count":0,"agents":[]}),
        false,
    );
    let agents: Vec<_> = (0..18)
        .map(|n| {
            json!({"kind":"peer","name":format!("listed-{n:02}"),
        "send_to":format!("listed-{n:02}")})
        })
        .collect();
    add(
        &mut source,
        "many",
        "agent_list",
        &json!({}),
        &json!({"count":18,"agents":agents}),
        false,
    );
    add(
        &mut source,
        "list-error",
        "agent_list",
        &json!({}),
        &json!("Agent enumeration unavailable"),
        true,
    );
    replay(&env, &source, "Recorded list-error.", |screen| {
        for expected in [
            "AgentList · 3 agents",
            "peer  peer-未知状态",
            "subagent  reviewer",
            "send_to=worker-exact-address-42 · active",
            "role=review · task_id=task-42",
            "send_to=worker-43 · inactive",
            "active only · 0 agents",
            "No agents found",
            "18 agents",
            "listed-14",
            "+3 agents · full list preserved in session",
            "Agent enumeration unavailable",
        ] {
            assert!(
                common::screen_contains(screen, expected),
                "missing {expected}: {screen}"
            );
        }
        assert!(!screen.contains("listed-15"));
        assert!(!screen.contains("peer-未知状态 · active"));
        assert!(!screen.contains("peer-未知状态 · inactive"));
        assert!(!screen.contains("online"));
    });
}
