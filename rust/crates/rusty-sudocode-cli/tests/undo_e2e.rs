//! End-to-end coverage for `/undo` against the real `scode` binary using
//! the `--resume` non-interactive path.
//!
//! Each test builds a session JSONL that contains the kind of
//! `edit_file` / `write_file` tool-result envelopes the runtime would
//! emit, drops the binary on it via `--resume <path> /undo`, then asserts
//! the on-disk file was actually restored.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::{ContentBlock, ConversationMessage, MessageRole, Session};
use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn resumed_undo_restores_file_modified_by_edit_file_tool() {
    // given — workspace with the post-edit content on disk and a session
    // recording the corresponding edit_file ToolResult.
    let temp_dir = unique_temp_dir("undo-edit");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let target = temp_dir.join("file.txt");
    let original_text = "version 1 alpha\nversion 1 beta\n";
    let edited_text = "version 1 alpha\nversion 1 OMEGA\n";
    fs::write(&target, edited_text).expect("seed edited file");

    let edit_payload = serde_json::json!({
        "filePath": target.to_str().expect("utf8 path"),
        "oldString": "version 1 beta",
        "newString": "version 1 OMEGA",
        "originalFile": original_text,
        "userModified": false,
        "replaceAll": false,
    })
    .to_string();

    let session_path = temp_dir.join("session.jsonl");
    let mut session = workspace_session(&temp_dir).with_persistence_path(&session_path);
    session
        .push_user_text("apply that edit")
        .expect("user prompt persisted");
    session
        .push_message(ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "tool-1".into(),
                name: "edit_file".into(),
                input: "{}".into(),
                thought_signature: None,
            }],
            usage: None,
            model: None,
        })
        .expect("tool_use persisted");
    session
        .push_message(ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".into(),
                tool_name: "edit_file".into(),
                output: edit_payload,
                is_error: false,
            }],
            usage: None,
            model: None,
        })
        .expect("tool_result persisted");

    // when
    let output = run_scode(
        &temp_dir,
        &[
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/undo",
        ],
    );

    // then
    assert!(
        output.status.success(),
        "scode failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(
        stdout.contains("Restored"),
        "expected confirmation; stdout:\n{stdout}",
    );
    let on_disk = fs::read_to_string(&target).expect("target should exist");
    assert_eq!(on_disk, original_text, "file should be reverted in place");
}

#[test]
fn resumed_undo_deletes_file_when_write_file_originally_created_it() {
    // given — write_file created a new file (originalFile: null). The
    // existing on-disk content is the post-write payload. /undo should
    // delete it.
    let temp_dir = unique_temp_dir("undo-create");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let target = temp_dir.join("brand_new.txt");
    fs::write(&target, "fresh content\n").expect("seed file");

    let create_payload = serde_json::json!({
        "type": "create",
        "filePath": target.to_str().expect("utf8 path"),
        "content": "fresh content\n",
        "originalFile": Value::Null,
        "structuredPatch": [],
    })
    .to_string();

    let session_path = temp_dir.join("session.jsonl");
    let mut session = workspace_session(&temp_dir).with_persistence_path(&session_path);
    session
        .push_message(ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-create-1".into(),
                tool_name: "write_file".into(),
                output: create_payload,
                is_error: false,
            }],
            usage: None,
            model: None,
        })
        .expect("tool_result persisted");

    // when
    let output = run_scode(
        &temp_dir,
        &[
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/undo",
        ],
    );

    // then
    assert!(
        output.status.success(),
        "scode failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(
        stdout.contains("Deleted"),
        "expected deletion confirmation; stdout:\n{stdout}",
    );
    assert!(!target.exists(), "file should have been removed");
}

#[test]
fn resumed_undo_reports_nothing_to_undo_when_session_has_no_edits() {
    // given — session with only a user prompt; no tool results to undo.
    let temp_dir = unique_temp_dir("undo-empty");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let session_path = temp_dir.join("session.jsonl");
    let mut session = workspace_session(&temp_dir).with_persistence_path(&session_path);
    session
        .push_user_text("just chatting")
        .expect("user prompt persisted");

    // when
    let output = run_scode(
        &temp_dir,
        &[
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/undo",
        ],
    );

    // then
    assert!(
        output.status.success(),
        "scode failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(
        stdout.contains("Nothing to undo"),
        "expected friendly message; stdout:\n{stdout}",
    );
}

#[test]
fn resumed_undo_survives_hook_feedback_suffix_in_tool_result() {
    // Regression: merge_hook_feedback appends a "Hook feedback:" suffix to
    // ToolResult outputs. Naive JSON parse fails on that, which used to
    // silently disable /undo. The split_hook_feedback helper strips the
    // suffix before parsing.
    let temp_dir = unique_temp_dir("undo-hook-feedback");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let target = temp_dir.join("hooked.txt");
    let original_text = "before hook\n";
    let edited_text = "after hook\n";
    fs::write(&target, edited_text).expect("seed file");

    let edit_json = serde_json::json!({
        "filePath": target.to_str().expect("utf8 path"),
        "oldString": "before hook",
        "newString": "after hook",
        "originalFile": original_text,
        "userModified": false,
        "replaceAll": false,
    })
    .to_string();
    let polluted = format!("{edit_json}\n\nHook feedback:\nformatter clean");

    let session_path = temp_dir.join("session.jsonl");
    let mut session = workspace_session(&temp_dir).with_persistence_path(&session_path);
    session
        .push_message(ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-hook-1".into(),
                tool_name: "edit_file".into(),
                output: polluted,
                is_error: false,
            }],
            usage: None,
            model: None,
        })
        .expect("tool_result persisted");

    // when
    let output = run_scode(
        &temp_dir,
        &[
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/undo",
        ],
    );

    // then
    assert!(
        output.status.success(),
        "scode failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), original_text);
}

#[test]
fn resumed_undo_emits_structured_json_when_requested() {
    let temp_dir = unique_temp_dir("undo-json");
    fs::create_dir_all(&temp_dir).expect("temp dir should exist");
    let target = temp_dir.join("structured.txt");
    let original_text = "lhs\n";
    let edited_text = "rhs\n";
    fs::write(&target, edited_text).expect("seed file");

    let payload = serde_json::json!({
        "filePath": target.to_str().expect("utf8 path"),
        "oldString": "lhs",
        "newString": "rhs",
        "originalFile": original_text,
        "userModified": false,
        "replaceAll": false,
    })
    .to_string();

    let session_path = temp_dir.join("session.jsonl");
    let mut session = workspace_session(&temp_dir).with_persistence_path(&session_path);
    session
        .push_message(ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-json-1".into(),
                tool_name: "edit_file".into(),
                output: payload,
                is_error: false,
            }],
            usage: None,
            model: None,
        })
        .expect("tool_result persisted");

    let output = run_scode(
        &temp_dir,
        &[
            "--output-format",
            "json",
            "--resume",
            session_path.to_str().expect("utf8 path"),
            "/undo",
        ],
    );

    assert!(
        output.status.success(),
        "scode failed.\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("json output");
    assert_eq!(parsed["kind"], "undo");
    assert_eq!(parsed["applied"], true);
    assert_eq!(parsed["tool_name"], "edit_file");
    assert_eq!(parsed["tool_use_id"], "tool-json-1");
    assert_eq!(parsed["deleted"], false);
    assert_eq!(parsed["file_path"], target.to_str().expect("utf8 path"));
    assert_eq!(fs::read_to_string(&target).unwrap(), original_text);
}

fn run_scode(current_dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_scode"));
    command.current_dir(current_dir).args(args);
    command.output().expect("scode should launch")
}

fn workspace_session(root: &Path) -> Session {
    Session::new().with_workspace_root(root.to_path_buf())
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
