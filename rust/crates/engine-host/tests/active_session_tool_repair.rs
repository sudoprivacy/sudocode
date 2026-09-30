use std::collections::BTreeMap;

use engine_core::EngineDelegate;
use engine_host::{SessionEngine, SessionLifecycle};
use runtime::{
    ContentBlock, ConversationMessage, PermissionPromptDecision, PermissionPrompter,
    PermissionRequest, RuntimeObserver,
};
use serde_json::Value;

struct Allow;

impl PermissionPrompter for Allow {
    fn decide(&mut self, _: &PermissionRequest) -> PermissionPromptDecision {
        PermissionPromptDecision::Allow
    }
}

struct Observe;
impl RuntimeObserver for Observe {}

#[test]
fn same_process_turn_repairs_orphan_before_model_request() {
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    let mock = rt
        .block_on(mock_anthropic_service::MockAnthropicService::spawn())
        .expect("mock provider");
    let workspace = tempfile::tempdir().expect("workspace");
    let config_home = tempfile::tempdir().expect("config home");
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config_home.path());
    let config = serde_json::json!({
        "auth_modes": { "api-key": { "anthropic": {
            "baseUrl": mock.base_url(), "apiKey": "test-key"
        } } },
        "models": { "claude-sonnet": {
            "alias": "claude-sonnet", "name": "Test Sonnet", "input": ["text"],
            "providers": { "api-key": { "provider": "anthropic", "model": "claude-sonnet-4-6" } }
        } }
    });
    std::fs::write(config_home.path().join("sudocode.json"), config.to_string())
        .expect("config written");
    let engine = SessionEngine::build(
        workspace.path(),
        &BTreeMap::new(),
        Default::default(),
        runtime::memory::MemoryMode::default(),
        Default::default(),
        "claude-sonnet".into(),
        Some("claude-sonnet".into()),
        None,
        None,
        None,
        None,
    )
    .expect("session engine");
    let path = engine.session_handle().path;
    engine.with_session_mut(&mut |session| {
        session
            .push_user_text("original request")
            .expect("user append");
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: "interrupted-call".into(),
                    name: "PowerShell".into(),
                    input: "{}".into(),
                    thought_signature: None,
                },
            ]))
            .expect("orphan append");
        session
            .push_user_text("first unanswered follow-up")
            .expect("follow-up append");
    });

    for _ in 0..2 {
        let complete = engine
            .run_turn(
                vec![ContentBlock::Text {
                    text: "PARITY_SCENARIO:streaming_text".into(),
                }],
                &mut Observe,
                &mut Allow,
            )
            .expect("turn resumes without invalid tool history");
        assert!(!complete.cancelled);
    }
    let snapshot = engine.session_snapshot();
    let ids = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "interrupted-call" => {
                Some(tool_use_id)
            }
            _ => None,
        })
        .count();
    assert_eq!(ids, 1, "repeat prompts do not duplicate repairs");
    let stored = std::fs::read_to_string(path).expect("repaired history persisted");
    let records: Vec<Value> = stored
        .lines()
        .map(|line| serde_json::from_str(line).expect("jsonl record"))
        .collect();
    let call_index = records
        .iter()
        .position(|record| {
            record["message"]["blocks"]
                .as_array()
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["type"] == "tool_use" && block["id"] == "interrupted-call"
                    })
                })
        })
        .expect("call persisted");
    assert!(
        records[call_index + 1]["message"]["blocks"]
            .as_array()
            .expect("result blocks")
            .iter()
            .any(|block| block["type"] == "tool_result"
                && block["tool_use_id"] == "interrupted-call"),
        "repair persisted before the follow-up user message"
    );
    let persisted = runtime::Session::load_from_path(engine.session_handle().path)
        .expect("repaired history reloads");
    assert_eq!(persisted.messages.len(), snapshot.messages.len());

    let requests = rt.block_on(mock.captured_requests());
    let mut checked = 0;
    for request in requests {
        let body: Value = serde_json::from_str(&request.raw_body).expect("request JSON");
        let Some(messages) = body["messages"].as_array() else {
            continue;
        };
        for (index, message) in messages.iter().enumerate() {
            let calls = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_use" && block["id"] == "interrupted-call")
                .count();
            if calls == 0 {
                continue;
            }
            checked += 1;
            let next = messages.get(index + 1).expect("tool result message");
            assert_eq!(next["role"], "user");
            assert!(
                next["content"]
                    .as_array()
                    .expect("content array")
                    .iter()
                    .any(|block| block["type"] == "tool_result"
                        && block["tool_use_id"] == "interrupted-call"),
                "model sees matching result immediately after tool call: {body}"
            );
        }
    }
    assert!(checked >= 2, "resumed turns must send the repaired history");
}
