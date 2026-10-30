//! Resume tool histories through the shared REPL/ACP engine and inspect the wire.
mod common;

use std::collections::HashSet;
use std::fs;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, MessageRole, Session};
use serde_json::Value;

fn call(id: &str) -> ConversationMessage {
    ConversationMessage::assistant(vec![ContentBlock::ToolUse {
        id: id.into(),
        name: "read_file".into(),
        input: r#"{"path":"fixture.txt"}"#.into(),
        thought_signature: None,
    }])
}

fn assert_pairs(body: &Value) {
    let mut pending = HashSet::new();
    for message in body["messages"].as_array().expect("messages") {
        if message["role"] == "assistant" {
            assert!(pending.is_empty(), "unanswered calls: {pending:?}");
            for block in message["content"].as_array().expect("blocks") {
                if block["type"] == "tool_use" {
                    assert!(pending.insert(block["id"].as_str().unwrap().to_owned()));
                }
            }
        } else {
            for block in message["content"].as_array().expect("blocks") {
                if block["type"] == "tool_result" {
                    let id = block["tool_use_id"].as_str().unwrap();
                    assert!(pending.remove(id), "unexpected tool_use_id {id}");
                }
            }
        }
    }
    assert!(pending.is_empty(), "unanswered final calls");
}

#[test]
fn resume_preserves_results_without_duplicate_or_unmatched_tool_ids() {
    let env = TestEnv::new_mock("tool-history-recovery");
    let root = env.workspace_root();
    let path = root.join("fixture.jsonl");
    let dump = root.join("requests");
    fs::create_dir_all(&dump).unwrap();
    let (healthy_prefix, source_results, created_file) = seed_legacy_tool_history(root, &path);
    let target = "claude-opus-4-6";

    // A second process must preserve the repair and all result content.
    for turn in 0..2 {
        let prompt = env.prompt(
            "Calculate 2+2 and reply with 'The answer is N', replacing N with the result.",
            "single_turn_text",
        );
        let mut cli = env.spawn_with_env(
            &[
                "--permission-mode",
                "read-only",
                "--resume",
                path.to_str().unwrap(),
            ],
            &[("SUDOCODE_DUMP_REQUESTS", dump.to_str().unwrap())],
        );
        common::expect_input_line_cleared(&cli, env.timeout(), "resume ready");
        if turn == 1 {
            let command = format!("/model {target}");
            cli.send(&command).expect("type model switch");
            common::expect_input_line(&cli, &command, env.timeout(), "model command typed");
            cli.send("\r").expect("submit model switch");
            cli.expect("Model updated").expect("model switch accepted");
            common::expect_input_line_cleared(&cli, env.timeout(), "input after switch");
        }
        let marker = common::turn_status_marker(&cli);
        cli.send(&prompt).expect("type resumed prompt");
        common::expect_input_line(&cli, "Calculate 2+2", env.timeout(), "prompt typed");
        cli.send("\r").expect("submit prompt");
        cli.expect("The answer is 4").expect("continued response");
        common::expect_turn_complete_after(&cli, &marker, env.timeout(), "turn finished");
        if turn == 1 {
            cli.send("/undo").expect("type undo");
            common::expect_input_line(&cli, "/undo", env.timeout(), "undo typed");
            cli.send("\r").expect("submit undo");
            cli.expect("Deleted")
                .expect("historical write can be undone");
            assert!(
                !created_file.exists(),
                "undo must preserve its source record"
            );
        }
        cli.send("/exit").expect("type exit");
        common::expect_input_line(&cli, "/exit", env.timeout(), "exit typed");
        cli.send("\r").expect("submit exit");
        assert_eq!(cli.expect_eof().expect("process exits"), 0);
        let restored = Session::load_from_path(&path).unwrap();
        assert_healthy_tool_history(&restored, &healthy_prefix);
        for source in &source_results {
            assert!(
                restored
                    .messages()
                    .iter()
                    .any(|message| message.blocks.contains(source)),
                "model projection must preserve structured source records: {source:?}"
            );
        }
        assert!(restored
            .messages()
            .iter()
            .any(|message| message.role == MessageRole::Assistant
                && message.blocks.iter().any(
                    |block| matches!(block, ContentBlock::Text { text } if text.contains('4'))
                )));
    }
    assert_resumed_tool_requests(&dump, target);
    let persisted = fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("ORPHAN-RESULT-KEPT"));
    assert!(persisted.contains("DUPLICATE-RESULT-KEPT"));
}

fn assert_healthy_tool_history(restored: &Session, healthy_prefix: &[ConversationMessage]) {
    let content = |messages: &[ConversationMessage]| {
        messages
            .iter()
            .cloned()
            .map(|mut message| {
                message.history = None;
                message
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        content(&restored.messages()[..healthy_prefix.len()]),
        content(healthy_prefix),
        "repair must not rewrite healthy history, even across a model switch"
    );
}

fn healthy_tool_history(root: &std::path::Path) -> Session {
    let mut session = Session::new().with_workspace_root(root);
    session.push_user_text("Read fixture.txt").unwrap();
    let mut parallel = call("valid-user-result");
    parallel.blocks.extend(call("valid-tool-result").blocks);
    session.push_message(parallel).unwrap();
    let mut result = ConversationMessage::tool_result(
        "valid-user-result",
        "read_file",
        "VALID-USER-RESULT",
        false,
    );
    result.role = MessageRole::User;
    session.push_message(result).unwrap();
    session
        .push_message(ConversationMessage::tool_result(
            "valid-tool-result",
            "read_file",
            "VALID-TOOL-RESULT",
            false,
        ))
        .unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "Read complete.".into(),
        }]))
        .unwrap();
    session
}

fn seed_legacy_tool_history(
    root: &std::path::Path,
    path: &std::path::Path,
) -> (
    Vec<ConversationMessage>,
    Vec<ContentBlock>,
    std::path::PathBuf,
) {
    let session = healthy_tool_history(root);
    let healthy_prefix = session.messages().to_vec();
    let mut messages = session.messages().to_vec();
    messages.push(ConversationMessage::tool_result(
        "missing-call",
        "read_file",
        "ORPHAN-RESULT-KEPT",
        false,
    ));
    messages.push(call("late-result"));
    messages.push(ConversationMessage::user_text(
        "The previous turn was interrupted.",
    ));
    messages.push(ConversationMessage::tool_result(
        "late-result",
        "read_file",
        "LATE-RESULT-KEPT",
        false,
    ));
    messages.push(call("duplicate-result"));
    messages.push(ConversationMessage::tool_result(
        "duplicate-result",
        "read_file",
        "FIRST-RESULT-KEPT",
        false,
    ));
    messages.push(ConversationMessage::tool_result(
        "duplicate-result",
        "read_file",
        "DUPLICATE-RESULT-KEPT",
        true,
    ));
    let created_file = root.join("undo.txt");
    fs::write(&created_file, "created before resume\n").unwrap();
    messages.push(ConversationMessage::tool_result(
        "historical-write",
        "write_file",
        serde_json::json!({
            "type": "create",
            "filePath": created_file,
            "content": "created before resume\n",
            "originalFile": null,
            "structuredPatch": [],
        })
        .to_string(),
        false,
    ));
    let source_results: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .cloned()
        .collect();
    // Deliberately malformed legacy history comes from stored bytes, rather
    // than asking the new writer to publish an unmatched tool result.
    let mut legacy: Value = serde_json::from_str(&session.to_json().unwrap().render()).unwrap();
    legacy["messages"] = Value::Array(
        messages
            .iter()
            .map(|message| {
                let mut value: Value = serde_json::from_str(&message.to_json().render()).unwrap();
                value.as_object_mut().unwrap().remove("history");
                value
            })
            .collect(),
    );
    fs::write(path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    (healthy_prefix, source_results, created_file)
}

fn assert_resumed_tool_requests(dump: &std::path::Path, target: &str) {
    let mut checked = 0;
    let mut switched_request = false;
    for entry in fs::read_dir(dump).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_name().to_string_lossy().contains("messages") {
            continue;
        }
        let body: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        switched_request |= body["model"] == target;
        assert_pairs(&body);
        let messages = body["messages"].to_string();
        for marker in [
            "VALID-USER-RESULT",
            "VALID-TOOL-RESULT",
            "ORPHAN-RESULT-KEPT",
            "LATE-RESULT-KEPT",
            "FIRST-RESULT-KEPT",
            "DUPLICATE-RESULT-KEPT",
        ] {
            assert!(messages.contains(marker), "lost result: {marker}");
        }
        checked += 1;
    }
    assert!(checked >= 2, "both continued turns reached the provider");
    assert!(
        switched_request,
        "the selected model received the continued history"
    );
}
