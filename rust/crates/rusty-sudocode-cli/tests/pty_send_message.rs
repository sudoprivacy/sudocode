//! PTY tests for the `SendMessage` inter-agent messaging tool.
//!
//! Coverage target: roadmap §Feature-inventory row "SendMessage
//! (inter-agent messaging)" — subagent-cc-fork-parity commit B. Before
//! this file: 0 PTY tests → the tool didn't exist at all in sudocode's
//! LLM surface. After: the two branches that catch real regressions.
//!
//! ## What SendMessage does (ported from sudoprivacy/claude-code)
//!
//! `SendMessage({to, message, summary?, sender?})` writes one JSONL
//! envelope to `<workspace>/.sudocode-inbox/<recipient>.jsonl`.
//! Semantics matched to `sudoprivacy/claude-code`'s
//! `SendMessageTool.ts` flag-off default path:
//!
//! - `to = "*"` → broadcast: enumerate recipients from existing
//!   `.sudocode-inbox/*.jsonl` files (except self) and write one line
//!   per recipient. Empty inbox dir → success with `recipients: []`.
//! - `to = <name>` + `message: string` → point-to-point plain text.
//!   `summary` is required (matches fork's `validateInput`).
//! - `to = <name>` + `message: {type: shutdown_request|...}` →
//!   structured envelope. Broadcast is forbidden for structured
//!   messages (fork's constraint, mirrored here).
//!
//! ## Two branches that matter in production
//!
//! 1. **Plain text point-to-point** — the tool must actually create
//!    `.sudocode-inbox/<recipient>.jsonl` and its content must be
//!    valid JSON with `from` / `to` / `text` / `kind: "message"`. The
//!    file assertion is disk-level — the strongest layer, works in
//!    both mock and live modes.
//!
//! 2. **Broadcast on empty inbox dir** — regression sentinel against
//!    "broadcast crashes when there are no recipients yet". The mock
//!    scenario sends `{to: "*"}` to a fresh workspace; the tool must
//!    return success with a `recipients: []` payload rather than
//!    error out.
//!
//! What's NOT covered here:
//! - Structured `shutdown_request` / `shutdown_response` /
//!   `plan_approval_response` envelopes — covered by shape review;
//!    live-mode roundtrip requires two agents which is out of scope
//!    for a single PTY.
//! - `sender` override — trivial branch.
//!
//! ```bash
//! cargo test --test pty_send_message                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_send_message  # real API
//! ```

mod common;

use std::fs;
use std::path::PathBuf;

use common::TestEnv;
use serde_json::Value;

fn inbox_dir(env: &TestEnv) -> PathBuf {
    env.workspace_root().join(".sudocode-inbox")
}

fn inbox_file(env: &TestEnv, recipient: &str) -> PathBuf {
    inbox_dir(env).join(format!("{recipient}.jsonl"))
}

fn read_jsonl(path: &std::path::Path) -> Vec<Value> {
    if !path.exists() {
        return Vec::new();
    }
    let text = fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// 1. Plain-text point-to-point — envelope lands on disk
// ──────────────────────────────────────────────────────────────────────

/// The model calls `SendMessage({to: "researcher", summary: "...",
/// message: "..."})`. The tool must create
/// `.sudocode-inbox/researcher.jsonl` and the file's single line must
/// be a valid envelope with `kind: "message"`, `from`/`to`/`text`
/// present, and no path traversal outside the inbox dir.
#[test]
fn send_message_plain_text_creates_inbox_file() {
    let env = TestEnv::new("send-message-plain");

    // Sanity: fresh workspace has no inbox yet.
    assert!(
        !inbox_dir(&env).exists(),
        "fresh workspace must not have an inbox yet"
    );

    let prompt = env.prompt(
        "Please call the SendMessage tool with to=researcher, summary=\"look into flaky test\", message=\"please investigate the flaky test\". Do not describe it; just call the tool.",
        "send_message_plain_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "SendMessage",
        &prompt,
    ]);

    sess.expect("SendMessage")
        .expect("model must invoke SendMessage (agent trigger)");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "send_message plain turn should exit 0; got {exit}");

    // Disk assertion — strongest layer; works in both mock and live.
    let path = inbox_file(&env, "researcher");
    assert!(
        path.exists(),
        "SendMessage(plain) must create the recipient inbox at {}",
        path.display()
    );
    let envelopes = read_jsonl(&path);
    assert_eq!(
        envelopes.len(),
        1,
        "point-to-point send must produce exactly one envelope, got {}",
        envelopes.len()
    );
    let envelope = &envelopes[0];
    assert_eq!(
        envelope.get("kind").and_then(|v| v.as_str()),
        Some("message"),
        "envelope kind must be \"message\" for plain text; got {envelope}"
    );
    assert_eq!(
        envelope.get("to").and_then(|v| v.as_str()),
        Some("researcher"),
        "envelope `to` must round-trip the recipient name; got {envelope}"
    );
    assert!(
        envelope
            .get("text")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty()),
        "envelope `text` must be a non-empty string; got {envelope}"
    );
    assert!(
        envelope
            .get("from")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "envelope `from` must be a non-empty sender name; got {envelope}"
    );

    // Path traversal sentinel: no sibling files created outside the
    // sanitized recipient stem.
    let entries: Vec<_> = fs::read_dir(inbox_dir(&env))
        .expect("read inbox dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "inbox dir must contain exactly one file; got {entries:?}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. Broadcast on empty inbox dir — success with recipients: []
// ──────────────────────────────────────────────────────────────────────

/// The model calls `SendMessage({to: "*", summary, message})` against
/// a fresh workspace with no `.sudocode-inbox/` directory. The tool
/// must return success (not error) with a `recipients: []` payload —
/// the same shape the fork returns for "you are the only team member".
///
/// Regression sentinel against:
/// - broadcast crashing on missing inbox dir
/// - broadcast writing to itself and creating a spurious inbox file
#[test]
fn send_message_broadcast_on_empty_inbox_is_a_noop() {
    let env = TestEnv::new("send-message-broadcast-empty");
    assert!(!inbox_dir(&env).exists());

    let prompt = env.prompt(
        "Please call the SendMessage tool with to=\"*\", summary=\"team-wide ping\", message=\"quick standup ping\". Do not describe it; just call the tool.",
        "send_message_broadcast_empty_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "SendMessage",
        &prompt,
    ]);

    sess.expect("SendMessage")
        .expect("model must invoke SendMessage (agent trigger)");
    // Response payload must claim success even with no recipients.
    sess.expect(r#""success":\s*true"#)
        .expect("broadcast on empty inbox must return success:true");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "send_message broadcast-empty turn should exit 0; got {exit}"
    );

    // The tool must NOT create any recipient inbox files as a
    // side-effect. It may or may not create the empty `.sudocode-inbox`
    // dir; assert only on files.
    if let Ok(entries) = fs::read_dir(inbox_dir(&env)) {
        let files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .collect();
        assert!(
            files.is_empty(),
            "broadcast on empty inbox must not create any inbox files; got {files:?}"
        );
    }
}
