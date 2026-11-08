//! PTY tests for the `TurnRenderer` chrome.
//!
//! These tests verify that the turn-scoped chrome (spinner + output
//! routing) works correctly. `TurnRenderer` is active by default when
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

    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let budget = if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        env.timeout()
    };
    common::run_turn(&mut sess, &prompt, budget, "turn");
    let screen = sess.render(common::screen_contents);
    assert!(
        screen
            .rsplit_once("Answer only the number.")
            .is_some_and(|(_, reply)| reply.contains('4')),
        "the answer should render below the prompt:\n{screen}"
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// A submitted line is echoed into the transcript exactly once, and the
/// editor below it is empty again.
#[test]
fn submitted_line_is_echoed_once() {
    let env = TestEnv::new("tr-single-echo");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    common::expect_input_line_cleared(&sess, env.timeout(), "REPL input ready");
    sess.send("/version\r").expect("send /version");
    common::expect_screen(
        &sess,
        |screen| {
            screen
                .lines()
                .any(|line| line.trim_start().starts_with("Version"))
        },
        env.timeout(),
        "version output should appear",
    );
    common::expect_input_line_cleared(&sess, env.timeout(), "prompt after /version");
    let screen = sess.render(common::screen_contents);
    let echoes = screen
        .lines()
        .filter(|line| line.trim_end().ends_with("/version") && line.contains('\u{276f}'))
        .count();
    assert_eq!(
        echoes, 1,
        "the submitted line should be echoed once:\n{screen}"
    );

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

/// Tool output with `TurnRenderer` — process exits cleanly.
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
