//! Integration test for the process-wide agent abort-signal registry
//! used by `SendMessage(shutdown_request)` for live abort delivery.
//!
//! Mirrors CC-fork's `SendMessageTool.ts:357` behavior —
//! `task.abortController.abort()` on an in-process teammate. In
//! sudocode the equivalent lookup is
//! `tools::abort_registered_agent(agent_id)`, which flips the
//! subagent's [`runtime::HookAbortSignal`].
//!
//! The registry is deliberately global (one per process) because agent
//! IDs are globally unique within a process. Tests here pick
//! never-colliding ids (test name + timestamp) so parallel test
//! execution stays isolated without needing a per-test mutex.

use std::time::{SystemTime, UNIX_EPOCH};

use runtime::HookAbortSignal;
use tools::{abort_registered_agent, register_agent_abort_signal, unregister_agent_abort_signal};

fn unique_agent_id(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("test-{label}-{nanos}")
}

// ── register + abort ──────────────────────────────────────────────

#[test]
fn abort_registered_agent_flips_signal_and_returns_true() {
    // given — a subagent whose HookAbortSignal is registered
    let signal = HookAbortSignal::default();
    let agent_id = unique_agent_id("abort-hits");
    register_agent_abort_signal(&agent_id, signal.clone());

    // sanity: signal starts un-aborted
    assert!(!signal.is_aborted(), "fresh signal must not be aborted");

    // when — SendMessage(shutdown_request) target lookup
    let hit = abort_registered_agent(&agent_id);

    // then — signal is aborted and lookup reports success
    assert!(
        hit,
        "abort_registered_agent must return true when target is registered"
    );
    assert!(
        signal.is_aborted(),
        "signal must be flipped by abort_registered_agent"
    );

    unregister_agent_abort_signal(&agent_id);
}

#[test]
fn abort_registered_agent_returns_false_for_unknown_agent() {
    let missing = unique_agent_id("nobody-registered");
    // Never called register — target is unknown.
    let hit = abort_registered_agent(&missing);
    assert!(
        !hit,
        "unregistered agent must return false (silent no-op path)"
    );
}

#[test]
fn unregister_prevents_subsequent_abort() {
    let signal = HookAbortSignal::default();
    let agent_id = unique_agent_id("unregister-clears");
    register_agent_abort_signal(&agent_id, signal.clone());

    // Simulate the subagent's `run_spawned_agent_job` cleanup on
    // completion — the entry is removed from the registry.
    unregister_agent_abort_signal(&agent_id);

    // A `SendMessage(shutdown_request)` racing the completion now
    // returns false. The already-terminated signal doesn't get flipped
    // (though even if it did it would be inert — this test proves the
    // lookup path is clean).
    let hit = abort_registered_agent(&agent_id);
    assert!(!hit, "unregistered agent must not be aborted");
    assert!(
        !signal.is_aborted(),
        "unregistered signal must not be flipped"
    );
}

#[test]
fn re_registering_overwrites_prior_signal() {
    // A recycled agent name (fresh agent gets the same id as one that
    // just finished) MUST bind the NEW signal, not the stale one.
    // This is the invariant that stops a `SendMessage(shutdown_request)`
    // from silently aborting the wrong agent when ids are recycled.
    let old_signal = HookAbortSignal::default();
    let new_signal = HookAbortSignal::default();
    let agent_id = unique_agent_id("recycled-id");

    register_agent_abort_signal(&agent_id, old_signal.clone());
    register_agent_abort_signal(&agent_id, new_signal.clone());

    let hit = abort_registered_agent(&agent_id);

    assert!(hit);
    assert!(new_signal.is_aborted(), "new signal must be aborted");
    assert!(
        !old_signal.is_aborted(),
        "old signal must NOT be aborted (overwritten registration must break the link)"
    );

    unregister_agent_abort_signal(&agent_id);
}
