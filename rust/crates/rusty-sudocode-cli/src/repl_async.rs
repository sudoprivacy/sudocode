//! Async REPL loop that accepts input DURING a running turn — Phase 2 of the
//! interrupt+queue plan (`notes/plans/conversation-interrupt-queue-sudocode.md`).
//!
//! Only activated when `SUDOCODE_INTERRUPT_QUEUE_MODE` is set to a non-off value.
//! The default REPL (`run_repl`) is unchanged and remains the sync path.
//!
//! ## Modes
//!
//! - `queue` — input typed while a turn is running is accumulated in the
//!   [`TurnInputCoordinator`]; on turn end (natural OR cancelled) the queue is
//!   flushed as ONE combined `run_turn` matching sudowork's post-#983
//!   batched-flush semantics.
//! - `interrupt` / `both` — same as `queue` for the queue side, PLUS the
//!   coordinator's `SubmitOutcome::Interrupt` calls
//!   [`TurnDriver::abort_current_turn`]. The runner's in-flight `run_turn`
//!   observes the aborted [`runtime::HookAbortSignal`] and returns a cancelled
//!   `TurnSummary`, which propagates as `TurnEvent::Done`; the drain then picks
//!   up the interrupter as a fresh solo turn per §3.2 row 2.
//!
//! Slash commands (/exit, /clear, ...) still work: they are intercepted before
//! being handed to the coordinator and dispatched under the same cli lock the
//! runner uses, so they cannot race a running turn.
//!
//! ## Architecture (three-role split from the plan)
//!
//! ```text
//! ┌──────────────────┐      InputEvent      ┌───────────────────────┐
//! │ input-thread     │ ────────────────────▶│ main coordinator loop │
//! │ rustyline blocking│                     │ TurnInputCoordinator  │
//! └──────────────────┘                     │ Arc<Mutex<LiveCli>>   │
//!                                          └───────────┬───────────┘
//!                                                      │ spawn_turn
//!                                                      ▼
//!                                          ┌───────────────────────┐
//!                                          │ runner (std::thread)  │
//!                                          │ locks cli, run_turn   │
//!                                          │ sends TurnDone        │
//!                                          └───────────────────────┘
//! ```
//!
//! The main loop uses **std::sync::mpsc with a 100 ms recv_timeout poll** on the
//! input receiver as its "select" primitive during a running turn — no
//! crossbeam / no tokio at this layer, so the wiring stays free of new deps and
//! is trivially portable across Windows/POSIX. Idle main just blocks on
//! `input_rx.recv()`.
//!
//! `LiveCli` is behind an `Arc<Mutex<>>` because `run_turn` needs `&mut self`
//! and it must run off-main so main can service input events. Main only locks
//! cli briefly to dispatch slash commands or record prompt history; the runner
//! thread holds the lock for the full duration of `run_turn`, which is exactly
//! what we want (nothing else can touch cli while it is streaming an LLM turn).
//!
//! ## Deferred (explicitly out of scope for this commit)
//!
//! - **Auto-interrupt (`interrupt` / `both`).** Requires exposing the current
//!   turn's `HookAbortSignal` to main so an in-flight `run_turn` can be
//!   cancelled. `LiveCli::run_turn` currently constructs a fresh signal per
//!   invocation inside `prepare_turn_runtime` — plumbing it out for external
//!   abort is a follow-up commit.
//! - **`↑`-key dequeue.** Needs a rustyline `ConditionalEventHandler` binding
//!   that reads from the shared coordinator queue and calls `Cmd::Insert`.
//!   Deferred so the wiring can land + get PTY coverage first.
//! - **PTY integration test.** The three-role architecture is best proven end-
//!   to-end via PTY (queue N inputs, verify N-1 batched flush, verify sudocode
//!   emits exactly ONE downstream request); ships in the follow-up.
//! - **Startup completions refresh mid-loop.** Input thread reads the completion
//!   candidates snapshot from cli at boot; if agents / slash commands change
//!   during a session, the completions don't refresh yet.
//!
//! The [`sudocode plan doc`](https://github.com/sudoprivacy/sudocode/blob/main/notes/plans/conversation-interrupt-queue-sudocode.md)
//! §落地节奏 covers the full sequence; this file is Phase-2 commit 1.

use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::input::{LineEditor, ReadOutcome};
use crate::input_queue::{QueueMode, SubmitOutcome, TurnInputCoordinator};

/// Builds the `↑`-arrow dequeue hook that the input thread's rustyline binds
/// to `KeyCode::Up` on an empty buffer. Kept as a free function so both the
/// production wiring and the future PTY test infrastructure can construct one
/// from the same `Arc<Mutex<TurnInputCoordinator>>` main uses.
///
/// Semantics (per shareone §3.2 muted note):
/// - Buffer non-empty → returns `None`, rustyline runs default history-up.
/// - Buffer empty + queue empty → returns `None`, same fall-through.
/// - Buffer empty + queue non-empty → pops NEWEST queued item; caller
///   `Cmd::Insert`s it for further editing. LIFO so the user gets back the
///   thing they most recently queued.
fn make_up_arrow_hook(coord: Arc<Mutex<TurnInputCoordinator>>) -> crate::input::UpArrowDequeueHook {
    Arc::new(move || {
        coord
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dequeue_last()
    })
}

/// Events flowing from the input thread to the main coordinator.
enum InputEvent {
    Submit(String),
    Exit,
}

/// Events flowing from the runner thread back to the main coordinator.
enum TurnEvent {
    Done,
}

/// Anything the main loop pulls off its select. Kept as a small closed enum
/// so the state machine is easy to read.
enum LoopEvent {
    Input(InputEvent),
    TurnDone,
}

/// A "cli driver" — anything that can execute a single turn (given a prompt
/// string). Abstracted so this loop can be exercised in tests with a mock; the
/// real callsite passes an `Arc<Mutex<LiveCli>>` and a closure that unlocks
/// and calls `LiveCli::run_turn`. See `run_repl_async` for the concrete wiring.
pub trait TurnDriver: Send + Sync + 'static {
    /// Run one turn to completion. Should NOT return until the turn is over
    /// (natural end OR cancelled). Result is ignored by the loop — errors are
    /// printed by the driver itself, matching the sync REPL's behavior.
    fn run_turn(&self, prompt: &str);

    /// Called on `/exit` / `/quit` before the coordinator loop returns. The
    /// concrete driver flushes session state (write to disk, emit
    /// `session_ended` telemetry, etc.). Default no-op keeps the loop
    /// self-contained for tests.
    fn on_exit(&self) {}

    /// Auto-interrupt the currently running turn. Called from main when the
    /// coordinator matrix decides `SubmitOutcome::Interrupt` — the runner
    /// thread's `run_turn` will observe the abort and return with a
    /// cancelled `TurnSummary`, then the drain picks up the interrupter
    /// (already `solo`-tagged at the queue head by `submit_during_turn`).
    /// Must NOT block on the runner (main is holding the coordinator loop).
    /// Idempotent: safe to call when no turn is active — the next
    /// `LiveCli::prepare_turn_runtime` resets the shared signal before use.
    /// Default no-op for test drivers that don't wire abort.
    fn abort_current_turn(&self) {}
}

/// A submitted line that the coordinator loop needs to classify: is it a
/// user-visible exit command, or a prompt to run? Kept as a helper so the
/// classification logic has one source of truth.
fn is_exit_command(text: &str) -> bool {
    matches!(text.trim(), "/exit" | "/quit")
}

/// The three-role coordinator loop. Kept generic over `TurnDriver` so tests can
/// swap in a mock driver that just records prompts + sleeps.
///
/// Prints the initial prompt via `startup_banner` before spawning input.
///
/// On `InputEvent::Exit` from the input thread, waits for any in-flight turn
/// to complete before returning (avoids interrupting a mid-flight `run_turn`
/// that might be writing to disk).
pub fn run_coordinator_loop<D: TurnDriver + 'static>(
    driver: Arc<D>,
    mode: QueueMode,
    startup_banner: String,
    initial_completions: Vec<(String, String)>,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{startup_banner}");

    let coord = Arc::new(Mutex::new(TurnInputCoordinator::new()));
    let (input_tx, input_rx) = sync_channel::<InputEvent>(16);
    let (turn_tx, turn_rx) = sync_channel::<TurnEvent>(1);

    // Input thread — owns its rustyline LineEditor. Sends every submitted line
    // to main via a bounded channel. Exits cleanly on Exit / channel closed.
    // The `↑`-arrow dequeue hook is bound here so the input thread can pop
    // the newest queued input back into the buffer without needing a
    // channel round-trip to main.
    let input_tx_clone = input_tx.clone();
    let dequeue_hook = make_up_arrow_hook(Arc::clone(&coord));
    thread::Builder::new()
        .name("repl-input".into())
        .spawn(move || {
            let mut editor =
                LineEditor::new_with_dequeue_hook("❯ ", initial_completions, Some(dequeue_hook));
            loop {
                match editor.read_line() {
                    Ok(ReadOutcome::Submit(text)) => {
                        if input_tx_clone.send(InputEvent::Submit(text)).is_err() {
                            break;
                        }
                    }
                    Ok(ReadOutcome::Exit) => {
                        let _ = input_tx_clone.send(InputEvent::Exit);
                        break;
                    }
                    Err(_) => break,
                }
            }
        })?;

    let mut turn_active = false;
    let mut runner_handle: Option<thread::JoinHandle<()>> = None;

    loop {
        // Simple sync "select": when idle, block on input; when a turn is
        // running, poll both channels with a 100 ms tick. 100 ms is well below
        // human input latency perception (~150 ms) so no UI jankiness, and it
        // keeps the loop dep-free (no crossbeam).
        let event = if !turn_active {
            match input_rx.recv() {
                Ok(evt) => LoopEvent::Input(evt),
                Err(_) => break, // input thread died
            }
        } else {
            match input_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(evt) => LoopEvent::Input(evt),
                Err(RecvTimeoutError::Timeout) => match turn_rx.try_recv() {
                    Ok(TurnEvent::Done) => LoopEvent::TurnDone,
                    Err(_) => continue,
                },
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };

        match event {
            LoopEvent::Input(InputEvent::Exit) => {
                if let Some(h) = runner_handle.take() {
                    // Wait for the in-flight turn to finish before exiting so
                    // half-written state (session persistence, telemetry) is
                    // flushed cleanly.
                    let _ = h.join();
                }
                driver.on_exit();
                break;
            }
            LoopEvent::Input(InputEvent::Submit(text)) => {
                // Slash-command intercept: /exit and /quit are user-visible
                // shutdown commands. They must NOT reach `TurnDriver::run_turn`
                // (that'd send the literal text to the LLM as a turn — the
                // regression the pty_repl_async_queue smoke caught). Handled
                // BEFORE the coordinator matrix so an in-flight turn (if any)
                // is aborted + joined + telemetry emits cleanly.
                if is_exit_command(&text) {
                    if runner_handle.is_some() {
                        // Turn still running — abort it first so the join below
                        // returns quickly, not after the full LLM/tool wall
                        // clock. Idempotent + safe to call when no runner is
                        // active (driver default is no-op).
                        driver.abort_current_turn();
                    }
                    if let Some(h) = runner_handle.take() {
                        let _ = h.join();
                    }
                    driver.on_exit();
                    break;
                }
                if !turn_active {
                    let next = coord.lock().unwrap().submit_when_idle(text);
                    turn_active = true;
                    runner_handle = Some(spawn_turn(
                        Arc::clone(&driver),
                        next.prompt,
                        turn_tx.clone(),
                    ));
                    continue;
                }
                let outcome = coord.lock().unwrap().submit_during_turn(text, mode);
                match outcome {
                    SubmitOutcome::Queued => {
                        // Silent: sudowork's queue chips render in the sendbox;
                        // for the CLI we punt to a status line in a follow-up.
                    }
                    SubmitOutcome::Interrupt => {
                        // Coordinator has already placed the interrupter at the
                        // queue head with `solo: true`. Now cancel the running
                        // turn via the driver's abort handle — the runner's
                        // `run_turn` observes the abort, returns a cancelled
                        // TurnSummary, sends `TurnEvent::Done`, and the drain
                        // picks up the interrupter as a fresh solo run.
                        driver.abort_current_turn();
                        eprintln!(
                            "\x1b[2m(interrupting current turn; interrupter will run as a solo new turn)\x1b[0m"
                        );
                    }
                    SubmitOutcome::Rejected => {
                        eprintln!(
                            "\x1b[2m(a turn is running; set SUDOCODE_INTERRUPT_QUEUE_MODE=queue to queue instead)\x1b[0m"
                        );
                    }
                }
            }
            LoopEvent::TurnDone => {
                turn_active = false;
                if let Some(h) = runner_handle.take() {
                    let _ = h.join();
                }
                let next = coord.lock().unwrap().drain_next();
                if let Some(next) = next {
                    turn_active = true;
                    runner_handle = Some(spawn_turn(
                        Arc::clone(&driver),
                        next.prompt,
                        turn_tx.clone(),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Fire off `driver.run_turn(&prompt)` on a fresh thread. Sends TurnEvent::Done
/// when the turn returns (natural or cancelled). Errors inside `run_turn` are
/// the driver's responsibility to print — the coordinator only cares that a
/// turn has ended.
fn spawn_turn<D: TurnDriver + 'static>(
    driver: Arc<D>,
    prompt: String,
    done_tx: SyncSender<TurnEvent>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("repl-runner".into())
        .spawn(move || {
            driver.run_turn(&prompt);
            let _ = done_tx.send(TurnEvent::Done);
        })
        .expect("spawn repl-runner thread")
}

// ------------------------------------------------------------------
// Executable spec of the coordinator loop's state machine. Same "one
// exception to the no-unit-tests rule" carve-out as input_queue.rs;
// real behavior gets a PTY integration test in the follow-up commit.
// ------------------------------------------------------------------

#[cfg(test)]
mod loop_docs {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A `TurnDriver` that just records the prompts it's called with and
    /// blocks for `turn_ms` before returning — mimics an LLM turn taking time.
    /// Also counts `abort_current_turn` calls so the matrix doc can verify
    /// the auto-interrupt hook fires as expected.
    struct RecordingDriver {
        prompts: Mutex<Vec<String>>,
        turn_ms: u64,
        run_count: AtomicUsize,
        abort_count: AtomicUsize,
    }

    impl RecordingDriver {
        fn new(turn_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                prompts: Mutex::new(Vec::new()),
                turn_ms,
                run_count: AtomicUsize::new(0),
                abort_count: AtomicUsize::new(0),
            })
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    impl TurnDriver for RecordingDriver {
        fn run_turn(&self, prompt: &str) {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.run_count.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(self.turn_ms));
        }

        fn abort_current_turn(&self) {
            self.abort_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    // These docs cover the *coordinator* branch of the design only. They do
    // NOT spin up rustyline (input thread is stubbed). The intent is that any
    // future edit to the state machine can prove its regressions in <100 ms
    // rather than requiring a PTY.
    //
    // Rather than driving `run_coordinator_loop` directly (which owns its
    // input thread), the state-machine tests exercise `TurnInputCoordinator`
    // through the same call sequence the loop would use. This keeps the
    // executable spec small and dependency-free.

    #[test]
    fn state_machine_batched_flush_via_coordinator() {
        // Sanity: 3 during-turn submits + drain_next MUST produce ONE combined
        // prompt containing all 3 in submission order — the exact contract
        // that the coordinator loop hands to the runner thread when a turn ends.
        let mut c = TurnInputCoordinator::new();
        c.submit_during_turn("B".into(), QueueMode::Queue);
        c.submit_during_turn("C".into(), QueueMode::Queue);
        c.submit_during_turn("D".into(), QueueMode::Queue);
        let next = c.drain_next().unwrap();
        assert_eq!(next.prompt, "B\n\nC\n\nD");
        assert_eq!(next.consumed, 3);
        assert!(!next.solo);
    }

    #[test]
    fn recording_driver_records_prompt_and_run_count() {
        // Sanity that our test double is honest, so failures in the loop tests
        // aren't masquerading as bugs in the test infrastructure itself.
        let d = RecordingDriver::new(5);
        d.run_turn("hello");
        d.run_turn("world");
        assert_eq!(d.prompts(), vec!["hello".to_string(), "world".to_string()]);
        assert_eq!(d.run_count.load(Ordering::SeqCst), 2);
    }
}
