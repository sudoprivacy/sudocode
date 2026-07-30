//! PTY tests for the live spinner token counter.
//!
//! The spinner displays `↓ N tokens` during streaming once enough bytes
//! have arrived and at least 1 s has elapsed. After the turn, the status
//! line prints cumulative token info (`Nk tokens`).
//!
//! ```bash
//! cargo test --test pty_spinner_token_counter                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_spinner_token_counter  # real API
//! ```
mod common;

use std::time::Duration;

use common::TestEnv;

/// After a single-turn prompt the status line shows token info.
///
/// Mock mode: the status line always appears (fast response, token count
/// from the mock usage payload).
/// Live mode: same — the status line is post-turn, always rendered.
#[test]
fn status_line_shows_token_count_after_turn() {
    let env = TestEnv::new("spinner-tokens");
    let prompt = env.prompt(
        "What is 2+2? Answer with just the number.",
        "single_turn_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    // The response must contain the answer.
    sess.expect("4").expect("response should contain '4'");

    // After the turn, the status line shows `N tokens` or `N.Nk tokens`.
    sess.expect("tokens")
        .expect("post-turn status line should show token count");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "single-turn should exit 0; got {exit}");
}

/// In live mode, the spinner should display `↓` (the token counter
/// marker) during a multi-paragraph response that takes >1 s.
///
/// Mock mode skips this assertion because the mock response completes
/// too fast for the 1 s display threshold.
#[test]
fn live_spinner_shows_token_counter_during_streaming() {
    let env = TestEnv::new("spinner-tokens-live");

    if env.is_mock() {
        // Mock responses complete in <100 ms — the spinner never reaches
        // the SHOW_TOKENS_AFTER_SECS threshold. Just verify no crash.
        let prompt = env.prompt("Tell me a short joke.", "streaming_text");
        let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
        sess.expect("\\w+").expect("some response text");
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);
        return;
    }

    // Live mode: ask for a long response so streaming takes >1 s.
    let prompt = env.prompt(
        "Write a 3-paragraph essay about the history of the Rust programming language. Be detailed.",
        "unused_live_only",
    );
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    sess.set_default_timeout(Duration::from_secs(60));

    // During streaming, the spinner should show the `↓` token counter
    // marker at some point. We check the full terminal contents after
    // the response completes — crossterm's in-place updates leave
    // traces in the PTY scrollback.
    sess.expect("\\w+\\s+\\w+")
        .expect("multi-word response should stream");

    // Wait for the turn to finish and status line to appear.
    sess.expect("tokens")
        .expect("post-turn status line should show token count");

    // Check terminal contents for the spinner token counter marker.
    let contents = sess.render(|s| s.contents());
    assert!(
        contents.contains('↓'),
        "spinner should have shown ↓ token counter during streaming.\nTerminal contents:\n{contents}"
    );

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}
