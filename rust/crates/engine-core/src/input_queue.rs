//! Shared input ownership for native and hosted sessions: humans, peers and tasks.
//!
//! Explicitly queued inputs wait for a turn boundary. Steering transfers them
//! to engine-core, retaining receipts until a durable consumption event arrives.
//! Esc flushes the queue; cancellation is a separate engine command.

pub use engine_events::InputSource as QueuedKind;
use engine_events::{EngineCommand, InputView, TurnInput};
use std::collections::{BTreeMap, VecDeque};

/// Whether the REPL queues inputs that land while a turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    Off,
    Queue,
}

impl QueueMode {
    /// Resolve from `SUDOCODE_INTERRUPT_QUEUE_MODE`. Case-insensitive.
    /// Default is `Queue` — message queue ON, matching Claude Code parity.
    /// In the async REPL, Enter steers independently of the queue setting.
    /// Startup `off` instead selects the legacy synchronous REPL.
    /// Esc flushes pending input;
    /// Ctrl+C always cancels through the engine control channel.
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
/// `display` is the echo source used when the item flushes: compact input for
/// humans (paste collapsed to a placeholder chip), the complete envelope for
/// peers so their body can be laid out at the width at flush time. The UI keeps
/// a separate one-line peer preview outside this scheduling queue.
/// `source` selects the scrollback marker at flush — `❯` for human input, a message frame
/// for an inbound A2A peer message. Both echo on flush (symmetric): the queued
/// item lands in scrollback at the moment it actually runs, never earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput<R = ()> {
    pub input: TurnInput,
    /// The transport receipt follows ownership until durable consumption.
    pub receipt: Option<R>,
}

impl<R> QueuedInput<R> {
    pub fn new(
        text: impl Into<String>,
        display: impl Into<String>,
        source: QueuedKind,
        receipt: Option<R>,
    ) -> Self {
        Self {
            input: TurnInput {
                id: 0,
                text: text.into(),
                display: display.into(),
                source,
                attachments: Vec::new(),
            },
            receipt,
        }
    }
    pub fn human(text: impl Into<String>, display: impl Into<String>) -> Self {
        Self::new(text, display, QueuedKind::Human, None)
    }
    pub fn peer(text: impl Into<String>, display: impl Into<String>) -> Self {
        Self::new(text, display, QueuedKind::Peer, None)
    }
}

/// The same bounded metadata shape drives live rows, consumption and replay.
pub type EchoLine = InputView;

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
pub struct NextTurn<R = ()> {
    /// The prompt text to hand to `run_turn`. When multiple items were batched,
    /// this is their `text` fields joined with `\n\n` in submission order.
    pub prompt: String,
    /// The scrollback lines to echo for this run, in submission order (every
    /// queued item echoes on flush now — human and peer alike). Each carries the
    /// marker `source` so the caller renders human echoes and peer frames correctly. Empty for an idle
    /// submit, whose echo the coordinator prints directly.
    pub echoes: Vec<EchoLine>,
    /// How many queued items this run consumed.
    pub consumed: usize,
    pub receipts: Vec<R>,
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

/// One scheduling policy for a session, independent of its transport.
/// Receipts remain owned here until the conversation durably consumes input.
/// Native and hosted adapters share the same ordering, flush and cancel rules.
#[derive(Debug, Default)]
enum Phase {
    #[default]
    Idle,
    Preparing,
    Running,
    Cancelling {
        resume_requested: bool,
    },
    Paused,
}

#[derive(Debug, Default)]
pub struct TurnInputCoordinator<R = ()> {
    queue: VecDeque<QueuedInput<R>>,
    steering: BTreeMap<u64, QueuedInput<R>>,
    next_id: u64,
    phase: Phase,
}

impl<R> TurnInputCoordinator<R> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            steering: BTreeMap::new(),
            next_id: 0,
            phase: Phase::Idle,
        }
    }

    #[must_use]
    pub fn can_start(&self) -> bool {
        matches!(self.phase, Phase::Idle)
    }
    #[must_use]
    pub fn can_steer(&self) -> bool {
        matches!(self.phase, Phase::Preparing | Phase::Running)
    }
    #[must_use]
    pub fn can_deliver(&self) -> bool {
        matches!(self.phase, Phase::Running)
    }
    #[must_use]
    pub fn has_idle_queue(&self) -> bool {
        !self.is_running() && !self.queue.is_empty()
    }
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(
            self.phase,
            Phase::Preparing | Phase::Running | Phase::Cancelling { .. }
        )
    }
    #[must_use]
    pub fn is_paused(&self) -> bool {
        matches!(
            self.phase,
            Phase::Cancelling {
                resume_requested: false
            } | Phase::Paused
        )
    }
    pub fn start_turn(&mut self) {
        self.phase = Phase::Preparing;
    }
    /// Engine admission opens the inbox and resets abort before this receipt.
    /// Controls accepted during preparation must follow that reset, never precede it.
    pub fn turn_started(&mut self) -> Option<EngineCommand> {
        match self.phase {
            Phase::Preparing => {
                self.phase = Phase::Running;
                let inputs = self
                    .steering
                    .values()
                    .map(|input| input.input.clone())
                    .collect::<Vec<_>>();
                (!inputs.is_empty()).then_some(EngineCommand::Steer { inputs })
            }
            Phase::Cancelling { .. } => Some(EngineCommand::Cancel),
            _ => None,
        }
    }
    pub fn cancel_turn(&mut self) {
        self.phase = Phase::Cancelling {
            resume_requested: false,
        };
    }
    pub fn finish_turn(&mut self) {
        let paused = self.is_paused();
        self.recover_uncommitted();
        self.phase = if paused && !self.queue.is_empty() {
            Phase::Paused
        } else {
            Phase::Idle
        };
    }
    pub fn resume(&mut self) {
        match &mut self.phase {
            Phase::Cancelling { resume_requested } => *resume_requested = true,
            Phase::Paused => self.phase = Phase::Idle,
            _ => {}
        }
    }

    fn identify(&mut self, input: &mut TurnInput) {
        if input.id == 0 {
            self.next_id += 1;
            input.id = self.next_id;
        }
    }

    pub fn enqueue(&mut self, mut input: QueuedInput<R>) -> InputView {
        self.identify(&mut input.input);
        let view = input.input.view();
        self.queue.push_back(input);
        view
    }

    pub fn steer(&mut self, mut input: QueuedInput<R>) -> TurnInput {
        self.identify(&mut input.input);
        let message = input.input.clone();
        self.steering.insert(message.id, input);
        message
    }

    pub fn flush(&mut self) -> Vec<TurnInput> {
        let queued = self.queue.drain(..).collect::<Vec<_>>();
        queued.into_iter().map(|input| self.steer(input)).collect()
    }

    #[must_use]
    pub fn has_pending_input(&self) -> bool {
        !self.queue.is_empty() || !self.steering.is_empty()
    }

    pub fn committed(&mut self, id: u64) -> Option<R> {
        self.steering.remove(&id).and_then(|input| input.receipt)
    }

    pub fn defer(&mut self, id: u64) -> bool {
        if let Some(input) = self.steering.remove(&id) {
            self.queue.push_back(input);
            true
        } else {
            false
        }
    }

    /// Recover unacknowledged steering after turn failure/cancellation. A late
    /// refusal can arrive after `TurnComplete`; ids make that reply idempotent.
    pub fn recover_uncommitted(&mut self) {
        let pending = std::mem::take(&mut self.steering);
        for (_, input) in pending.into_iter().rev() {
            self.queue.push_front(input);
        }
    }

    /// Number of pending inputs (not the currently-running turn — just what's
    /// queued behind it). For the queue-chip UI in the terminal chrome.
    #[must_use]
    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Snapshot of pending display texts, oldest first. Drives the queue
    /// overlay so it reflects the coordinator's queue exactly.
    #[must_use]
    pub fn previews(&self) -> Vec<EchoLine> {
        self.queue.iter().map(|input| input.input.view()).collect()
    }

    /// Pending ownership, including admitted but not yet committed steering.
    pub(crate) fn entries(
        &self,
    ) -> impl Iterator<Item = (&QueuedInput<R>, engine_events::InputDelivery)> {
        self.queue
            .iter()
            .map(|item| (item, engine_events::InputDelivery::Queue))
            .chain(
                self.steering
                    .values()
                    .map(|item| (item, engine_events::InputDelivery::Steer)),
            )
    }

    /// Recall preserves the full input payload and never removes machine input.
    pub(crate) fn recall_human(&mut self) -> Vec<QueuedInput<R>> {
        let (humans, rest) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|item| item.input.source == QueuedKind::Human);
        self.queue = rest;
        humans.into()
    }

    #[cfg(test)]
    #[must_use]
    pub fn peek_display(&self) -> Vec<String> {
        self.queue.iter().map(|q| q.input.display.clone()).collect()
    }

    /// Called on the FIRST submit when the REPL is idle. No decision — just run
    /// the input as a normal turn immediately. Kept as a distinct method so
    /// callers don't accidentally route idle submits through the during-turn
    /// path. `echoes` is empty: an idle submit's echo is printed by the caller
    /// right away, not deferred to a turn boundary.
    #[must_use]
    pub fn submit_when_idle(&mut self, input: QueuedInput<R>) -> NextTurn<R> {
        NextTurn {
            prompt: input.input.text,
            echoes: Vec::new(),
            consumed: 1,
            receipts: input.receipt.into_iter().collect(),
        }
    }

    /// Called when a submit lands WHILE a turn is running. Returns what the
    /// caller must do next.
    pub fn submit_during_turn(&mut self, input: QueuedInput<R>, mode: QueueMode) -> SubmitOutcome {
        if mode.queue_enabled() {
            let _ = self.enqueue(input);
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
    pub fn drain_next(&mut self) -> Option<NextTurn<R>> {
        if self.queue.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        let mut echoes = Vec::new();
        let mut consumed = 0_usize;
        let mut receipts = Vec::new();
        while let Some(item) = self.queue.pop_front() {
            echoes.push(item.input.view());
            parts.push(item.input.text);
            receipts.extend(item.receipt);
            consumed += 1;
        }
        Some(NextTurn {
            prompt: parts.join("\n\n"),
            echoes,
            consumed,
            receipts,
        })
    }

    /// Up-arrow dequeue: pop the newest **human**-queued item back so the user
    /// can edit it in the input buffer. Returns the removed text; the caller
    /// feeds it into the input slot.
    ///
    /// Peer (inbound A2A) items are skipped — they are machine-delivered, not
    /// something the user typed, so `↑` never pulls one into the edit buffer.
    /// A skipped Peer item stays in the queue in its original position; only
    /// the newest Human item is removed. Returns `None` when the queue holds no
    /// Human item (all-Peer or empty), in which case the caller falls through
    /// to its normal empty-buffer `↑` behavior (prompt history).
    /// Up-arrow recall: remove **all** queued human items and return their
    /// text joined by newlines in submit (FIFO) order, so the input slot reads
    /// oldest-at-top. Mirrors Claude Code's `popAllEditable`: one `↑` pulls
    /// back everything the user typed while the turn ran, in one step — not one
    /// item per press.
    ///
    /// Peer (inbound A2A) items are skipped — machine-delivered, not typed —
    /// and left in the queue in their original positions. Returns `None` when
    /// the queue holds no human item (all-Peer or empty), so the caller falls
    /// through to its normal empty-buffer `↑` behavior (prompt history).
    pub fn dequeue_all_human(&mut self) -> Option<String> {
        if !self
            .queue
            .iter()
            .any(|q| q.input.source == QueuedKind::Human)
        {
            return None;
        }
        // Partition in place: collect human texts in order, retain every
        // non-human item (Peer / Subagent — machine-delivered, not typed).
        let mut humans = Vec::new();
        let mut rest = VecDeque::with_capacity(self.queue.len());
        for item in std::mem::take(&mut self.queue) {
            if item.input.source == QueuedKind::Human {
                humans.push(item.input.text);
            } else {
                rest.push_back(item);
            }
        }
        self.queue = rest;
        Some(humans.join("\n"))
    }

    /// Drop everything without running any of it. Used by shutdown / /clear.
    pub fn clear(&mut self) {
        self.steering.clear();
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
        let mut c = TurnInputCoordinator::<()>::new();
        let next = c.submit_when_idle(human("hello"));
        assert_eq!(next.prompt, "hello");
        assert_eq!(next.echoes, [] as [engine_events::InputView; 0]);
        assert_eq!(next.consumed, 1);
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn queue_mode_batches_and_echoes_on_turn_end() {
        // queue ON. Three inputs during a turn should flush as ONE batched turn
        // joined with "\n\n", echoing each queued display line in order.
        let mut c = TurnInputCoordinator::<()>::new();
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
        assert!(next.echoes.iter().all(|e| e.source == QueuedKind::Human));
        assert_eq!(next.consumed, 3);
        assert!(c.drain_next().is_none(), "queue drained");
    }

    #[test]
    fn peer_and_human_both_echo_with_their_kind() {
        // Both human and peer messages echo on flush now (symmetric), each with
        // its own marker kind so the caller renders `❯` vs `📨`.
        let mut c = TurnInputCoordinator::<()>::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn(human("human"), mode);
        c.submit_during_turn(QueuedInput::peer("peer-text", "peer-display"), mode);
        let next = c.drain_next().expect("batched turn present");
        assert_eq!(next.prompt, "human\n\npeer-text");
        assert_eq!(next.echoes.len(), 2, "both echo on flush");
        assert_eq!(next.echoes[0].display, "human");
        assert_eq!(next.echoes[0].source, QueuedKind::Human);
        assert_eq!(next.echoes[1].display, "peer-display");
        assert_eq!(next.echoes[1].source, QueuedKind::Peer);
    }

    #[test]
    fn off_mode_rejects_during_turn() {
        let mut c = TurnInputCoordinator::<()>::new();
        assert_eq!(
            c.submit_during_turn(human("B"), QueueMode::Off),
            SubmitOutcome::Rejected
        );
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn dequeue_all_human_returns_all_in_submit_order() {
        // One ↑ pops ALL human items at once, joined oldest-at-top (submit
        // order), and empties the human side of the queue.
        let mut c = TurnInputCoordinator::<()>::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn(human("first"), mode);
        c.submit_during_turn(human("second"), mode);
        c.submit_during_turn(human("third"), mode);
        assert_eq!(
            c.dequeue_all_human(),
            Some("first\nsecond\nthird".to_string())
        );
        assert_eq!(c.pending(), 0);
        // Nothing left to recall.
        assert_eq!(c.dequeue_all_human(), None);
    }

    #[test]
    fn dequeue_all_human_skips_peers_and_leaves_them_queued() {
        // Peer (a2a) items interleaved with humans must be skipped: `↑` pulls
        // only the human texts (in order) and every peer item stays in the
        // queue, in its original relative order, to flush on turn end.
        let mut c = TurnInputCoordinator::<()>::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn(human("h1"), mode);
        c.submit_during_turn(QueuedInput::peer("p1-text", "p1-display"), mode);
        c.submit_during_turn(human("h2"), mode);
        c.submit_during_turn(QueuedInput::peer("p2-text", "p2-display"), mode);
        assert_eq!(c.dequeue_all_human(), Some("h1\nh2".to_string()));
        // Both peers untouched and still pending, in order.
        assert_eq!(c.pending(), 2);
        assert_eq!(c.peek_display(), vec!["p1-display", "p2-display"]);
    }

    #[test]
    fn dequeue_all_human_returns_none_when_only_peer_items_queued() {
        // All-peer queue: `↑` finds no human item, returns None so the caller
        // falls through to prompt-history. No peer item is popped.
        let mut c = TurnInputCoordinator::<()>::new();
        let mode = QueueMode::Queue;
        c.submit_during_turn(QueuedInput::peer("a", "a"), mode);
        c.submit_during_turn(QueuedInput::peer("b", "b"), mode);
        assert_eq!(c.dequeue_all_human(), None);
        assert_eq!(c.pending(), 2);
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
