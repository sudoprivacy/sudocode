//! PTY tests for the `/compact` slash command.
//!
//! /compact has shipped for a while but had no PTY coverage; was marked
//! "Gap" in the roadmap. This file fills the gap as part of the
//! context-overflow systemic fix series (2026-06-28), alongside:
//! - `image_registry::preflight_base64` (ACP push_images downsample)
//! - `MAX_CONSECUTIVE_AUTO_COMPACT_NOOPS` (auto-compact circuit-breaker)
//!
//! The other two surfaces are internal — preflight runs inside the ACP
//! push_images handler before the LLM call (not user-visible from CLI),
//! and the circuit-breaker is a private counter on ConversationRuntime
//! observable only via internal state. PTY can't meaningfully reach
//! either without exposing instrumentation hooks (would violate the
//! simplify-contract audit principle). They're integration-tested by
//! the runtime crate's existing test suite.
//!
//! For `/compact` itself we can PTY-cover the user-visible contract:
//! the slash command produces a report message, and post-compact turns
//! still flow normally.
//!
//! ```bash
//! cargo test --test pty_compact                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_compact  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

/// User runs a couple of turns, types `/compact`, sees a compact report,
/// then sends another turn that still works (compacted session is usable).
#[test]
fn slash_compact_produces_report_and_session_continues() {
    let env = TestEnv::new("slash-compact");

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    // ── Turn 1: seed conversation with a memorable answer ──
    let first = env.prompt(
        "My name is Alice. Please greet me by name.",
        "compact_seed_alice",
    );
    sess.send(&format!("{first}\r")).expect("send first msg");
    sess.expect("Alice")
        .expect("first response should mention Alice");
    sess.expect("❯")
        .expect("should see prompt after first turn");

    // ── Turn 2: another turn so we have something for compact to summarize ──
    let second = env.prompt(
        "What is 7 times 6? Reply with the number only.",
        "compact_seed_arith",
    );
    sess.send(&format!("{second}\r")).expect("send second msg");
    sess.expect("42").expect("second response should contain '42'");
    sess.expect("❯")
        .expect("should see prompt after second turn");

    // ── /compact: triggers runtime::compact_session ──
    sess.send("/compact\r").expect("send /compact");
    // The compact-report message format is established by
    // `format_compact_report` in rusty-sudocode-cli; it contains either
    // "compact" (action verb) or a "kept" / "removed" count line.
    // Match loosely to survive copy tweaks.
    sess.expect("(?i)compact|kept|removed|summary")
        .expect("compact report should surface in the terminal");
    sess.expect("❯")
        .expect("should see prompt after /compact completes");

    // ── Turn 3: post-compact, session should still accept input ──
    // We can't assert that Alice is recalled — /compact preserves the last
    // N messages and Alice was 2 turns ago; she may or may not survive
    // depending on preserve_recent_messages. What we CAN assert is that
    // the runtime is still alive and responding (no crash from compact
    // mangling the session state).
    let third = env.prompt(
        "Reply with the single word: ready.",
        "compact_post_compact_ack",
    );
    sess.send(&format!("{third}\r")).expect("send third msg");
    sess.expect("(?i)ready")
        .expect("post-compact turn should still get a response");
    sess.expect("❯")
        .expect("should see prompt after post-compact turn");

    // Exit cleanly.
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("scode should exit after /exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "interactive scode should exit 0; got {exit}");
}
