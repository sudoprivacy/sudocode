//! PTY smoke test: async REPL is reachable and processes a single turn
//! end-to-end when `SUDOCODE_INTERRUPT_QUEUE_MODE=queue` is set.
//!
//! Guards the wiring landed in PR #297 against a
//! regression that would make the async dispatch panic on startup, deadlock
//! between the input-thread and runner thread, or fail to route a single
//! input through the coordinator's `submit_when_idle` path.
//!
//! ## Scope of this test
//!
//! - Env var flip → dispatch reaches `run_repl_iocraft_dispatch` (not the sync
//!   loop). Verified indirectly: the REPL prompt still renders + a single
//!   turn completes, i.e., the async path is at least as functional as sync
//!   for the idle-then-turn baseline.
//! - `/exit` shuts the coordinator loop cleanly (Exit branch joins the
//!   runner thread, drops the `LiveCli` mutex, telemetry emits).
//!
//! ## Explicitly deferred (needs follow-up scoping)
//!
//! The full "queue N inputs during a running turn → exactly one downstream
//! `/v1/messages` for the combined batch" assertion is not here. That test
//! requires either
//!  1. a mock scenario with a controlled per-request delay so the queue is
//!     provably still-open when the follow-ups are sent, or
//!  2. a live-only test that inspects `captured_message_count` after a slow
//!     real turn.
//!
//! Neither exists in `MockAnthropicService` today. Task #37 follow-up.

mod common;

use common::TestEnv;
use std::time::Duration;

/// Async REPL processes a single turn under `SUDOCODE_INTERRUPT_QUEUE_MODE=queue`,
/// then exits cleanly on `/exit`. Baseline smoke — proves the async dispatch is
/// reachable + not deadlocked + telemetry-completion runs.
#[test]
fn async_repl_processes_single_turn_and_exits() {
    let env = TestEnv::new("repl-async-queue-smoke");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );

    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "async input ready");

    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    let before = common::turn_status_marker(&sess);
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .expect("type prompt into async REPL");
    common::expect_input_line(
        &sess,
        &prompt,
        common::DEFAULT_TIMEOUT,
        "complete async prompt rendered before submission",
    );
    sess.send("\r").expect("submit async prompt");
    common::expect_turn_complete_after(
        &sess,
        &before,
        common::DEFAULT_TIMEOUT,
        "async turn completes before exit",
    );
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "async input restored");

    // A bare "4" can match a startup path or session ID before a request has
    // even been submitted. Assert the actual response and captured request.
    if env.is_mock() {
        common::expect_screen(
            &sess,
            |screen| screen.contains("The answer is 4"),
            common::DEFAULT_TIMEOUT,
            "async mock answer is rendered",
        );
        let requests = env.captured_message_bodies();
        assert_eq!(requests.len(), 1, "one async model request");
        assert!(requests[0].contains("PARITY_SCENARIO:single_turn_text"));
        assert!(
            !requests[0].contains("/exit"),
            "exit is not part of the prompt"
        );
    }

    sess.send("/exit").expect("type /exit");
    common::expect_input_line(
        &sess,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "exit command rendered before submission",
    );
    sess.send("\r").expect("submit /exit");

    // Generous timeout: CI runners (esp. macos-latest) are slower to reap
    // child processes than the local dev box. 30 s comfortably covers the
    // runner-thread join + persist_session + telemetry flush + process
    // teardown even on a congested runner.
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!(
            "async REPL should exit cleanly after /exit; got error: {e:?}\nPTY screen:\n{screen}"
        )
    });
    assert_eq!(exit, 0, "async REPL clean-exit expected 0; got {exit}");
}
