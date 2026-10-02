//! Model switches must update both the API route and the assistant's context.
mod common;

use common::TestEnv;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn transcript(dir: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = transcript(&path) {
                return Some(found);
            }
        } else if path
            .file_name()
            .is_some_and(|name| name == "transcript.jsonl")
        {
            return Some(path);
        }
    }
    None
}

fn messages(env: &TestEnv) -> Vec<Value> {
    let path = transcript(&env.workspace_root().join(".scode")).expect("saved transcript");
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["message"].clone())
        .filter(|message| message.is_object())
        .collect()
}

fn text(message: &Value) -> String {
    message["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn model_context_follows_switches_and_switching_back() {
    let env = TestEnv::new("model-context");
    let target =
        std::env::var("SCODE_LIVE_SWITCH_MODEL").unwrap_or_else(|_| "claude-opus-4-6".to_string());
    let budget = if env.is_live() {
        Duration::from_secs(90)
    } else {
        env.timeout()
    };
    let mut cli = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    cli.set_default_timeout(budget);
    common::expect_input_line_cleared(&cli, budget, "initial input");
    let mut initial = String::new();
    for turn in 0..4 {
        let selected = if turn == 3 { &initial } else { &target };
        if turn == 1 || turn == 3 {
            let command = format!("/model {selected}");
            cli.send(&command).unwrap();
            common::expect_input_line(&cli, &command, budget, "model command");
            cli.send("\r").unwrap();
            cli.expect("Model updated").unwrap_or_else(|error| {
                panic!(
                    "model switch accepted: {error}\n{}",
                    cli.render(|screen| screen.contents())
                );
            });
            common::expect_input_line_cleared(&cli, budget, "input after switch");
        }
        let prompt = env.prompt(
            "你知道你自己是什么模型吗？只输出当前模型名称。",
            "single_turn_text",
        );
        let marker = common::turn_status_marker(&cli);
        cli.send(&prompt).unwrap();
        common::expect_input_line(&cli, "你知道你自己", budget, "question entered");
        cli.send("\r").unwrap();
        common::expect_turn_complete_after(&cli, &marker, budget, "model answered");
        let saved = messages(&env);
        let user = saved.iter().rev().find(|m| m["role"] == "user").unwrap();
        let assistant = saved
            .iter()
            .rev()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        if turn == 0 {
            initial = assistant["model"].as_str().unwrap().to_string();
            assert_ne!(initial, target, "the test must switch to a different model");
        }
        let expected = if turn == 1 || turn == 2 {
            &target
        } else {
            &initial
        };
        let user_text = text(user);
        let has_announcement = user_text.contains("<system-reminder>You are running as ")
            || user_text.contains("<system-reminder>The active model has changed");
        if turn == 2 {
            assert!(
                !has_announcement,
                "unchanged model should not repeat its identity"
            );
        } else {
            assert!(has_announcement && user_text.contains(expected),
                "the current turn must announce {expected}, including after runtime rebuild: {user_text}");
        }
        assert_eq!(assistant["model"], expected.as_str());
        if env.is_mock() {
            let captured = env.captured_message_bodies();
            let request: Value = serde_json::from_str(captured.last().unwrap()).unwrap();
            assert_eq!(request["model"], expected.as_str(), "actual API route");
        } else {
            assert_eq!(
                text(assistant).trim().trim_matches('`'),
                expected,
                "live assistant identity"
            );
        }
    }
    cli.send("/exit").unwrap();
    common::expect_input_line(&cli, "/exit", budget, "exit entered");
    cli.send("\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
}
