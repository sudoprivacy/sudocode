//! PTY tests for the five core conversation features:
//!
//! 1. Single-turn prompt — exits cleanly after one response
//! 2. Multi-turn context — prior turns are visible in follow-ups
//! 3. Streaming response — tokens render incrementally
//! 4. Multi-tool turn roundtrip — multiple tool calls in one turn
//! 5. Graceful cancel mid-execution — Ctrl+C stops cleanly
//!
//! ## Dual-mode (mock / live)
//!
//! All tests go through `TestEnv` (see `common/mod.rs`). By default
//! they run against `MockAnthropicService` (CI-safe). Set
//! `SCODE_TEST_BACKEND=live` to run against a real API with the
//! credentials in `~/.nexus/sudocode/sudocode.json`.
//!
//! ```bash
//! # CI / default — mock, no API key needed
//! cargo test --test pty_core_conversation
//!
//! # Local — real API (sudorouter proxy)
//! SCODE_TEST_BACKEND=live cargo test --test pty_core_conversation
//! ```
mod common;

use std::time::Duration;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. Single-turn prompt — `scode "prompt"` → response → exit 0
// ──────────────────────────────────────────────────────────────────────

/// Spawn scode with a one-shot prompt, see a response, clean exit.
///
/// - Mock: expects "answer" and "4" (deterministic response).
/// - Live: expects any non-empty response before exit.
#[test]
fn single_turn_exits_after_response() {
    let env = TestEnv::new("single-turn");
    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    if env.is_mock() {
        sess.expect("answer").expect("mock: should see 'answer'");
        sess.expect("4").expect("mock: should see '4'");
    } else {
        // Live: just expect some output before EOF.
        sess.expect("(?s).+").expect("live: should see a response");
    }

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "single-turn should exit 0; got {exit}");

    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 1, "mock: exactly 1 request");
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. Multi-turn context — interactive REPL, prior turns carry forward
// ──────────────────────────────────────────────────────────────────────

/// Start REPL, send two messages, verify the second response shows
/// awareness of the first.
///
/// - Mock: first response = "Nice to meet you", second = "Your name is".
/// - Live: first response contains "Alice", second also contains "Alice".
#[test]
fn multi_turn_references_prior() {
    let env = TestEnv::new("multi-turn");

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    // Wait for the REPL prompt.
    sess.expect("❯").expect("should see REPL prompt");

    // First turn: introduce a name.
    let first = env.prompt(
        "My name is Alice. Please greet me by name.",
        "multi_turn_context",
    );
    sess.send(&format!("{first}\r")).expect("send first msg");

    if env.is_mock() {
        sess.expect("Nice to meet you")
            .expect("mock: first response greeting");
    } else {
        sess.expect("Alice")
            .expect("live: first response mentions Alice");
    }

    // Wait for prompt after first turn.
    sess.expect("❯")
        .expect("should see prompt after first turn");

    // Second turn: ask for the name back.
    let second = env.prompt(
        "What is my name? Reply in one sentence.",
        "multi_turn_context",
    );
    sess.send(&format!("{second}\r")).expect("send second msg");

    if env.is_mock() {
        sess.expect("Your name is")
            .expect("mock: second response recalls name");
    } else {
        sess.expect("Alice")
            .expect("live: second response recalls Alice");
    }

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

    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 2, "mock: exactly 2 requests");
    }
}

// ──────────────────────────────────────────────────────────────────────
// 3. Streaming response — tokens render incrementally
// ──────────────────────────────────────────────────────────────────────

/// Verify streamed tokens appear in the terminal before EOF.
///
/// - Mock: two SSE chunks produce "Mock streaming" then "parity harness".
/// - Live: any multi-word response proves streaming flushes.
#[test]
fn streaming_tokens_render_incrementally() {
    let env = TestEnv::new("streaming");
    let prompt = env.prompt("Tell me a short joke.", "streaming_text");

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    if env.is_mock() {
        sess.expect("Mock streaming").expect("mock: first chunk");
        sess.expect("parity harness").expect("mock: second chunk");
    } else {
        // Live: just verify some text appeared.
        sess.expect("(?s).+")
            .expect("live: should see streamed text");
    }

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "streaming should exit 0; got {exit}");

    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 1, "mock: exactly 1 request");
    }
}

// ──────────────────────────────────────────────────────────────────────
// 4. Multi-tool turn roundtrip — read_file + grep_search in one turn
// ──────────────────────────────────────────────────────────────────────

/// Model calls two tools in one turn, results feed the final response.
///
/// - Mock: deterministic read_file + grep_search → "roundtrip complete".
/// - Live: model reads fixture.txt and greps it, mentions content.
#[test]
fn multi_tool_roundtrip() {
    let env = TestEnv::new("multi-tool");

    // Both modes need this file — the model/mock will read_file + grep it.
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

    if env.is_mock() {
        // Mock: deterministic tool names in output.
        sess.expect("read_file")
            .expect("mock: should see read_file");
        sess.expect("grep").expect("mock: should see grep_search");
        sess.expect("roundtrip complete")
            .expect("mock: final response");
    } else {
        // Live: the model calls tools and responds. Tool names or
        // file content should appear. Use a generous timeout since
        // the model needs to think + execute two tools.
        sess.set_default_timeout(Duration::from_secs(60));
        sess.expect("(?i)(read_file|grep|fixture|parity|alpha)")
            .expect("live: should see tool activity or file content");
        sess.expect("(?i)(parity|2|two|lines|occurrences|matches)")
            .expect("live: final response references results");
    }

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

/// Ctrl+C during a long-running bash tool exits cleanly.
///
/// - Mock: bash sleeps 30s → interrupted by Ctrl+C.
/// - Live: model runs `sleep 30` → interrupted by Ctrl+C.
#[test]
#[cfg(unix)] // ConPTY does not propagate Ctrl+C to the bash subprocess the same way
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

    // Wait until the bash tool call starts executing.
    sess.expect("bash").expect("should see bash tool call");

    // Let the command start, then interrupt.
    std::thread::sleep(Duration::from_millis(500));
    sess.send_ctrl('c').expect("send Ctrl+C");

    // Process should exit — the assertion is that it does NOT hang.
    sess.set_default_timeout(Duration::from_secs(15));
    let _exit = sess
        .expect_eof()
        .expect("scode should exit after Ctrl+C, not hang");
}
