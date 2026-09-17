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
//! ## Echo timing
//!
//! Only the coordinator knows whether a submit runs now (idle) or waits (queued
//! during a turn), so it — not the UI thread — owns writing the `❯ text` echo to
//! scrollback: immediately for an idle submit, and at the turn boundary for a
//! queued one (via [`NextTurn::echoes`]). Until then a queued input lives only
//! in the transient queue overlay, so pressing Enter mid-turn no longer looks
//! like the input was sent.
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

/// A single queued user input.
///
/// `text` is the full content sent to the model (paste placeholders expanded).
/// `display` is the compact form shown in the pending overlay and echoed to
/// scrollback when the item flushes (paste collapsed to a placeholder chip).
/// `kind` selects the scrollback marker at flush — `❯` for human input, `📨`
/// for an inbound A2A peer message. Both echo on flush (symmetric): the queued
/// item lands in scrollback at the moment it actually runs, never earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput {
    pub text: String,
    pub display: String,
    pub kind: QueuedKind,
}

/// Where a queued item came from — selects its scrollback marker on flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedKind {
    Human,
    Peer,
}

impl QueuedInput {
    pub fn human(text: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            display: display.into(),
            kind: QueuedKind::Human,
        }
    }
    pub fn peer(text: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            display: display.into(),
            kind: QueuedKind::Peer,
        }
    }
}

/// One line to echo to scrollback when the queue flushes — its `display` text
/// and the `kind` that selects the marker. The caller renders it (the queue
/// stays rendering-agnostic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoLine {
    pub display: String,
    pub kind: QueuedKind,
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
    /// The scrollback lines to echo for this run, in submission order (every
    /// queued item echoes on flush now — human and peer alike). Each carries the
    /// marker `kind` so the caller renders `❯`/`📨` correctly. Empty for an idle
    /// submit, whose echo the coordinator prints directly.
    pub echoes: Vec<EchoLine>,
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

    /// Snapshot of pending display texts, oldest first. Drives the queue
    /// overlay so it reflects the coordinator's queue exactly.
    #[must_use]
    pub fn peek_display(&self) -> Vec<String> {
        self.queue.iter().map(|q| q.display.clone()).collect()
    }

    /// Called on the FIRST submit when the REPL is idle. No decision — just run
    /// the input as a normal turn immediately. Kept as a distinct method so
    /// callers don't accidentally route idle submits through the during-turn
    /// path. `echoes` is empty: an idle submit's echo is printed by the caller
    /// right away, not deferred to a turn boundary.
    #[must_use]
    pub fn submit_when_idle(&mut self, text: String) -> NextTurn {
        NextTurn {
            prompt: text,
            echoes: Vec::new(),
            consumed: 1,
        }
    }

    /// Called when a submit lands WHILE a turn is running. Returns what the
    /// caller must do next.
    pub fn submit_during_turn(&mut self, input: QueuedInput, mode: QueueMode) -> SubmitOutcome {
        if mode.queue_enabled() {
            self.queue.push_back(input);
            return SubmitOutcome::Queued;
        }
        // Queue off — sudocode's historical behavior: reject and let the caller
        // print "still running, wait" (or ignore).
        SubmitOutcome::Rejected
    }

    /// Called on turn end. Consumes all queued items and joins their text with
    /// `\n\n` for a single batched turn. `echoes` collects an [`EchoLine`] per
    /// item (human and peer alike) so the caller can write them to scrollback
    /// now, at the moment they actually run, with the right marker.
    ///
    /// Returns `None` when the queue is empty — the caller reverts to the
    /// idle prompt.
    pub fn drain_next(&mut self) -> Option<NextTurn> {
        if self.queue.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        let mut echoes = Vec::new();
        let mut consumed = 0_usize;
        while let Some(item) = self.queue.pop_front() {
            echoes.push(EchoLine {
                display: item.display,
                kind: item.kind,
            });
            parts.push(item.text);
            consumed += 1;
        }
        Some(NextTurn {
            prompt: parts.join("\n\n"),
            echoes,
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

    fn human(text: &str) -> QueuedInput {
        QueuedInput::human(text, text)
    }

    #[test]
    fn idle_submit_runs_alone_without_deferred_echo() {
        let mut c = TurnInputCoordinator::new();
        let next = c.submit_when_idle("hello".to_string());
        assert_eq!(next.prompt, "hello");
        assert!(next.echoes.is_empty());
        assert_eq!(next.consumed, 1);
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn queue_mode_batches_and_echoes_on_turn_end() {
        // queue ON. Three inputs during a turn should flush as ONE batched turn
        // joined with "\n\n", echoing each queued display line in order.
        let mut c = TurnInputCoordinator::new();
        let mode = QueueMode::Queue;
        assert_eq!(
            c.submit_during_turn(human("B"), mode),
            SubmitOutcome::Queued
        );
        assert_eq!(
            c.submit_during_turn(human("C"), mode),
            SubmitOutcome::Queued
        );
        assert_eq!(
            c.submit_during_turn(human("D"), mode),
            SubmitOutcome::Queued
        );
        assert_eq!(c.pending(), 3);
        assert_eq!(c.peek_display(), vec!["B", "C", "D"]);
        let next = c.drain_next().expect("batched turn present");
        assert_eq!(next.prompt, "B\n\nC\n\nD");
        let echo_texts: Vec<&str> = next.echoes.iter().map(|e| e.display.as_str()).collect();
        assert_eq!(echo_texts, vec!["B", "C", "D"]);
        assert!(next.echoes.iter().all(|e| e.kind == QueuedKind::Human));
        assert_eq!(next.consumed, 3);
        assert!(c.drain_next().is_none(), "queue drained");
    }

    #[test]
    fn peer_and_human_both_echo_with_their_kind() {
        // Both human and peer messages echo on flush now (symmetric), each with
        // its own marker kind so the caller renders `❯` vs `📨`.
        let mut c = TurnInputCoordinator::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn(human("human"), mode);
        c.submit_during_turn(QueuedInput::peer("peer-text", "peer-display"), mode);
        let next = c.drain_next().expect("batched turn present");
        assert_eq!(next.prompt, "human\n\npeer-text");
        assert_eq!(next.echoes.len(), 2, "both echo on flush");
        assert_eq!(next.echoes[0].display, "human");
        assert_eq!(next.echoes[0].kind, QueuedKind::Human);
        assert_eq!(next.echoes[1].display, "peer-display");
        assert_eq!(next.echoes[1].kind, QueuedKind::Peer);
    }

    #[test]
    fn off_mode_rejects_during_turn() {
        let mut c = TurnInputCoordinator::new();
        assert_eq!(
            c.submit_during_turn(human("B"), QueueMode::Off),
            SubmitOutcome::Rejected
        );
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn dequeue_last_pops_newest_for_up_arrow_refill() {
        let mut c = TurnInputCoordinator::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn(human("first"), mode);
        c.submit_during_turn(human("second"), mode);
        c.submit_during_turn(human("third"), mode);
        assert_eq!(c.dequeue_last(), Some("third".to_string()));
        assert_eq!(c.dequeue_last(), Some("second".to_string()));
        assert_eq!(c.peek_display(), vec!["first"]);
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
