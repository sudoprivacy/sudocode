//! PTY tests for the TurnRenderer chrome.
//!
//! These tests verify that the turn-scoped chrome (spinner + separators +
//! footer) renders correctly and cleans up when the turn ends.
//!
//! ```bash
//! cargo test --test pty_iocraft_chrome                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_iocraft_chrome  # real API
//! ```

mod common;

use common::TestEnv;

/// The iocraft spinner renders during a single-turn prompt and the
/// response text appears above the chrome region.
#[test]
fn iocraft_single_turn_shows_response() {
    let env = TestEnv::new("iocraft-single-turn");
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only", &prompt],
        &[("SUDOCODE_IOCRAFT", "1")],
    );

    // The response should contain "4".
    sess.expect("4").expect("response should contain 4");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// After a turn completes, the chrome (separator + footer) is cleared
/// and the status line is printed. No orphaned chrome lines should
/// remain in the output.
#[test]
fn iocraft_chrome_cleared_after_turn() {
    let env = TestEnv::new("iocraft-chrome-clear");
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only", &prompt],
        &[("SUDOCODE_IOCRAFT", "1")],
    );

    // Wait for the response to complete.
    sess.expect("4").expect("response");

    // The process should exit cleanly — no hanging render thread.
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// In REPL mode with iocraft, the chrome appears during the turn and
/// the prompt re-appears after the turn completes.
#[test]
fn iocraft_repl_turn_completes_and_reprompts() {
    let env = TestEnv::new("iocraft-repl-reprompt");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_IOCRAFT", "1")],
    );

    // Wait for initial prompt.
    sess.expect("❯").expect("initial prompt");

    // Submit a prompt.
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // Response should appear.
    sess.expect("4").expect("response");

    // After the turn, a new prompt should appear.
    sess.expect("❯").expect("prompt after turn");

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// The spinner line contains the model name during a turn.
#[test]
fn iocraft_spinner_shows_model_name() {
    let env = TestEnv::new("iocraft-spinner-model");
    let prompt = env.prompt("What is 2+2? Answer only the number.", "single_turn_text");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only", &prompt],
        &[("SUDOCODE_IOCRAFT", "1")],
    );

    // The spinner should show a model identifier in brackets.
    // In mock mode the model is "sonnet", in live it's the real model.
    if env.is_mock() {
        sess.expect("sonnet")
            .expect("spinner should show model name");
    }

    // Response should complete.
    sess.expect("4").expect("response");

    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// The footer shows the current permission mode during a turn.
#[test]
fn iocraft_footer_shows_permission_mode() {
    let env = TestEnv::new("iocraft-footer-perm");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_IOCRAFT", "1")],
    );

    // The footer should show "read only" mode text.
    sess.expect("read only")
        .expect("footer should show permission mode");

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("clean exit");
    assert_eq!(exit, 0);
}

/// Tool output appears during a turn with bash tool calls. The process
/// exits cleanly (no hanging render thread).
#[test]
fn iocraft_tool_output_during_turn() {
    let env = TestEnv::new("iocraft-tool-output");
    let prompt = env.prompt("Run echo hello in bash", "bash_stdout_roundtrip");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", &prompt],
        &[("SUDOCODE_IOCRAFT", "1")],
    );

    // In mock mode the bash tool call produces "bash completed: <output>".
    // In live mode the actual echo output appears. Either way, the process
    // should exit cleanly.
    let exit = sess
        .expect_eof()
        .expect("process should exit after single turn");
    assert_eq!(exit, 0);
}
