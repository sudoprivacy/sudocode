//! Integration test for the `SendMessage` tool's mailbox surface —
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
            .map_or(0, |d| d.as_nanos()),
    ));
    fs::create_dir_all(&root).expect("workspace tmp dir");
    root
}

fn make_envelope(from: &str, to: &str, text: &str, kind: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: from.to_string(),
        to: to.to_string(),
        body: text.to_string(),
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

    let expected_file = agent_mailbox::inbox_path_under(&ws, "researcher");
    assert!(
        expected_file.exists(),
        "recipient's inbox must be created lazily on first append at {}",
        expected_file.display()
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
    let ts = value
        .get("timestamp")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
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
    assert_eq!(envelopes[0].body, "first");
    assert_eq!(envelopes[1].body, "second");
    assert_eq!(envelopes[2].body, "third");
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
    assert_eq!(envelopes[0].body, "good1");
    assert_eq!(envelopes[1].body, "good2");
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
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: "team-lead".to_string(),
        to: "worker".to_string(),
        body,
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
    // Body is JSON-in-JSON — parse the outer `body` field again.
    let inner: serde_json::Value = serde_json::from_str(&round[0].body).expect("parse inner body");
    assert_eq!(
        inner.get("request_id").and_then(|v| v.as_str()),
        Some("req_abc")
    );
    assert_eq!(
        inner.get("reason").and_then(|v| v.as_str()),
        Some("user cancelled")
    );
}

/// Every host tells an agent the same A2A contract, and it matches what the agent sees.
///
/// One section and one wrapper: the section describes the `<mailbox-message>` block
/// that `compose_next_turn_from_envelopes` really produces, says that only `send`
/// reaches a peer, and makes no claim about who reads the session. A host-specific
/// promise is how plain ACP sessions came to be told "you stay silent" while their
/// prose was being mailed to the sender.
#[test]
fn every_host_tells_an_agent_the_same_a2a_contract() {
    let section = agent_mailbox::a2a_prompt_section("win-ai");
    assert!(section.contains(&agent_mailbox::a2a_reply_contract("win-ai")));
    assert!(section.contains(agent_mailbox::a2a_discovery_contract()));

    let envelope = agent_mailbox::MailboxEnvelope::from_bytes(
        br#"{"from":"peer","to":"win-ai","body":"hello"}"#,
    )
    .unwrap();
    let wrapped = agent_mailbox::compose_next_turn_from_envelopes(&[envelope]);
    assert_eq!(
        wrapped,
        "<mailbox-message from=\"peer\">\nhello\n</mailbox-message>"
    );
    assert!(
        section.contains("`<mailbox-message"),
        "the section describes the wrapper the agent is actually shown:\n{section}"
    );
    assert!(
        section.contains("Only a `send` call reaches another agent"),
        "the one delivery rule:\n{section}"
    );
    let lower = section.to_lowercase();
    for claim in [
        "human",
        "controller",
        "delivered to that sender",
        "stay silent",
    ] {
        assert!(
            !lower.contains(claim),
            "no host-specific claim ({claim}):\n{section}"
        );
    }
}
