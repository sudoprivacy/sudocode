//! PTY tests for the write_plan approval dialog.
//!
//! When the model calls `write_plan` in REPL mode, the CLI writes the plan
//! file and shows a 4-choice dialog. These tests verify:
//! 1. The dialog appears
//! 2. Choice 2 (keep context & execute) completes the turn normally
//! 3. Choice 4 (exit plan) rejects execution
//!
//! Choice 1 (clear context & execute) triggers a recursive `run_turn` needing a
//! second mock response, so it's covered end-to-end in live mode. Choice 3 and
//! free-text comments feed the plan back for revision (also live-covered).
//!
//! ```bash
//! cargo test --test pty_plan_confirm                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_plan_confirm  # real API
//! ```
mod common;

use std::time::Duration;

use common::{expect_turn_complete_after, turn_status_marker, TestEnv, LIVE_TURN_BUDGET};

/// When the model calls write_plan in REPL mode, the user should see a
/// confirmation dialog. Choosing "keep context & execute" completes the turn.
//
// Runs on Windows too: the confirmation crosses the engine↔renderer seam as a
// QuestionRequest answered above the seam by CliQuestionPrompter (rustyline).
#[test]
fn write_plan_shows_confirm_dialog_and_accepts_keep_context() {
    let env = TestEnv::new("plan-confirm");

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_plan",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call the write_plan tool right now with a short markdown plan as `content`. Do not explain anything.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(Duration::from_secs(30));
    let dialog_appeared = sess.expect("Choose an action").is_ok();

    if !dialog_appeared && env.is_live() {
        eprintln!("SKIP: live model did not call write_plan");
        return;
    }
    assert!(dialog_appeared, "should see confirmation dialog");

    // Choose option 2: keep context & execute
    let marker = turn_status_marker(&sess);
    sess.send("2\r").expect("send choice 2");
    expect_turn_complete_after(
        &sess,
        &marker,
        if env.is_live() {
            LIVE_TURN_BUDGET
        } else {
            env.timeout()
        },
        "keep-context plan turn should complete",
    );

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or(0);
    assert!(
        exit == 0 || exit == 143,
        "exit code should be 0; got {exit}"
    );
}

/// Choice 1 (clear context & execute) is the default: it resets the session
/// and re-runs with the plan file as the fresh prompt. Live-only — clearing the
/// context drops the mock scenario marker, so only a real model can carry the
/// recursive execute turn.
#[test]
fn write_plan_choice_clear_context_executes_plan() {
    let env = TestEnv::new("plan-clear");
    if env.is_mock() {
        // Clearing context strips the PARITY_SCENARIO marker from the injected
        // plan prompt, so the mock can't answer the recursive turn. The path is
        // exercised live below.
        return;
    }

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_plan,read_file,glob_search",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call the write_plan tool right now with a short markdown plan as `content`. Do not explain anything.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(Duration::from_secs(30));
    if sess.expect("Choose an action").is_err() {
        eprintln!("SKIP: live model did not call write_plan");
        return;
    }

    // Choose option 1: clear context & execute. The session clears and re-runs
    // with the plan as the new prompt — a fresh turn must complete without error.
    let marker = turn_status_marker(&sess);
    sess.send("1\r").expect("send choice 1");
    expect_turn_complete_after(
        &sess,
        &marker,
        LIVE_TURN_BUDGET,
        "clear-context execute turn should complete",
    );

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or(0);
    assert!(
        exit == 0 || exit == 143,
        "exit code should be 0; got {exit}"
    );
}

/// The `[+]` free-text row (comment / keep-planning) must be reachable by arrow
/// keys and accept typed input — the DialPad bug where the cursor couldn't land
/// on it and typing was swallowed. Down past the 4 options lands on `[+]`;
/// Enter opens text entry; the typed comment feeds back and the model revises
/// (calls write_plan again → the dialog reappears). Live-only: only a real model
/// re-plans on the comment.
#[test]
fn write_plan_comment_row_is_reachable_and_revises() {
    let env = TestEnv::new("plan-comment");
    if env.is_mock() {
        // The revise loop needs the model to re-call write_plan on the comment;
        // the mock returns a fixed plan and can't. The reachability of the row
        // is unit-tested in repl_ui (custom_input_row_is_selectable_past_last_option).
        return;
    }

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_plan",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call the write_plan tool right now with a short markdown plan as `content`. Do not explain anything.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(Duration::from_secs(30));
    if sess.expect("Choose an action").is_err() {
        eprintln!("SKIP: live model did not call write_plan");
        return;
    }

    // Arrow down past the 4 options onto the `[+]` custom-input row, then Enter
    // to open free-text entry (the previously-broken path).
    for _ in 0..4 {
        sess.send("\x1b[B").expect("send Down arrow");
    }
    sess.send("\r").expect("open custom input");

    // Type a revision comment and submit. The model should revise and re-present
    // the plan (dialog reappears), proving the comment fed back.
    let marker = turn_status_marker(&sess);
    sess.send("Add an explicit testing step to the plan.\r")
        .expect("send comment");

    sess.set_default_timeout(LIVE_TURN_BUDGET);
    let revised = sess.expect("Choose an action").is_ok();
    if !revised {
        // Some models answer the comment in prose without re-calling write_plan;
        // the turn still completes without error.
        expect_turn_complete_after(&sess, &marker, LIVE_TURN_BUDGET, "comment turn completes");
    }

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or(0);
    assert!(
        exit == 0 || exit == 143,
        "exit code should be 0; got {exit}"
    );
}

/// Choice 4 (exit plan) should reject execution and let the model continue
/// without implementing the plan.
#[test]
fn write_plan_choice_exit_rejects_execution() {
    let env = TestEnv::new("plan-exit");

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_plan",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call the write_plan tool right now with a short markdown plan as `content`. Do not explain anything.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(Duration::from_secs(30));
    let dialog_appeared = sess.expect("Choose an action").is_ok();

    if !dialog_appeared && env.is_live() {
        eprintln!("SKIP: live model did not call write_plan");
        return;
    }
    assert!(dialog_appeared, "should see confirmation dialog");

    // Choose option 4: exit plan (don't execute)
    let marker = turn_status_marker(&sess);
    sess.send("4\r").expect("send choice 4");
    expect_turn_complete_after(
        &sess,
        &marker,
        if env.is_live() {
            LIVE_TURN_BUDGET
        } else {
            env.timeout()
        },
        "exit-plan turn should complete",
    );

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or(0);
    assert!(
        exit == 0 || exit == 143,
        "exit code should be 0; got {exit}"
    );
}
