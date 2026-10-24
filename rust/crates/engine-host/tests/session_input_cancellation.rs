//! Cancel real input requests through the shared engine before reading status.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::{EngineCommand, EngineEvent, EngineHandle, EngineSession};
use engine_host::SessionEngine;
use runtime::{
    memory::MemoryMode, ContentBlock, PermissionMode, SystemPrompt, SystemPromptOverrides,
};
use serde_json::{json, Value};

fn receive(handle: &EngineHandle, deadline: Instant) -> EngineEvent {
    handle
        .events
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .expect("the production engine must finish the connected input journey")
}

fn prompt(handle: &EngineHandle, calls: &Value) {
    handle
        .commands
        .send(EngineCommand::Prompt {
            blocks: vec![ContentBlock::Text {
                text: format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}"),
            }],
            context: None,
        })
        .unwrap();
}

fn cancel_input(handle: &EngineHandle, request_id: u64, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    // Both commands use the public seam in FIFO order. Status must stop
    // asking for input as soon as Cancel retires the request, even if the
    // real model/tool turn has not published its terminal event yet.
    handle.commands.send(EngineCommand::Cancel).unwrap();
    handle
        .commands
        .send(EngineCommand::RuntimeStatus { request_id: 1001 })
        .unwrap();
    let mut completed = false;
    loop {
        match receive(handle, deadline) {
            EngineEvent::CommandResult { id: 1001, result } => {
                let status = result.unwrap();
                assert!(
                    matches!(
                        status["foreground"]["state"].as_str(),
                        Some("running" | "idle")
                    ),
                    "cancelled {reason} request still owns input: {status}"
                );
                assert!(status["foreground"].get("waiting_for").is_none());
                break;
            }
            EngineEvent::TurnComplete(turn) => {
                assert!(turn.cancelled);
                completed = true;
            }
            EngineEvent::TurnFailed { message, .. } => panic!("{message}"),
            _ => {}
        }
    }
    while !completed {
        if let EngineEvent::TurnComplete(turn) = receive(handle, deadline) {
            assert!(turn.cancelled);
            completed = true;
        }
    }

    // Late replies must neither revive the old input nor affect a new turn.
    handle
        .commands
        .send(EngineCommand::QuestionAnswer {
            id: request_id,
            answers: Ok(Vec::new()),
        })
        .unwrap();
    handle
        .commands
        .send(EngineCommand::PermissionAnswer {
            id: request_id,
            decision: runtime::PermissionPromptDecision::Deny {
                reason: "late reply after cancellation".into(),
            },
        })
        .unwrap();
}

#[test]
fn cancelling_permission_and_question_releases_input_before_the_turn_drains() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let provider = rt
        .block_on(mock_anthropic_service::MockAnthropicService::spawn())
        .unwrap();
    let config = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", config.path());
    std::env::set_var("USERPROFILE", config.path());
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config.path());
    std::fs::write(
        config.path().join("sudocode.json"),
        json!({
            "auth_modes": {"api-key": {"anthropic": {
                "baseUrl": provider.base_url(), "apiKey": "isolated-input-cancellation"
            }}},
            "models": {"scripted": {"alias": "scripted", "name": "Scripted",
                "input": ["text"], "providers": {"api-key": {
                    "provider": "anthropic", "model": "claude-sonnet-4-6"
                }}}
            }
        })
        .to_string(),
    )
    .unwrap();

    for reason in ["permission", "question"] {
        let workspace = tempfile::tempdir().unwrap();
        let host = SessionEngine::build(
            workspace.path(),
            &BTreeMap::default(),
            SystemPromptOverrides::default(),
            MemoryMode::default(),
            SystemPrompt::default(),
            "scripted".into(),
            Some("scripted".into()),
            None,
            Some(if reason == "permission" {
                PermissionMode::Prompt
            } else {
                PermissionMode::DangerFullAccess
            }),
            None,
            None,
        )
        .unwrap();
        let handle = EngineSession::spawn(Arc::new(host));
        prompt(
            &handle,
            &if reason == "permission" {
                json!([{"id":"cancelled-write", "name":"Write", "input":{
                    "path":"cancelled.txt", "content":"must not be written"
                }}])
            } else {
                json!([{"id":"cancelled-question", "name":"AskUserQuestion",
                    "input":{"question":"Which value should this fresh session use?"}}])
            },
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let request_id = loop {
            match receive(&handle, deadline) {
                EngineEvent::PermissionRequest { id, .. } if reason == "permission" => break id,
                EngineEvent::QuestionRequest { id, .. } if reason == "question" => break id,
                EngineEvent::TurnFailed { message, .. } => panic!("{message}"),
                _ => {}
            }
        };

        cancel_input(&handle, request_id, reason);
        assert!(!workspace.path().join("cancelled.txt").exists());
        handle
            .commands
            .send(EngineCommand::SetPermissionMode {
                mode: PermissionMode::DangerFullAccess,
            })
            .unwrap();
        let proof = format!("fresh-{reason}-{}", workspace.path().display());
        prompt(
            &handle,
            &json!([{"id":"fresh-write", "name":"Write", "input":{
                "path":"after-cancel.txt", "content":proof
            }}]),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match receive(&handle, deadline) {
                EngineEvent::TurnComplete(turn) => {
                    assert!(!turn.cancelled);
                    break;
                }
                EngineEvent::TurnFailed { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("after-cancel.txt")).unwrap(),
            proof
        );
        handle.commands.send(EngineCommand::Close).unwrap();
        assert!(handle.shutdown(Duration::from_secs(15)));
    }
}
