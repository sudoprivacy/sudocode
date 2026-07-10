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

    let prompt = env.prompt("Please reply with the single word: ready", "single_turn_text");
    sess.send(&format!("{prompt}\r"))
        .expect("send prompt into async REPL");

    // The coordinator's `submit_when_idle` path spawns the runner thread which
    // calls `LiveCli::run_turn`; on completion, `TurnEvent::Done` re-enters the
    // main select loop and rustyline re-renders the prompt for the next turn.
    // Seeing the prompt AGAIN is the strongest signal that the whole cycle
    // (input → runner → turn-done → back to select) completed without leak.
    sess.expect("❯")
        .expect("async REPL should re-render prompt after turn end");

    sess.send("/exit\r").expect("send /exit");

    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        panic!("async REPL should exit cleanly after /exit; got error: {e:?}")
    });
    assert_eq!(exit, 0, "async REPL clean-exit expected 0; got {exit}");
}
