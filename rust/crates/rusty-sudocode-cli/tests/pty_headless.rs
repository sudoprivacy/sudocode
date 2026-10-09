//! Print mode must never read approval answers, even from a real terminal.
mod common;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::{ContentBlock, MessageRole, Session, SessionStore};

fn assert_file_roundtrip(env: &common::TestEnv, nonce: &str, format: &str) {
    let store = SessionStore::from_cwd(env.workspace_root()).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "{format}: expected one persisted session"
    );
    let session = Session::load_from_path(&sessions[0].path).unwrap();
    let fixture = env.workspace_root().join("fixture.txt");
    let mut reads = std::collections::BTreeSet::new();
    let mut successful_reads = std::collections::BTreeSet::new();
    for message in &session.messages {
        for block in &message.blocks {
            match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } if matches!(name.as_str(), "read_file" | "Read") => {
                    let args: serde_json::Value = serde_json::from_str(input).unwrap();
                    let path = args["path"].as_str().expect("read_file requires a path");
                    // A foreign path that later fails must not be hidden by a
                    // subsequent successful read. Keep the ordinary relative
                    // task: supplying an absolute path would mask this incident.
                    let requested = env.workspace_root().join(PathBuf::from(path));
                    assert_eq!(
                        std::fs::canonicalize(&requested).ok(),
                        Some(std::fs::canonicalize(&fixture).unwrap()),
                        "{format}: model read an unrelated path: {path}"
                    );
                    reads.insert(id.as_str());
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    output,
                    ..
                } => {
                    assert!(
                        !is_error,
                        "{format}: file workflow recovered from a failed tool: {tool_use_id}"
                    );
                    if output.contains(nonce) {
                        successful_reads.insert(tool_use_id.as_str());
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        !reads.is_disjoint(&successful_reads),
        "{format}: no actual file read returned this run's contents"
    );
    let answer = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .expect("headless command must persist its assistant answer");
    let text: String = answer
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains(nonce),
        "{format}: final persisted answer lacks the actual file contents"
    );
}

#[test]
fn print_denies_approval_without_reading_the_terminal() {
    let env = common::TestEnv::new("headless-permission");
    if env.is_live() {
        return;
    }
    let prompt = env.prompt("", "bash_permission_prompt_denied");
    let mut child = env.spawn(&[
        "-p",
        &prompt,
        "--permission-mode",
        "workspace-write",
        "--output-format=json",
    ]);
    child.set_default_timeout(std::time::Duration::from_secs(15));
    // Sending no keystrokes is the assertion: the old prompter would hang here.
    child
        .expect("interactive approval unavailable in print mode")
        .unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
}

/// A finite invocation must retain its synchronous child until completion,
/// even when the ordinary REPL would hand that child back as a background job.
#[test]
fn print_waits_for_synchronous_agent_past_auto_background_threshold() {
    let env = common::TestEnv::new("headless-sync-agent");
    if env.is_live() {
        return;
    }
    let prompt = env.prompt("", "subagent_events_sync_slow");
    let mut child = env.spawn_with_env(
        &["-p", &prompt, "--permission-mode", "danger-full-access"],
        &[("SUDOCODE_AGENT_AUTO_BG_SECS", "1")],
    );
    child.expect("subagent_events_sync_slow done").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    let requests: Vec<serde_json::Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    let result = requests
        .iter()
        .flat_map(|request| request["messages"].as_array().unwrap())
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == "toolu_events_sync")
        .expect("parent must receive the delegated result");
    let text = result["content"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>()
        });
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(manifest["status"], "completed", "{manifest}");
    assert_eq!(
        manifest["result"],
        mock_anthropic_service::SUBAGENT_CHILD_ANSWER
    );
}

#[test]
fn print_completes_tool_roundtrip_in_each_output_format() {
    for format in ["text", "json", "stream-json"] {
        let env = common::TestEnv::new("headless-output");
        let nonce = format!(
            "HEADLESS_FILE_{}_{}",
            format,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::fs::write(env.workspace_root().join("fixture.txt"), &nonce).unwrap();
        let prompt = env.prompt(
            "Read fixture.txt and report its exact contents.",
            "read_file_roundtrip",
        );
        let requests = std::env::var_os("SCODE_LIVE_ARTIFACTS").map_or_else(
            || env.workspace_root().join("requests"),
            |root| PathBuf::from(root).join(env.workspace_root().file_name().unwrap()),
        );
        let requests = requests.to_str().unwrap();
        let mut child = env.spawn_with_env(
            &[
                "-p",
                &prompt,
                "--permission-mode",
                "read-only",
                "--output-format",
                format,
            ],
            &[("SUDOCODE_DUMP_REQUESTS", requests)],
        );
        child.expect(&nonce).unwrap_or_else(|error| {
            panic!("{format}: {error}; {}", common::screen_tail(&child, 6000))
        });
        assert_eq!(child.expect_eof().unwrap(), 0);
        assert_file_roundtrip(&env, &nonce, format);
    }
}
