//! REPL input queue for the interactive shell.
//!
//! When a turn is running, user (and A2A peer) inputs are queued rather than
//! blocked, and flushed as one batched turn when the turn ends. This mirrors
//! the queue half of sudowork's `turnInputCoordinator`; sudocode's REPL does
//! not auto-interrupt a running turn — the human uses ESC / Ctrl-C to cancel
//! explicitly (see `EscCancelHandler` and the `abort_hook` in `LineEditor`),
//! and A2A peers never interrupt. The two products are runtime-exclusive: one
//! sudocode process is either a REPL for a human at a terminal OR an ACP server
//! for sudowork.
//!
//! ## Behavior
//!
//! |                | queue OFF                  | queue ON (default)                            |
//! |----------------|----------------------------|-----------------------------------------------|
//! | during a turn  | rejected (blocked)         | queued during turn; batched flush on turn end |
//!
//! ## Batched flush
//!
//! When N inputs are queued while a turn is running, on turn end the coordinator
//! joins them with `\n\n` and issues ONE `run_turn` — not N. This matches
//! "I'll queue up what I want to say, send it all together when you finish".
//!
//! ## Environment override
//!
//! Reads `SUDOCODE_INTERRUPT_QUEUE_MODE`:
//! - unset — defaults to `queue` (CC parity)
//! - `off` — sync behavior, no queue
//! - `queue` — queue ON (default)
//!
//! Wiring (input-thread + main coordinator + tokio worker) lives in `main.rs`'s
//! interactive REPL.

use std::collections::VecDeque;

/// Whether the REPL queues inputs that land while a turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Off,
    Queue,
}

impl QueueMode {
    /// Resolve from `SUDOCODE_INTERRUPT_QUEUE_MODE`. Case-insensitive.
    /// Default is `Queue` — message queue ON, matching Claude Code parity.
    /// ESC and Ctrl-C cancel are wired in the async REPL via
    /// `EscCancelHandler` and the `abort_hook` in `LineEditor`.
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("SUDOCODE_INTERRUPT_QUEUE_MODE")
            .ok()
            .and_then(|v| Self::from_str(&v))
            .unwrap_or(Self::Queue)
    }

    fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "" => Some(Self::Off),
            "queue" => Some(Self::Queue),
            _ => None,
        }
    }

    #[must_use]
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Queue => 1,
        }
    }

    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Off,
            _ => Self::Queue,
        }
    }

    #[must_use]
    pub fn queue_enabled(self) -> bool {
        matches!(self, Self::Queue)
    }
}

/// A single queued user input (the text the user typed at the prompt, already
/// trimmed and stripped of slash-command chrome by whatever caller stores it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput {
    pub text: String,
}

impl QueuedInput {
    pub fn normal(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// The decision returned by `submit_during_turn` — what the caller must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Input added to the queue; on turn end, it will flush.
    Queued,
    /// Queue is off — the current sync behavior. Caller should print a
    /// "wait for reply" tip and drop the input.
    Rejected,
}

/// The next batch to run after a turn ends. `None` = queue is empty, sit at
/// the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextTurn {
    /// The prompt text to hand to `run_turn`. When multiple items were batched,
    /// this is their `text` fields joined with `\n\n` in submission order.
    pub prompt: String,
    /// How many queued items this run consumed.
    pub consumed: usize,
}

/// The active [`QueueMode`], shared with whatever reads it mid-session.
///
/// `/config set queue on|off` writes here and the REPL reads it at each turn
/// boundary, so the mode is a live setting rather than a value fixed at startup.
pub type SharedQueueMode = std::sync::Arc<std::sync::atomic::AtomicU8>;

/// Create a shared queue mode from the initial value.
#[must_use]
pub fn shared_queue_mode(mode: QueueMode) -> SharedQueueMode {
    std::sync::Arc::new(std::sync::atomic::AtomicU8::new(mode.to_u8()))
}

/// Read the current queue mode from the shared cell.
#[must_use]
pub fn load_queue_mode(shared: &SharedQueueMode) -> QueueMode {
    QueueMode::from_u8(shared.load(std::sync::atomic::Ordering::Relaxed))
}

/// SSOT for "what does the REPL do with each new line the user types". Mirrors
/// the queue half of sudowork's `turnInputCoordinator`
/// (`src/process/task/turnInputCoordinator.ts`) but the state model is simpler
/// because sudocode has ONE conversation per REPL instance (sudowork
/// multiplexes many by `conversationId`).
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

    /// Called on the FIRST submit when the REPL is idle. No decision — just run
    /// the input as a normal turn immediately. Kept as a distinct method so
    /// callers don't accidentally route idle submits through the during-turn
    /// path.
    #[must_use]
    pub fn submit_when_idle(&mut self, text: String) -> NextTurn {
        NextTurn {
            prompt: text,
            consumed: 1,
        }
    }

    /// Called when a submit lands WHILE a turn is running. Returns what the
    /// caller must do next.
    pub fn submit_during_turn(&mut self, text: String, mode: QueueMode) -> SubmitOutcome {
        if mode.queue_enabled() {
            self.queue.push_back(QueuedInput::normal(text));
            return SubmitOutcome::Queued;
        }
        // Queue off — sudocode's historical behavior: reject and let the caller
        // print "still running, wait" (or ignore).
        SubmitOutcome::Rejected
    }

    /// Called on turn end. Consumes all queued items and joins their text with
    /// `\n\n` for a single batched turn.
    ///
    /// Returns `None` when the queue is empty — the caller reverts to the
    /// idle prompt.
    pub fn drain_next(&mut self) -> Option<NextTurn> {
        let head = self.queue.pop_front()?;
        let mut parts = vec![head.text];
        let mut consumed = 1_usize;
        while let Some(taken) = self.queue.pop_front() {
            parts.push(taken.text);
            consumed += 1;
        }
        Some(NextTurn {
            prompt: parts.join("\n\n"),
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
// documentation of the queue behavior and are the ONLY unit tests in this
// crate.
//
// Real behavioral coverage lands in a PTY integration test.
// -------------------------------------------------------------------

#[cfg(test)]
mod queue_docs {
    use super::*;

    #[test]
    fn idle_submit_runs_alone() {
        let mut c = TurnInputCoordinator::new();
        let next = c.submit_when_idle("hello".to_string());
        assert_eq!(next.prompt, "hello");
        assert_eq!(next.consumed, 1);
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn queue_mode_batches_on_turn_end() {
        // queue ON. Three inputs during a turn should flush as ONE batched turn
        // joined with "\n\n".
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
        assert!(c.drain_next().is_none(), "queue drained");
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
    fn queue_mode_env_var_parses_variants() {
        // Executable spec of the env-var contract.
        assert_eq!(QueueMode::from_str("off"), Some(QueueMode::Off));
        assert_eq!(QueueMode::from_str("QUEUE"), Some(QueueMode::Queue));
        assert_eq!(QueueMode::from_str(" queue "), Some(QueueMode::Queue));
        assert_eq!(QueueMode::from_str("interrupt"), None);
        assert_eq!(QueueMode::from_str("nonsense"), None);
    }
}
