//! PTY tests for the TurnRenderer chrome.
//!
//! These tests verify that the turn-scoped chrome (spinner + output
//! routing) works correctly. TurnRenderer is active by default when
//! stdout is a terminal (which it is in PTY tests).
//!
//! ```bash
//! cargo test --test pty_iocraft_chrome                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_iocraft_chrome  # real API
//! ```

mod common;

use common::TestEnv;

/// Single-turn prompt — response text appears and process exits.
#[test]
fn turn_renderer_single_turn_shows_response() {
    let env = TestEnv::new("tr-single-turn");
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    sess.expect("4").expect("response should contain 4");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// Process exits cleanly — no hanging render thread.
#[test]
fn turn_renderer_clean_exit() {
    let env = TestEnv::new("tr-clean-exit");
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    sess.expect("4").expect("response");
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// REPL mode: turn completes and prompt re-appears.
#[test]
fn turn_renderer_repl_reprompt() {
    let env = TestEnv::new("tr-repl-reprompt");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("❯").expect("initial prompt");

    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.expect("4").expect("response");
    sess.expect("❯").expect("prompt after turn");

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// Spinner shows model name during a turn.
#[test]
fn turn_renderer_spinner_shows_model() {
    let env = TestEnv::new("tr-spinner-model");
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    if env.is_mock() {
        sess.expect("sonnet")
            .expect("spinner should show model name");
    }
    sess.expect("4").expect("response");
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// Tool output with TurnRenderer — process exits cleanly.
#[test]
fn turn_renderer_tool_output() {
    let env = TestEnv::new("tr-tool-output");
    let prompt = env.prompt("Run echo hello in bash", "bash_stdout_roundtrip");
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);

    let exit = sess
        .expect_eof()
        .expect("process should exit after single turn");
    assert_eq!(exit, 0);
}
