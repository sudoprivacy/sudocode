//! REPL input queue + interrupt coordinator for the interactive shell.
//!
//! Mirrors the semantics of sudowork's `turnInputCoordinator` (see
//! [design](https://s.shareone.vip/s/sudowork-interrupt-queue) §3.2 matrix) so
//! users get the same behavior whether they're in the sudocode CLI or driving it
//! through sudowork's ACP server. The two products are runtime-exclusive — one
//! sudocode process is either a REPL for a human at a terminal OR an ACP server
//! for sudowork; behavior parity is on the interaction semantics, not on a shared
//! runtime queue.
//!
//! ## Matrix (§3.2)
//!
//! |                        | queue OFF                  | queue ON                                        |
//! |------------------------|----------------------------|--------------------------------------------------|
//! | **auto-interrupt OFF** | current behavior (blocked) | queued during turn; batched flush on turn end   |
//! | **auto-interrupt ON**  | send-immediately-interrupt | interrupter runs solo, rest queued + batched    |
//!
//! ## Batched flush
//!
//! When N inputs are queued while a turn is running, on turn end the coordinator
//! joins them with `\n\n` and issues ONE `run_turn` — not N. This matches
//! "I'll queue up what I want to say, send it all together when you finish".
//! The auto-interrupter, if any, always runs alone as a fresh solo turn (§3.2
//! row 2: "第一条立即打断并单独作为新轮启动") — its follow-ups then batch normally.
//!
//! ## Env-gated opt-in
//!
//! Reads `SUDOCODE_INTERRUPT_QUEUE_MODE`:
//! - unset / `off` — current sync behavior, no queue, no interrupt
//! - `queue` — queue ON, auto-interrupt OFF
//! - `interrupt` — auto-interrupt ON, queue OFF (interrupt-then-send)
//! - `both` — both ON (sudowork-parity default)
//!
//! Wiring (input-thread + main coordinator + tokio worker) lives in
//! `main.rs`'s interactive REPL and is only activated when this env var is set,
//! so the default sync REPL path is untouched. See
//! `notes/plans/conversation-interrupt-queue-sudocode.md` for the full plan.

use std::collections::VecDeque;

/// Which parts of the interrupt+queue matrix are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Off,
    Queue,
    Interrupt,
    Both,
}

impl QueueMode {
    /// Resolve from `SUDOCODE_INTERRUPT_QUEUE_MODE`. Case-insensitive; anything
    /// unrecognized (including unset) → `Off` so the default REPL path stays
    /// bit-for-bit identical to today's behavior until a user explicitly opts in.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("SUDOCODE_INTERRUPT_QUEUE_MODE")
            .ok()
            .and_then(|v| Self::from_str(&v))
            .unwrap_or(Self::Off)
    }

    fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "" => Some(Self::Off),
            "queue" => Some(Self::Queue),
            "interrupt" => Some(Self::Interrupt),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    #[must_use]
    pub fn queue_enabled(self) -> bool {
        matches!(self, Self::Queue | Self::Both)
    }

    #[must_use]
    pub fn interrupt_enabled(self) -> bool {
        matches!(self, Self::Interrupt | Self::Both)
    }
}

/// A single queued user input (the text the user typed at the prompt, already
/// trimmed and stripped of slash-command chrome by whatever caller stores it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput {
    pub text: String,
    /// When true, this input MUST run as its own solo turn — it does NOT join
    /// a batched flush. Set by the auto-interrupt path so an interrupter is a
    /// fresh turn per §3.2 row 2.
    pub solo: bool,
}

impl QueuedInput {
    pub fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            solo: false,
        }
    }
    pub fn solo(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            solo: true,
        }
    }
}

/// The decision returned by `submit_during_turn` — what the caller must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Input added to the queue; no interrupt. On turn end, it will flush.
    Queued,
    /// Auto-interrupt fires. Caller MUST call the abort signal to cancel the
    /// current turn; the interrupter has been placed at the queue head and
    /// will run solo on the next drain iteration.
    Interrupt,
    /// Both toggles are off — the current sync behavior. Caller should print
    /// a "wait for reply" tip and drop the input.
    Rejected,
}

/// The next batch to run after a turn ends (or after an interrupt fires and
/// the current turn cancels). `None` = queue is empty, sit at the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextTurn {
    /// The prompt text to hand to `run_turn`. When multiple non-solo items were
    /// batched, this is their `text` fields joined with `\n\n` in submission order.
    pub prompt: String,
    /// True when this run corresponds to a single solo item (an auto-interrupter).
    /// Callers can use this for UI (e.g. "🔀 running interrupter" vs "▶ flushing
    /// queued batch of N").
    pub solo: bool,
    /// How many queued items this run consumed. Solo runs always report 1;
    /// batched runs report the batch size.
    pub consumed: usize,
}

/// SSOT for "what does the REPL do with each new line the user types". Mirrors
/// sudowork's `turnInputCoordinator` (`src/process/task/turnInputCoordinator.ts`)
/// but the state model is simpler because sudocode has ONE conversation per REPL
/// instance (sudowork multiplexes many by `conversationId`).
///
/// The coordinator itself is pure sync — no threads, no channels. Wiring it to
/// the async input-thread + tokio worker lives in `main.rs`.
#[derive(Debug, Default)]
pub struct TurnInputCoordinator {
    queue: VecDeque<QueuedInput>,
}

impl TurnInputCoordinator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Number of pending inputs (not the currently-running turn — just what's
    /// queued behind it). For the queue-chip UI in the terminal chrome.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Snapshot of pending texts, oldest first. Used to render the terminal
    /// queue chips and for the Up-arrow dequeue candidate ("give me the last
    /// thing I queued back").
    #[must_use]
    pub fn peek(&self) -> Vec<&str> {
        self.queue.iter().map(|q| q.text.as_str()).collect()
    }

    /// Called on the FIRST submit when the REPL is idle. No matrix decision —
    /// just run the input as a normal turn immediately. Kept as a distinct
    /// method so callers don't accidentally route idle submits through the
    /// during-turn matrix.
    #[must_use]
    pub fn submit_when_idle(&mut self, text: String) -> NextTurn {
        NextTurn {
            prompt: text,
            solo: false,
            consumed: 1,
        }
    }

    /// Called when a submit lands WHILE a turn is running. Applies the matrix
    /// and mutates the queue in place. Returns what the caller must do next.
    pub fn submit_during_turn(&mut self, text: String, mode: QueueMode) -> SubmitOutcome {
        if mode.interrupt_enabled() {
            // Auto-interrupt: the new input becomes a solo run at the head. If
            // queue is also off, drop everything else — matches sudowork
            // "auto-interrupt + queue off = pending items are dropped".
            if !mode.queue_enabled() {
                self.queue.clear();
            }
            self.queue.push_front(QueuedInput::solo(text));
            return SubmitOutcome::Interrupt;
        }
        if mode.queue_enabled() {
            self.queue.push_back(QueuedInput::normal(text));
            return SubmitOutcome::Queued;
        }
        // Both toggles off — sudocode's historical behavior: reject and let
        // the caller print "still running, wait" (or ignore).
        SubmitOutcome::Rejected
    }

    /// Called on turn end (natural completion OR after `abort_signal.abort()`
    /// during an auto-interrupt). Consumes items off the head:
    /// - if the head is `solo`, take ONLY it (auto-interrupter runs alone)
    /// - else, take the head + all successive non-solo items and join their
    ///   text with `\n\n` for a single batched turn
    ///
    /// Returns `None` when the queue is empty — the caller reverts to the
    /// idle prompt.
    pub fn drain_next(&mut self) -> Option<NextTurn> {
        let head = self.queue.pop_front()?;
        if head.solo {
            return Some(NextTurn {
                prompt: head.text,
                solo: true,
                consumed: 1,
            });
        }
        let mut parts = vec![head.text];
        let mut consumed = 1_usize;
        while let Some(front) = self.queue.front() {
            if front.solo {
                break;
            }
            let taken = self
                .queue
                .pop_front()
                .expect("front peek proved item exists");
            parts.push(taken.text);
            consumed += 1;
        }
        Some(NextTurn {
            prompt: parts.join("\n\n"),
            solo: false,
            consumed,
        })
    }

    /// Up-arrow dequeue: pop the LAST-queued (newest) item back so the user can
    /// edit it in the input buffer. Returns the removed text; the caller feeds
    /// it back into rustyline as `Cmd::Insert`.
    pub fn dequeue_last(&mut self) -> Option<String> {
        self.queue.pop_back().map(|q| q.text)
    }

    /// Drop everything without running any of it. Used by shutdown / /clear.
    pub fn clear(&mut self) {
        self.queue.clear();
    }
}

// -------------------------------------------------------------------
// Unit tests below are gated so that release builds skip them entirely,
// per the sudocode "no unit tests" convention (memory
// feedback_no_unit_tests_sudocode) — the tests exist purely as executable
// documentation of the §3.2 matrix and are the ONLY unit tests in this
// crate. If the matrix ever grows, extend the doc tests below rather
// than adding a Cargo test target.
//
// Real behavioral coverage lands in a PTY integration test (Phase 2 of
// notes/plans/conversation-interrupt-queue-sudocode.md).
// -------------------------------------------------------------------

#[cfg(test)]
mod matrix_docs {
    use super::*;

    #[test]
    fn idle_submit_runs_alone_no_matrix() {
        let mut c = TurnInputCoordinator::new();
        let next = c.submit_when_idle("hello".to_string());
        assert_eq!(next.prompt, "hello");
        assert!(!next.solo);
        assert_eq!(next.consumed, 1);
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn queue_mode_batches_on_turn_end() {
        // §3.2: queue ON, auto-interrupt OFF. Three inputs during a turn should
        // flush as ONE batched turn joined with "\n\n".
        let mut c = TurnInputCoordinator::new();
        let mode = QueueMode::Queue;
        assert_eq!(
            c.submit_during_turn("B".into(), mode),
            SubmitOutcome::Queued
        );
        assert_eq!(
            c.submit_during_turn("C".into(), mode),
            SubmitOutcome::Queued
        );
        assert_eq!(
            c.submit_during_turn("D".into(), mode),
            SubmitOutcome::Queued
        );
        assert_eq!(c.pending(), 3);
        let next = c.drain_next().expect("batched turn present");
        assert_eq!(next.prompt, "B\n\nC\n\nD");
        assert_eq!(next.consumed, 3);
        assert!(!next.solo);
        assert!(c.drain_next().is_none(), "queue drained");
    }

    #[test]
    fn interrupt_mode_forces_solo_and_drops_queue_when_queue_off() {
        // §3.2: auto-interrupt ON, queue OFF. Any pending items are dropped;
        // the interrupter runs alone.
        let mut c = TurnInputCoordinator::new();
        // Prime the queue with one item that queue-mode had put there earlier.
        c.queue.push_back(QueuedInput::normal("stale-B"));
        assert_eq!(
            c.submit_during_turn("C".into(), QueueMode::Interrupt),
            SubmitOutcome::Interrupt
        );
        // stale-B dropped; C is the solo head.
        let next = c.drain_next().expect("interrupter present");
        assert_eq!(next.prompt, "C");
        assert!(next.solo);
        assert_eq!(next.consumed, 1);
        assert!(c.drain_next().is_none());
    }

    #[test]
    fn both_mode_interrupter_solo_then_batched_rest() {
        // §3.2: both ON. Interrupter runs solo; anything queued behind it
        // batches together on the next drain.
        let mut c = TurnInputCoordinator::new();
        assert_eq!(
            c.submit_during_turn("B".into(), QueueMode::Queue),
            SubmitOutcome::Queued
        );
        // C interrupts — solo, goes to head; B is pushed back one slot.
        assert_eq!(
            c.submit_during_turn("C".into(), QueueMode::Both),
            SubmitOutcome::Interrupt
        );
        // D queued after the interrupt in Queue-only mode — goes to tail.
        assert_eq!(
            c.submit_during_turn("D".into(), QueueMode::Queue),
            SubmitOutcome::Queued
        );
        // First drain: solo C.
        let next = c.drain_next().unwrap();
        assert_eq!(next.prompt, "C");
        assert!(next.solo);
        // Second drain: B + D batched.
        let next = c.drain_next().unwrap();
        assert_eq!(next.prompt, "B\n\nD");
        assert!(!next.solo);
        assert_eq!(next.consumed, 2);
    }

    #[test]
    fn off_mode_rejects_during_turn() {
        let mut c = TurnInputCoordinator::new();
        assert_eq!(
            c.submit_during_turn("B".into(), QueueMode::Off),
            SubmitOutcome::Rejected
        );
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn dequeue_last_pops_newest_for_up_arrow_refill() {
        let mut c = TurnInputCoordinator::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn("first".into(), mode);
        c.submit_during_turn("second".into(), mode);
        c.submit_during_turn("third".into(), mode);
        assert_eq!(c.dequeue_last(), Some("third".to_string()));
        assert_eq!(c.dequeue_last(), Some("second".to_string()));
        assert_eq!(c.peek(), vec!["first"]);
    }

    #[test]
    fn queue_mode_env_var_parses_all_variants() {
        // Executable spec of the env-var contract — bump if the enum changes.
        assert_eq!(QueueMode::from_str("off"), Some(QueueMode::Off));
        assert_eq!(QueueMode::from_str("QUEUE"), Some(QueueMode::Queue));
        assert_eq!(
            QueueMode::from_str(" interrupt "),
            Some(QueueMode::Interrupt)
        );
        assert_eq!(QueueMode::from_str("Both"), Some(QueueMode::Both));
        assert_eq!(QueueMode::from_str("nonsense"), None);
    }
}
