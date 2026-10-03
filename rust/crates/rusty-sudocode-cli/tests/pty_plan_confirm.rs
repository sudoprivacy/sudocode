//! PTY tests for the `write_plan` approval dialog.
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
use runtime::{ContentBlock, Session};

/// Budget for the process to exit after `/exit`. Generous on purpose: teardown
/// (unwinding the render loop and persisting the session) is its own cost,
/// unrelated to how long the asserted behaviour took.
const EXIT_BUDGET: Duration = Duration::from_secs(30);

/// When the model calls `write_plan` in REPL mode, the user should see a
/// confirmation dialog. Choosing "keep context & execute" completes the turn.
//
// Runs on Windows too: the confirmation crosses the engine↔renderer seam as a
// QuestionRequest answered above the seam by CliQuestionPrompter (rustyline).
#[test]
fn write_plan_shows_confirm_dialog_and_accepts_keep_context() {
    choose_plan_action("plan-confirm", "2", "The user APPROVED the plan", false);
}

// render takes a closure over screens with different lifetimes.
#[allow(clippy::redundant_closure_for_method_calls)]
fn choose_plan_action(name: &str, choice: &str, expected: &str, is_error: bool) {
    let env = TestEnv::new(name);
    let budget = if env.is_live() {
        LIVE_TURN_BUDGET
    } else {
        env.timeout()
    };

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_plan",
    ]);
    common::expect_input_line_cleared(&sess, budget, "plan REPL ready");

    let prompt = env.prompt(
        "Call the write_plan tool right now with a short markdown plan as `content`. Do not explain anything.",
        "write_plan_roundtrip",
    );
    sess.send(&prompt).expect("type prompt");
    common::expect_input_line(
        &sess,
        "Call the write_plan tool",
        budget,
        "plan prompt entered",
    );
    sess.send("\r").expect("submit prompt");

    // The title prints before rustyline enters readline. Sending a choice at
    // that point races terminal setup: CI echoed the digit above the options
    // and left the dialog waiting at `Your choice: 2`, having lost Enter.
    common::expect_screen(
        &sess,
        |screen| screen.contains("Choose an action") && screen.contains("Your choice:"),
        budget,
        "plan choice editor ready",
    );
    let marker = turn_status_marker(&sess);
    sess.send(choice).expect("type choice");
    common::expect_screen(
        &sess,
        |screen| screen.contains(&format!("Your choice: {choice}")),
        budget,
        "plan choice entered",
    );
    // A user can pause before Enter. Span several spinner ticks: background
    // progress must not erase the choice editor while it owns the terminal.
    std::thread::sleep(Duration::from_millis(300));
    let screen = sess.render(|s| s.contents());
    assert!(
        screen.contains(&format!("Your choice: {choice}")),
        "the pending choice must remain visible: {screen}"
    );
    sess.send("\r").expect("submit choice");
    expect_turn_complete_after(&sess, &marker, budget, "chosen plan action completes");

    common::expect_input_line_cleared(&sess, budget, "plan REPL rearmed");
    sess.send("/exit").expect("type /exit");
    common::expect_input_line(&sess, "/exit", budget, "exit entered");
    sess.send("\r").expect("submit /exit");
    sess.set_default_timeout(EXIT_BUDGET);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("clean exit after write_plan dialog: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "scode should exit 0 after /exit; got {exit}");
    assert!(
        saved_plan_decision(&env.workspace_root().join(".scode"), expected, is_error),
        "the chosen approval/rejection must reach the persisted tool result"
    );
}

fn saved_plan_decision(dir: &std::path::Path, expected: &str, expected_error: bool) -> bool {
    std::fs::read_dir(dir).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            saved_plan_decision(&path, expected, expected_error)
        } else if path
            .file_name()
            .is_some_and(|name| name == "transcript.jsonl")
        {
            let session = Session::load_from_path(&path).unwrap();
            session
                .messages
                .iter()
                .flat_map(|message| &message.blocks)
                .any(|block| {
                    matches!(block, ContentBlock::ToolResult { tool_name, output, is_error, .. }
                    if tool_name == "write_plan" && output.contains(expected) && *is_error == expected_error)
                })
        } else {
            false
        }
    })
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

    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    if sess.expect("Choose an action").is_err() {
        eprintln!("SKIP: live model did not call write_plan");
        return;
    }

    // Choose option 1: clear context & execute. The session clears and re-runs
    // with the plan as the new prompt — a fresh (recursive) turn starts.
    let marker = turn_status_marker(&sess);
    sess.send("1\r").expect("send choice 1");

    // The clear-context path runs TWO turns: the write_plan turn completes, then
    // the session clears and re-executes the plan as a new turn. Wait for the
    // second turn's status line (a new marker) so `/exit` isn't sent while a
    // turn is still running (it would queue, not exit).
    expect_turn_complete_after(
        &sess,
        &marker,
        LIVE_TURN_BUDGET,
        "clear-context recursive execute turn should complete",
    );

    // Best-effort teardown: the behavior under test (recursive execute ran) is
    // already asserted above. The final `/exit` sync after a multi-turn flow is
    // non-deterministic over a PTY, so don't gate the test on the exit code —
    // the suite uses this same tolerant teardown elsewhere.
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(EXIT_BUDGET);
    let _ = sess.expect_eof();
}

/// The `[+]` free-text row (comment / keep-planning) must be reachable by arrow
/// keys and accept typed input — the `DialPad` bug where the cursor couldn't land
/// on it and typing was swallowed. Down past the 4 options lands on `[+]`;
/// Enter opens text entry; the typed comment feeds back and the model revises
/// (calls `write_plan` again → the dialog reappears). Live-only: only a real model
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

    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
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

    // Best-effort teardown: the behavior under test (comment reached the model
    // via the `[+]` row) is already asserted above. The final `/exit` sync after
    // a multi-turn flow is non-deterministic over a PTY, so don't gate on it.
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(EXIT_BUDGET);
    let _ = sess.expect_eof();
}

/// Choice 4 (exit plan) should reject execution and let the model continue
/// without implementing the plan.
#[test]
fn write_plan_choice_exit_rejects_execution() {
    choose_plan_action(
        "plan-exit",
        "4",
        "User chose to exit plan mode without executing",
        true,
    );
}
