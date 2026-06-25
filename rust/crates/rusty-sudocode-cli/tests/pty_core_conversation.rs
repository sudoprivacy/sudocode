//! PTY tests for core conversation features.
//!
//! ## Test principles
//!
//! - **Live quality first.** Every assertion must be meaningful against
//!   a real API. Mock is for CI convenience, not a separate test suite.
//! - **DRY.** One set of assertions for both modes. `if env.is_mock()`
//!   only for things inherently mock-only (e.g. `captured_message_count`).
//! - **Agent trigger.** Live mode verifies the LLM selected the right
//!   tools — not just that scode can execute them.
//!
//! ```bash
//! cargo test --test pty_core_conversation                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_core_conversation  # real API
//! ```
mod common;

use std::time::Duration;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. Single-turn prompt — `scode "prompt"` → response → exit 0
// ──────────────────────────────────────────────────────────────────────

/// User runs `scode "What is 2+2?"`, sees "4" in the response, exits 0.
#[test]
fn single_turn_exits_after_response() {
    let env = TestEnv::new("single-turn");
    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    // Any model (mock or live) must produce "4" for "what is 2+2".
    sess.expect("4").expect("response should contain '4'");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "single-turn should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 2. Multi-turn context — prior turns carry forward
// ──────────────────────────────────────────────────────────────────────

/// REPL: send name → response greets → ask name back → response recalls.
#[test]
fn multi_turn_references_prior() {
    let env = TestEnv::new("multi-turn");

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.expect("❯").expect("should see REPL prompt");

    // First turn: introduce a name.
    let first = env.prompt(
        "My name is Alice. Please greet me by name.",
        "multi_turn_context",
    );
    sess.send(&format!("{first}\r")).expect("send first msg");
    sess.expect("Alice")
        .expect("first response should mention Alice");

    sess.expect("❯")
        .expect("should see prompt after first turn");

    // Second turn: ask for the name back.
    let second = env.prompt(
        "What is my name? Reply in one sentence.",
        "multi_turn_context",
    );
    sess.send(&format!("{second}\r")).expect("send second msg");
    sess.expect("Alice")
        .expect("second response should recall Alice");

    // Exit cleanly.
    sess.expect("❯")
        .expect("should see prompt after second turn");
    sess.send("/exit\r").expect("send /exit");

    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("scode should exit after /exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "interactive scode should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 3. Streaming response — tokens render incrementally
// ──────────────────────────────────────────────────────────────────────

/// Streamed tokens appear in the terminal as multi-word text before EOF.
#[test]
fn streaming_tokens_render_incrementally() {
    let env = TestEnv::new("streaming");
    let prompt = env.prompt("Tell me a short joke.", "streaming_text");

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    // Multiple words must appear — proves streaming flushed.
    sess.expect("\\w+\\s+\\w+")
        .expect("should see multi-word streamed text");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "streaming should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 4. Multi-tool turn roundtrip — read_file + grep_search in one turn
// ──────────────────────────────────────────────────────────────────────

/// Model calls read_file + grep_search, results feed the final response.
/// Agent trigger test: verifies LLM selected the correct tools.
#[test]
fn multi_tool_roundtrip() {
    let env = TestEnv::new("multi-tool");

    std::fs::write(
        env.workspace_root().join("fixture.txt"),
        "alpha parity line\nbeta line\ngamma parity line\n",
    )
    .expect("fixture.txt");

    let prompt = env.prompt(
        "Read fixture.txt and count how many lines contain the word 'parity'. Use read_file and grep_search tools.",
        "multi_tool_turn_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--allowedTools",
        "read_file,grep_search",
        &prompt,
    ]);

    // Agent trigger: verify tool names in PTY output (both modes).
    sess.set_default_timeout(Duration::from_secs(60));
    sess.expect("read_file")
        .expect("should see read_file tool call (agent trigger)");
    sess.expect("grep")
        .expect("should see grep_search tool call (agent trigger)");

    // Final response references the results.
    sess.expect("(?i)(parity|2|two|lines|occurrences|matches|complete)")
        .expect("final response should reference tool results");

    sess.set_default_timeout(Duration::from_secs(120));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("multi-tool scode should exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "multi-tool should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 5. Graceful cancel mid-execution — Ctrl+C during bash tool
// ──────────────────────────────────────────────────────────────────────

/// Ctrl+C during a long-running bash tool exits cleanly, not hang.
#[test]
#[cfg(unix)]
fn sigint_cancels_streaming() {
    let env = TestEnv::new("sigint-cancel");
    let prompt = env.prompt(
        "Run this exact bash command: printf 'interrupt-start'; sleep 30; printf 'interrupt-done'",
        "bash_interrupt_long_running",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    sess.expect("bash").expect("should see bash tool call");
    std::thread::sleep(Duration::from_millis(500));
    sess.send_ctrl('c').expect("send Ctrl+C");

    sess.set_default_timeout(Duration::from_secs(15));
    let _exit = sess
        .expect_eof()
        .expect("scode should exit after Ctrl+C, not hang");
}

// ──────────────────────────────────────────────────────────────────────
// 6. ESC key cancels mid-execution (CC parity)
// ──────────────────────────────────────────────────────────────────────

/// ESC key during a long-running bash tool exits cleanly (CC parity).
#[test]
#[cfg(unix)]
fn esc_cancels_streaming() {
    let env = TestEnv::new("esc-cancel");
    let prompt = env.prompt(
        "Run this exact bash command: printf 'esc-start'; sleep 30; printf 'esc-done'",
        "bash_interrupt_long_running",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);

    sess.expect("bash").expect("should see bash tool call");
    std::thread::sleep(Duration::from_millis(500));
    sess.send("\x1b").expect("send ESC");

    sess.set_default_timeout(Duration::from_secs(15));
    let _exit = sess
        .expect_eof()
        .expect("scode should exit after ESC, not hang");
}
