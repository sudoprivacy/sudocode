//! Visual regression tests for terminal output correctness.
//!
//! These tests verify that the REPL chrome (separator line, footer) renders
//! correctly and that no *post-readline* cursor-up sequences pollute output.
//!
//! The sandwich chrome (`print_before_prompt`) uses `ESC[2F` to position the
//! cursor on the prompt line — this is safe because positions are deterministic
//! (lines were just printed). The dangerous case was `replace_after_submit`
//! which cursored up AFTER readline returned (unpredictable → scrollback
//! jumping). That function has been permanently removed; these tests guard
//! against its reintroduction.
//!
//! ```bash
//! cargo test --test visual_regression                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test visual_regression  # real API
//! ```

mod common;

use common::TestEnv;

/// After the startup banner, a `─` separator character appears in the REPL
/// output before the prompt.
#[test]
fn separator_line_appears_after_startup() {
    let env = TestEnv::new("visual-separator");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("─")
        .expect("separator line should appear after startup banner");

    sess.expect("❯").expect("REPL prompt should appear");

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// REPL mode: the footer hint line with permission mode info should be
/// visible in the output stream.
#[test]
fn footer_hint_appears_in_repl() {
    let env = TestEnv::new("visual-footer");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("read only")
        .expect("footer should show permission mode");

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// After submitting input, the user's text must NOT appear twice (once in
/// the prompt `❯ text` and again as an echo `› text`). The old
/// `replace_after_submit` cursor-up was used to overwrite the prompt with
/// a styled echo; without it, printing an echo causes duplication.
/// The correct behavior is: prompt stays as-is, no echo printed.
#[test]
fn no_duplicate_echo_after_submit() {
    let env = TestEnv::new("visual-no-dup-echo");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("❯").expect("REPL prompt should appear");

    // Send /help — deterministic output, no LLM call needed.
    sess.send("/help\r").expect("send /help");

    // Wait for the separator that precedes the next prompt — this
    // confirms print_before_prompt ran and the full output is captured.
    let output = sess.expect("─").expect("next separator should appear");

    // The submitted text must NOT appear as a `›`-prefixed echo line.
    // If it does, the echo duplication bug has regressed.
    assert!(
        !output.contains("› /help"),
        "found echo line '› /help' — input was duplicated: {output:?}"
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// After submitting, the bottom separator and footer from the sandwich
/// chrome must be cleared. If `\x1b[J]` fails to clear them, the bottom
/// sep `─` would appear between the prompt and the command output.
/// We verify by sending `/version` (short output) and checking that the
/// output between the prompt echo and the version text has no stray `─`.
#[test]
fn bottom_chrome_cleared_after_submit() {
    let env = TestEnv::new("visual-bottom-clear");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("❯").expect("REPL prompt should appear");

    // Send /version — produces "Sudo Code\n  Version ..." with no `─`.
    sess.send("/version\r").expect("send /version");

    // Expect the version output.
    let output = sess
        .expect("Version")
        .expect("version output should appear");

    // Between prompt submission and version output, there must be no
    // stray bottom separator `─`. The only `─` should be from the TOP
    // separator printed by print_before_prompt BEFORE readline.
    // If `\x1b[J]` failed to clear the bottom sep, `─` would appear
    // between the prompt echo and "Version".
    let after_submit = output.split("/version").last().unwrap_or(&output);
    assert!(
        !after_submit.contains('─'),
        "bottom separator leaked after submit: {after_submit:?}"
    );

    // Wait for next prompt before exiting.
    sess.expect("❯").expect("next prompt");
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// Single-turn mode: output must not contain the dangerous `ESC[1F`
/// cursor-up that was used by the old `replace_after_submit` (the root
/// cause of scrollback jumping). `ESC[2F` in `print_before_prompt` is
/// safe and expected.
#[test]
fn no_post_readline_cursor_up_in_single_turn() {
    let env = TestEnv::new("visual-no-cursor-up");
    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    let matched = sess.expect("4").expect("response should contain '4'");
    // ESC[1F was used by replace_after_submit to go back up per-line.
    assert!(
        !matched.contains("\x1b[1F"),
        "output contains ESC[1F (replace_after_submit cursor-up): {matched:?}"
    );

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// REPL mode: after submitting input, no `ESC[1F` (the dangerous
/// per-line cursor-up from `replace_after_submit`) should appear. The
/// `ESC[2F` from `print_before_prompt` is safe and expected.
#[test]
fn no_post_readline_cursor_up_in_repl() {
    let env = TestEnv::new("visual-no-cursor-up-repl");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("❯").expect("REPL prompt should appear");

    sess.send("/help\r").expect("send /help");
    let help_output = sess.expect("❯").expect("next prompt after /help");
    assert!(
        !help_output.contains("\x1b[1F"),
        "post-submit output contains ESC[1F: {help_output:?}"
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}
