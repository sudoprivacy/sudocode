//! Integration test for the SendMessage tool's mailbox surface —
//! `runtime::agent_mailbox`.
//!
//! Complements `session_pre_seed.rs` and `fork_dispatch_context.rs`;
//! together they lock in every runtime-side primitive the sub-agent
//! parity work in the tools crate depends on, without spawning a PTY
//! or hitting an LLM.

use std::fs;

use runtime::agent_mailbox::{self, kinds, MailboxEnvelope};

fn temp_workspace(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "mailbox-roundtrip-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    fs::create_dir_all(&root).expect("workspace tmp dir");
    root
}

fn make_envelope(from: &str, to: &str, text: &str, kind: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        from: from.to_string(),
        to: to.to_string(),
        text: text.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: kind.to_string(),
        request_id: None,
    }
}

// ── append + round-trip ───────────────────────────────────────────

#[test]
fn append_creates_inbox_dir_and_file() {
    let ws = temp_workspace("append-creates-file");

    agent_mailbox::append_envelope(
        &ws,
        "researcher",
        make_envelope("team-lead", "researcher", "hello", kinds::MESSAGE),
    )
    .expect("append should succeed");

    let expected_dir = ws.join(".sudocode-inbox");
    assert!(
        expected_dir.exists(),
        ".sudocode-inbox must be created lazily on first append"
    );
    let expected_file = expected_dir.join("researcher.jsonl");
    assert!(
        expected_file.exists(),
        "recipient's JSONL file must exist after first append"
    );
}

#[test]
fn append_fills_timestamp_when_zero() {
    let ws = temp_workspace("append-fills-timestamp");
    let path = agent_mailbox::append_envelope(
        &ws,
        "worker",
        make_envelope("team-lead", "worker", "hi", kinds::MESSAGE),
    )
    .expect("append should succeed");
    let contents = fs::read_to_string(&path).expect("read jsonl");
    let value: serde_json::Value = serde_json::from_str(contents.trim()).expect("parse jsonl");
    let ts = value.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);
    assert!(
        ts > 0,
        "append_envelope must stamp `timestamp` with the current unix seconds when caller passes 0"
    );
}

#[test]
fn read_all_returns_envelopes_in_append_order() {
    let ws = temp_workspace("read-all-order");
    agent_mailbox::append_envelope(
        &ws,
        "worker",
        make_envelope("a", "worker", "first", kinds::MESSAGE),
    )
    .unwrap();
    agent_mailbox::append_envelope(
        &ws,
        "worker",
        make_envelope("b", "worker", "second", kinds::MESSAGE),
    )
    .unwrap();
    agent_mailbox::append_envelope(
        &ws,
        "worker",
        make_envelope("c", "worker", "third", kinds::MESSAGE),
    )
    .unwrap();

    let envelopes = agent_mailbox::read_all(&ws, "worker").expect("read_all should succeed");
    assert_eq!(envelopes.len(), 3);
    assert_eq!(envelopes[0].text, "first");
    assert_eq!(envelopes[1].text, "second");
    assert_eq!(envelopes[2].text, "third");
}

#[test]
fn read_all_on_missing_inbox_is_empty_not_error() {
    let ws = temp_workspace("read-missing");
    // Never wrote anything for this recipient.
    let envelopes = agent_mailbox::read_all(&ws, "nobody").expect("missing inbox must not error");
    assert!(envelopes.is_empty());
}

#[test]
fn read_all_skips_malformed_lines() {
    let ws = temp_workspace("read-malformed");
    agent_mailbox::append_envelope(
        &ws,
        "worker",
        make_envelope("a", "worker", "good1", kinds::MESSAGE),
    )
    .unwrap();
    // Inject a malformed line via raw file write between two valid appends.
    let path = agent_mailbox::mailbox_path(&ws, "worker");
    {
        use std::io::Write as _;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open jsonl for malformed injection");
        writeln!(f, "not-json-at-all").expect("write malformed line");
    }
    agent_mailbox::append_envelope(
        &ws,
        "worker",
        make_envelope("b", "worker", "good2", kinds::MESSAGE),
    )
    .unwrap();

    let envelopes = agent_mailbox::read_all(&ws, "worker").expect("read_all should succeed");
    assert_eq!(
        envelopes.len(),
        2,
        "malformed line must be skipped, not counted"
    );
    assert_eq!(envelopes[0].text, "good1");
    assert_eq!(envelopes[1].text, "good2");
}

// ── list_recipients ───────────────────────────────────────────────

#[test]
fn list_recipients_returns_sorted_names() {
    let ws = temp_workspace("list-recipients-sorted");
    agent_mailbox::append_envelope(
        &ws,
        "zebra",
        make_envelope("a", "zebra", "x", kinds::MESSAGE),
    )
    .unwrap();
    agent_mailbox::append_envelope(
        &ws,
        "alpha",
        make_envelope("a", "alpha", "y", kinds::MESSAGE),
    )
    .unwrap();
    agent_mailbox::append_envelope(
        &ws,
        "mango",
        make_envelope("a", "mango", "z", kinds::MESSAGE),
    )
    .unwrap();

    let names = agent_mailbox::list_recipients(&ws).expect("list should succeed");
    assert_eq!(names, vec!["alpha", "mango", "zebra"]);
}

#[test]
fn list_recipients_on_fresh_workspace_is_empty() {
    let ws = temp_workspace("list-fresh");
    let names = agent_mailbox::list_recipients(&ws).expect("list should succeed");
    assert!(names.is_empty());
}

// ── kinds SSOT ───────────────────────────────────────────────────

#[test]
fn kind_constants_are_stable_wire_strings() {
    // Any change here MUST stay in sync with CC-fork's
    // `sudoprivacy/claude-code`'s teammate-mailbox message tags —
    // recipients on both sides must recognise the exact same
    // spellings for the pipe to work end-to-end.
    assert_eq!(kinds::MESSAGE, "message");
    assert_eq!(kinds::SHUTDOWN_REQUEST, "shutdown_request");
    assert_eq!(kinds::SHUTDOWN_RESPONSE, "shutdown_response");
    assert_eq!(kinds::PLAN_APPROVAL_RESPONSE, "plan_approval_response");
}

// ── structured envelope shape (round-trip through JSONL) ─────────

#[test]
fn structured_shutdown_envelope_survives_roundtrip() {
    let ws = temp_workspace("structured-shutdown");
    let body = serde_json::json!({
        "type": "shutdown_request",
        "request_id": "req_abc",
        "from": "team-lead",
        "reason": "user cancelled",
    })
    .to_string();
    let envelope = MailboxEnvelope {
        from: "team-lead".to_string(),
        to: "worker".to_string(),
        text: body,
        summary: None,
        timestamp: 0,
        color: None,
        kind: kinds::SHUTDOWN_REQUEST.to_string(),
        request_id: Some("req_abc".to_string()),
    };
    agent_mailbox::append_envelope(&ws, "worker", envelope).expect("append");

    let round = agent_mailbox::read_all(&ws, "worker").expect("read");
    assert_eq!(round.len(), 1);
    assert_eq!(round[0].kind, kinds::SHUTDOWN_REQUEST);
    assert_eq!(round[0].request_id.as_deref(), Some("req_abc"));
    // Body is JSON-in-JSON — parse the outer `text` field again.
    let inner: serde_json::Value = serde_json::from_str(&round[0].text).expect("parse inner body");
    assert_eq!(
        inner.get("request_id").and_then(|v| v.as_str()),
        Some("req_abc")
    );
    assert_eq!(
        inner.get("reason").and_then(|v| v.as_str()),
        Some("user cancelled")
    );
}
