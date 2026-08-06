//! Async REPL loop that accepts input DURING a running turn — Phase 2 of the
//! interrupt+queue plan (`notes/plans/conversation-interrupt-queue-sudocode.md`).
//!
//! Active by default (`QueueMode::Queue`). Set `SUDOCODE_INTERRUPT_QUEUE_MODE=off`
//! to fall back to the sync REPL.
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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::input::{EscAbortHook, LineEditor, ReadOutcome};
use crate::input_queue::{QueueMode, SubmitOutcome, TurnInputCoordinator};
use crate::render::CliOutput;

/// Shared queue mode that can be toggled at runtime via `/config set`.
pub type SharedQueueMode = Arc<AtomicU8>;

/// Create a shared queue mode from the initial value.
#[must_use]
pub fn shared_queue_mode(mode: QueueMode) -> SharedQueueMode {
    Arc::new(AtomicU8::new(mode.to_u8()))
}

/// Read the current queue mode from the shared atomic.
#[must_use]
pub fn load_queue_mode(shared: &SharedQueueMode) -> QueueMode {
    QueueMode::from_u8(shared.load(Ordering::Relaxed))
}

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

    /// Try to handle a slash command synchronously on the coordinator thread.
    /// Returns `true` if the input was a slash command and was handled (caller
    /// should NOT route it to `run_turn`). Returns `false` if the input is
    /// not a slash command (caller should route it normally).
    /// Default returns `false` — test drivers don't handle slash commands.
    fn try_handle_slash_command(&self, _input: &str) -> bool {
        false
    }

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
    mode: SharedQueueMode,
    output: CliOutput,
    startup_banner: String,
    initial_completions: Vec<(String, String)>,
    esc_abort_hook: Option<EscAbortHook>,
) -> Result<(), Box<dyn std::error::Error>> {
    let coord = Arc::new(Mutex::new(TurnInputCoordinator::new()));
    let (input_tx, input_rx) = sync_channel::<InputEvent>(16);
    let (turn_tx, turn_rx) = sync_channel::<TurnEvent>(1);
    // Coordinator → input thread: "output is done, you may prompt."
    // Sent after slash commands complete and after startup. For LLM turns
    // the input thread doesn't wait (user can type during streaming).
    let (prompt_ready_tx, prompt_ready_rx) = sync_channel::<()>(1);

    output.println(startup_banner);
    // Signal the input thread: startup output is done, show ❯.
    let _ = prompt_ready_tx.send(());

    // Input thread — owns its rustyline LineEditor. Sends every submitted line
    // to main via a bounded channel. Exits cleanly on Exit / channel closed.
    // The `↑`-arrow dequeue hook is bound here so the input thread can pop
    // the newest queued input back into the buffer without needing a
    // channel round-trip to main. The ESC abort hook cancels the running
    // turn directly from rustyline's event handler — no raw-mode conflict
    // because rustyline already owns stdin.
    let input_tx_clone = input_tx.clone();
    let dequeue_hook = make_up_arrow_hook(Arc::clone(&coord));
    let input_output = output.clone();
    thread::Builder::new()
        .name("repl-input".into())
        .spawn(move || {
            let mut editor = LineEditor::new_with_dequeue_hook(
                "❯ ",
                initial_completions,
                Some(dequeue_hook),
                esc_abort_hook,
            );
            if prompt_ready_rx.recv().is_err() {
                return;
            }
            loop {
                let read_result = input_output.suspend(|| editor.read_line());
                match read_result {
                    Ok(ReadOutcome::Submit(text)) => {
                        // Echo the submitted input above the sticky bars.
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            let w = crossterm::terminal::size()
                                .map(|(cols, _)| cols as usize)
                                .unwrap_or(80);
                            let (echo, _) = crate::cli::format::format_input_echo(trimmed, w);
                            input_output.println(echo);
                        }
                        if input_tx_clone.send(InputEvent::Submit(text)).is_err() {
                            break;
                        }
                        // Wait for the coordinator to signal that all output
                        // is done before re-entering read_line.
                        if prompt_ready_rx.recv().is_err() {
                            break; // coordinator exited (e.g. /exit)
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
                output.finish();
                driver.on_exit();
                break;
            }
            LoopEvent::Input(InputEvent::Submit(text)) => {
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
                    output.finish();
                    driver.on_exit();
                    break;
                }
                // Slash commands are dispatched synchronously on the
                // coordinator thread — no runner thread, no TurnDone
                // roundtrip. This avoids the timing issues (chrome
                // overwriting output) that plagued the thread-based path.
                if driver.try_handle_slash_command(&text) {
                    let _ = prompt_ready_tx.send(());
                    continue;
                }
                if !turn_active {
                    let next = coord.lock().unwrap().submit_when_idle(text);
                    turn_active = true;
                    runner_handle = Some(spawn_turn(
                        Arc::clone(&driver),
                        next.prompt,
                        turn_tx.clone(),
                    ));
                    // Don't signal prompt_ready — wait for TurnDone so ❯
                    // appears AFTER the runner's output + separator.
                    continue;
                }
                let outcome = coord
                    .lock()
                    .unwrap()
                    .submit_during_turn(text, load_queue_mode(&mode));
                match outcome {
                    SubmitOutcome::Queued => {}
                    SubmitOutcome::Interrupt => {
                        driver.abort_current_turn();
                    }
                    SubmitOutcome::Rejected => {
                        eprintln!(
                            "\x1b[2m(a turn is running; set SUDOCODE_INTERRUPT_QUEUE_MODE=queue to queue instead)\x1b[0m"
                        );
                    }
                }
                // Don't signal prompt_ready — wait for TurnDone.
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
                } else {
                    // All output done, queue drained — show ❯.
                    let _ = prompt_ready_tx.send(());
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
