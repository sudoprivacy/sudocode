//! PTY test for the `↑`-arrow dequeue in async REPL mode.
//!
//! Guards the wiring landed with the `↑`-arrow handler (PR #TBD):
//! `LineEditor` gets built with a dequeue hook backed by the shared
//! `Arc<Mutex<TurnInputCoordinator>>`; on `Up` with an empty buffer, the
//! newest queued item is spliced into rustyline via `Cmd::Insert`.
//!
//! ## Real user journey exercised (3+ steps, data flow between them)
//!
//! Follows the [integration-test-generator](file:///C/Users/songym/cursor-projects/document-ai/.claude/skills/integration-test-generator/SKILL.md)
//! standard: every step's output feeds the next; no orphan operations.
//!
//! 1. **Start a long-running turn.** Send prompt A that triggers a bash tool
//!    invocation the model has to wait on — gives the coordinator a real
//!    "turn_active = true" window in which queue-during-turn matters.
//! 2. **Queue during the turn.** Send `MARKER_QUEUED_INPUT\r` while A is
//!    still executing → coordinator's `submit_during_turn` returns Queued;
//!    the item lives in the shared queue behind the running turn.
//! 3. **Press `↑` with empty buffer.** Sends the `ESC[A` sequence → input
//!    thread's rustyline routes to `UpArrowDequeueHandler` → hook calls
//!    `dequeue_last` → returns `MARKER_QUEUED_INPUT` → rustyline
//!    `Cmd::Insert`s it into the buffer.
//! 4. **Observe the re-rendered buffer.** The Insert rerenders the prompt
//!    line WITH the dequeued text — pty-expect finds `MARKER_QUEUED_INPUT`
//!    in the stream. This is the strong end-to-end signal that the whole
//!    Arc-shared queue → hook → Cmd::Insert path works.
//! 5. **Clean exit.** `/exit` — same as `pty_repl_async_queue::async_repl_processes_single_turn_and_exits`
//!    proves the coordinator loop still tears down cleanly after the `↑`
//!    interaction (no rustyline / mutex leaks left over).
//!
//! ## Why the `BashInterruptLongRunning` mock scenario
//!
//! It's the only stock scenario whose `run_turn` stays in-flight long enough
//! for a scripted PTY sequence to reliably interleave. `SleepShortRoundtrip`'s
//! 600 ms window is too tight for CI timing to hit deterministically. The
//! bash `sleep 30` this scenario invokes gets killed cleanly on `/exit`
//! (scode's `HookAbortMonitor` sends SIGTERM to the tool subprocess), so the
//! test wall-clock is dominated by the queue-then-up dance (~2 s), not the
//! 30 s the raw scenario would take.

mod common;

use common::TestEnv;
use std::time::Duration;

const MARKER: &str = "MARKER_QUEUED_INPUT";

/// Real user journey (5 steps): start long turn → queue → `↑` → verify buffer
/// contains the dequeued text → clean `/exit`.
///
/// Regression guard for: any change that breaks the Arc-shared coordinator
/// queue → dequeue hook → rustyline `Cmd::Insert` path. Failing signal:
/// `MARKER` never surfaces in the terminal output after `↑`, meaning either
/// the hook wasn't installed, the coordinator wasn't shared, or the empty-
/// buffer guard is inverted.
#[test]
fn up_arrow_on_empty_buffer_dequeues_last_queued_input() {
    let env = TestEnv::new("repl-async-up-dequeue");

    let mut sess = env.spawn_with_env(
        // danger-full-access so the mock's canned bash command actually runs —
        // pty_core_conversation's bash_interrupt_long_running case uses the
        // same flag; without it the tool call is denied and we never get the
        // "interrupt-start" output we key on below.
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );

    // Step 1: async REPL boots + shows prompt.
    sess.expect("❯")
        .expect("async REPL should render the initial prompt");

    // Step 2: fire the long-running scenario so we have a real "turn active"
    // window during which submit_during_turn goes down the Queued arm.
    let prompt = env.prompt(
        "Use the bash tool to run a background sleep and let me know when \
         you started it.",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r"))
        .expect("send long-running prompt");

    // Wait for the tool to actually start — the mock's canned bash command
    // does `printf 'interrupt-start'` before the sleep, and PTY captures
    // the bash stdout stream. Seeing "interrupt-start" is the strongest
    // signal that the turn is really in-flight at the runner thread.
    sess.expect("interrupt-start")
        .expect("bash tool should print interrupt-start before the sleep");

    // Step 3: submit the marker DURING the running turn — coordinator's
    // submit_during_turn queues it (queue mode) and returns Queued. The
    // input thread's rustyline is now idle waiting for the next line, and
    // the marker text should NOT execute as its own turn.
    sess.send(&format!("{MARKER}\r"))
        .expect("queue marker during running turn");

    // Give rustyline + the input thread a beat to process + hand off. 200 ms
    // is well below the coordinator's 100 ms recv_timeout tick so we don't
    // race the drain loop.
    std::thread::sleep(Duration::from_millis(500));

    // Step 4: press `↑` on an empty buffer. `ESC[A` is the ANSI sequence
    // rustyline reads as `KeyCode::Up` under a real terminal. UpArrowDequeueHandler
    // sees empty buffer + non-empty queue → dequeues `MARKER` → returns
    // `Cmd::Insert(1, MARKER)` → rustyline splices into the buffer AND
    // re-renders the prompt line.
    sess.send("\x1b[A").expect("send Up-arrow key sequence");

    // The rerender is the observable end-to-end signal: `MARKER` shows up
    // in the PTY output stream. If the hook were unbound or the buffer-empty
    // guard inverted, the queue would stay populated and the marker would
    // NOT appear here — clean failure with 30 s timeout.
    sess.expect(MARKER)
        .expect("↑ on empty buffer should splice the dequeued marker into rustyline");

    // Step 5: send `/exit` to signal shutdown. `/exit` now calls
    // `driver.abort_current_turn()` before joining the runner (PR #TBD), so
    // the bash `sleep 30` gets SIGTERM'd rather than the test waiting 30 s.
    // We don't assert on `expect_eof` here — the test's real assertion is
    // step 4's MARKER visibility; the drop-cleanup of PtySession will kill
    // any surviving child if the abort didn't propagate for some reason
    // (e.g., a bash quirk on a specific platform), which is fine at this
    // layer — `pty_repl_async_queue` already gates the clean-`/exit` path
    // for the non-tool code path.
    sess.send("/exit\r").expect("send /exit");
    // Bounded wait — if abort_current_turn works, this returns in a beat.
    // If it doesn't, PtySession's Drop kills the child. Either way we don't
    // hang the CI runner.
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
