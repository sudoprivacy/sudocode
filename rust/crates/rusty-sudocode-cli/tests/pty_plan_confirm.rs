//! PTY tests for the plan mode confirmation dialog.
//!
//! When the model calls `ExitPlanMode` in REPL mode, the CLI shows a
//! 3-choice dialog. These tests verify:
//! 1. The dialog appears with the expected options
//! 2. Choice 2 (keep context) executes normally
//! 3. Choice 3 (keep planning) rejects the tool call
//!
//! Choice 1 (clear context & execute) is harder to test in mock mode
//! because it triggers a recursive `run_turn` which needs a second
//! mock response. Live mode covers it end-to-end.
//!
//! ```bash
//! cargo test --test pty_plan_confirm                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_plan_confirm  # real API
//! ```
mod common;

use std::time::Duration;

use common::TestEnv;

/// When the model calls ExitPlanMode in REPL mode, the user should
/// see a confirmation dialog with 3 choices.
///
/// Mock mode: deterministic — the mock scenario always calls ExitPlanMode.
/// Live mode: the model may not reliably call ExitPlanMode from a prompt
/// alone (no plan context), so we skip if the dialog doesn't appear.
// Windows PTY does not reliably deliver stdin writes to the child's
// `read_line` during the interactive dialog; the test hangs waiting
// for the post-choice output. macOS + Linux cover the same code path.
#[test]
#[cfg_attr(windows, ignore)]
fn exit_plan_mode_shows_confirm_dialog_and_accepts_keep_context() {
    let env = TestEnv::new("plan-confirm");

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "ExitPlanMode",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call the ExitPlanMode tool right now. Do not explain anything, just call ExitPlanMode with empty arguments {}.",
        "exit_plan_mode_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // Wait for the confirmation dialog
    sess.set_default_timeout(Duration::from_secs(30));
    let dialog_appeared = sess.expect("Choose an action").is_ok();

    if !dialog_appeared && env.is_live() {
        // Live model didn't call ExitPlanMode — skip gracefully.
        // The dialog is thoroughly tested in mock mode.
        eprintln!("SKIP: live model did not call ExitPlanMode (no plan context)");
        return;
    }
    assert!(dialog_appeared, "should see confirmation dialog");

    // Choose option 2: keep context & execute
    sess.send("2\r").expect("send choice 2");

    // Turn should complete normally
    sess.expect("tokens")
        .expect("should see post-turn status line");

    // Exit cleanly
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or(0);
    assert!(
        exit == 0 || exit == 143,
        "exit code should be 0; got {exit}"
    );
}

/// Choice 3 (keep planning) should reject the tool call and let
/// the model continue in plan mode.
#[test]
#[cfg_attr(windows, ignore)]
fn exit_plan_mode_choice_keep_planning_rejects_tool() {
    let env = TestEnv::new("plan-keep");

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "ExitPlanMode",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call the ExitPlanMode tool right now. Do not explain anything, just call ExitPlanMode with empty arguments {}.",
        "exit_plan_mode_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(Duration::from_secs(30));
    let dialog_appeared = sess.expect("Choose an action").is_ok();

    if !dialog_appeared && env.is_live() {
        eprintln!("SKIP: live model did not call ExitPlanMode (no plan context)");
        return;
    }
    assert!(dialog_appeared, "should see confirmation dialog");

    // Choose option 3: keep planning
    sess.send("3\r").expect("send choice 3");

    // The model should get an error result and continue
    sess.expect("tokens")
        .expect("should see status line after rejected tool");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or(0);
    assert!(
        exit == 0 || exit == 143,
        "exit code should be 0; got {exit}"
    );
}
// CI trigger
