//! PTY smoke test: async REPL is reachable and processes a single turn
//! end-to-end when `SUDOCODE_INTERRUPT_QUEUE_MODE=queue` is set.
//!
//! Guards the wiring landed in PR #297 (`src/repl_async.rs`) against a
//! regression that would make the async dispatch panic on startup, deadlock
//! between the input-thread and runner thread, or fail to route a single
//! input through the coordinator's `submit_when_idle` path.
//!
//! ## Scope of this test
//!
//! - Env var flip → dispatch reaches `run_repl_async_dispatch` (not the sync
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

    // Startup: the async dispatcher prints the same startup banner as the sync
    // path (via `run_coordinator_loop`) + the input thread's rustyline renders
    // the `❯` prompt. If either dropped, we'd see a hang / EOF here.
    sess.expect("❯").expect("async REPL should render prompt");

    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    sess.send(&format!("{prompt}\r"))
        .expect("send prompt into async REPL");

    // Wait for the LLM to actually respond. The mock's `single_turn_text`
    // scenario emits "The answer is 4"; the `4` proves the turn *ran* through
    // the runner thread. We deliberately do NOT wait for a second `❯` prompt
    // afterwards — the input thread's rustyline shares stdout with the runner
    // thread's LLM stream, so the "next prompt" glyph can end up interleaved
    // with LLM output in ways pty-expect's forward-only search doesn't
    // reliably re-locate. The strong signal of "turn actually ran" is the
    // "4"; the strong signal of "loop is still alive after turn end" is
    // that `/exit` below causes a clean exit rather than a hang.
    sess.expect("4")
        .expect("async REPL should stream the LLM answer through the runner thread");

    sess.send("/exit\r").expect("send /exit");

    // Generous timeout: CI runners (esp. macos-latest) are slower to reap
    // child processes than the local dev box. 30 s comfortably covers the
    // runner-thread join + persist_session + telemetry flush + process
    // teardown even on a congested runner.
    sess.set_default_timeout(Duration::from_secs(30));
    let exit = sess
        .expect_eof()
        .unwrap_or_else(|e| panic!("async REPL should exit cleanly after /exit; got error: {e:?}"));
    assert_eq!(exit, 0, "async REPL clean-exit expected 0; got {exit}");
}
