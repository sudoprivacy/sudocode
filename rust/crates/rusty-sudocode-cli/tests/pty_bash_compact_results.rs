//! Assert model-visible bash results through the real CLI, including save/resume.
#![cfg(unix)]
mod common;

use common::TestEnv;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn results(request: &Value) -> BTreeMap<String, String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            let text = b["content"].as_str().map_or_else(
                || {
                    b["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|c| c["text"].as_str())
                        .collect::<String>()
                },
                str::to_owned,
            );
            (b["tool_use_id"].as_str().unwrap().to_owned(), text)
        })
        .collect()
}

fn transcript(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()? {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if let Some(found) = transcript(&path) {
                return Some(found);
            }
        } else if path.file_name().unwrap() == "transcript.jsonl" {
            return Some(path);
        }
    }
    None
}

#[test]
fn compact_bash_results_reach_model_and_survive_resume() {
    let env = TestEnv::new("compact-bash-results");
    if !env.is_mock() {
        return;
    }
    let prompt = env.prompt("Exercise shell result statuses", "bash_compact_results");
    let mut cli = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    cli.expect("compact bash results verified")
        .expect("all tool steps complete");
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let requests: Vec<Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    let sent = results(requests.last().unwrap());
    assert_eq!(
        sent.len(),
        7,
        "six shell runs plus reading offloaded output"
    );
    let parsed: Vec<Value> = (0..5)
        .map(|i| serde_json::from_str(&sent[&format!("compact_bash_{i}")]).unwrap())
        .collect();
    assert_eq!(
        parsed[0],
        serde_json::json!({"stdout":"compact stdout", "exit_code":0})
    );
    assert_eq!(parsed[1]["exit_code"], 7);
    assert_eq!(parsed[1]["stderr"], "failure detail");
    assert_eq!(parsed[1]["returnCodeInterpretation"], "exit_code:7");
    assert_eq!(parsed[2]["returnCodeInterpretation"], "timeout");
    assert_eq!(parsed[2]["interrupted"], true);
    assert!(parsed[2].get("exit_code").is_none());
    assert!(parsed[3]["backgroundTaskId"].as_str().is_some());
    assert_eq!(parsed[3]["noOutputExpected"], true);
    assert!(parsed[3].get("exit_code").is_none());
    assert!(parsed[4]["returnCodeInterpretation"]
        .as_str()
        .unwrap()
        .contains("signal"));
    assert!(parsed[4].get("exit_code").is_none());
    for result in &parsed {
        assert!(result.get("sandboxStatus").is_none());
        assert!(result.as_object().unwrap().values().all(|v| !v.is_null()));
    }
    assert!(sent["compact_bash_5"].contains("<persisted-output"));
    assert!(sent["compact_bash_6"].contains("large output line"));
    let path = transcript(&env.workspace_root().join(".scode/sessions")).expect("saved transcript");
    let saved = runtime::Session::load_from_path(&path).unwrap();
    let saved_results: BTreeMap<_, _> = saved
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            runtime::ContentBlock::ToolResult {
                tool_use_id,
                output,
                ..
            } => Some((tool_use_id.clone(), output.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(saved_results, sent, "persist exactly what the model saw");
    let mut resumed = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--resume",
        path.to_str().unwrap(),
    ]);
    resumed.expect("❯").expect("resumed prompt");
    let budget = std::time::Duration::from_secs(15);
    common::expect_input_line_cleared(&resumed, budget, "resume ready");
    let marker = common::turn_status_marker(&resumed);
    let request_count = env.captured_message_count();
    resumed.send(&format!("{prompt}\r")).unwrap();
    resumed
        .expect("compact bash results verified")
        .expect("resume completes");
    common::expect_turn_complete_after(&resumed, &marker, budget, "resumed turn");
    common::expect_input_line_cleared(&resumed, budget, "ready to exit");
    assert!(
        env.captured_message_count() > request_count,
        "resume must actually call the provider"
    );
    resumed.send("/exit\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);
    assert_eq!(
        results(
            &serde_json::from_str::<Value>(env.captured_message_bodies().last().unwrap()).unwrap()
        ),
        sent,
        "resume preserves every result byte-for-byte"
    );
}

#[test]
fn cancelled_bash_persists_a_compact_interruption() {
    let env = TestEnv::new("compact-bash-cancel");
    if !env.is_mock() {
        return;
    }
    let prompt = env.prompt("Run a long command", "bash_interrupt_long_running");
    let mut cli = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    cli.expect("❯").expect("initial prompt");
    cli.send(&format!("{prompt}\r")).unwrap();
    cli.expect("interrupt-start")
        .expect("command has actually started");
    cli.send("\x1b").unwrap();
    cli.expect("(?i)(cancelled|interrupted)")
        .expect("turn cancelled");
    cli.send("/exit\r").unwrap();
    cli.set_default_timeout(std::time::Duration::from_secs(15));
    cli.expect_eof().expect("cancel exits promptly");
    let path = transcript(&env.workspace_root().join(".scode/sessions")).expect("saved transcript");
    let saved = runtime::Session::load_from_path(&path).unwrap();
    let output = saved
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .find_map(|b| match b {
            runtime::ContentBlock::ToolResult {
                tool_name, output, ..
            } if tool_name == "bash" => Some(output),
            _ => None,
        })
        .expect("cancelled tool must have a result");
    let result: Value = serde_json::from_str(output).unwrap();
    assert_eq!(result["interrupted"], true);
    assert_eq!(result["returnCodeInterpretation"], "interrupted");
    assert!(result.get("exit_code").is_none());
    assert!(result.get("sandboxStatus").is_none());
    assert!(result.as_object().unwrap().values().all(|v| !v.is_null()));
}
