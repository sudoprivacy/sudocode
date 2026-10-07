//! The engine↔renderer seam, realized.
//!
//! # Where the cut is
//!
//! This module is the whole abstraction. There are exactly three public
//! artifacts, and every renderer / every engine goes through them — nothing
//! else crosses:
//!
//! ```text
//!            renderer side  (ABOVE the seam)          engine side (BELOW)
//!            ────────────────────────────────         ────────────────────
//!   REPL  ──▶ EngineHandle { commands, events } ──▶ EngineSession ──▶ EngineDelegate
//!   ACP   ──▶ (send EngineCommand, recv EngineEvent)   (the pump)      (impl'd by the CLI)
//!  moss/…─▶                                                             wraps one runtime turn
//! ```
//!
//! * [`EngineCommand`] / [`EngineEvent`] — the only *data* that crosses (defined
//!   in `engine_events`).
//! * [`EngineHandle`] — the renderer holds this: `send` a [`EngineCommand`],
//!   `recv` a [`EngineEvent`]. To add a NEW renderer, consume an `EngineHandle`.
//!   That is the entire renderer-side contract.
//! * [`EngineDelegate`] — the engine holds this: one method per thing a turn can
//!   do (`run_turn`, `set_model`, …). To plug a NEW engine, implement
//!   `EngineDelegate`. That is the entire engine-side contract.
//!
//! [`EngineSession`] is the pump between the two. It is generic over
//! `dyn EngineDelegate`, so it contains **no** engine-specific and **no**
//! renderer-specific logic — it only routes commands to the delegate and the
//! delegate's callbacks back out as events. It is the generalization of the ACP
//! server's `run_acp_on_transport`, with the ACP wire swapped for the
//! [`EngineEvent`] channel.
//!
//! # Interactive requests
//!
//! Permissions and questions return owned reply futures. A turn-scoped input
//! queue serializes their presentation, while the engine keeps polling other
//! tools and the provider stream. A request lease removes the pending answer
//! on completion or cancellation. Synchronous compatibility callers use the
//! same bridge with `block_in_place`; the normal tool loop awaits replies.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;

use engine_events::{
    ContentBlock, EngineCommand, EngineEvent, EngineState, PermissionMode,
    PermissionPromptDecision, PermissionRequest, QuestionPromptAnswer, QuestionPromptRequest,
    RequestId, TurnComplete,
};
use runtime::{
    HookAbortSignal, PermissionPrompter, QuestionPrompter, RuntimeError, RuntimeObserver,
    TokenUsage,
};

/// The engine-side contract: one live session's worth of "things a turn can do".
///
/// Implement this to plug an engine into the seam. The CLI implements it over
/// its `ConversationRuntime`; the ACP path reuses the same impl. Held as
/// `Arc<dyn EngineDelegate>` so the pump and the blocking turn can share it
/// (methods take `&self` + interior mutability, mirroring the runtime's own
/// `&self` tool dispatch).
///
/// The driver never inspects engine internals — it only calls these methods and
/// forwards the results as [`EngineEvent`]s.
pub trait EngineDelegate: Send + Sync + 'static {
    /// Run exactly one turn to completion (or cancellation), driving the
    /// model/tool loop.
    ///
    /// The impl MUST:
    /// * forward every streaming event to `observer` (the runtime already does
    ///   this when you pass the observer into `run_turn_with_blocks`);
    /// * consult `prompter` for permission decisions (pass it into the runtime);
    /// * run on a **multi-threaded Tokio runtime** (`rt.block_on(...)`), so the
    ///   `prompter`/question `block_in_place` bridge does not panic.
    ///
    /// Returns the end-of-turn aggregate, or a typed [`RuntimeError`] carrying
    /// the who-can-act classification (context-window / retryable) so surfaces
    /// above the seam can act on the bucket instead of string-matching text.
    fn run_turn(
        &self,
        blocks: Vec<ContentBlock>,
        observer: &mut dyn RuntimeObserver,
        prompter: &mut dyn PermissionPrompter,
    ) -> Result<TurnComplete, RuntimeError>;

    /// Install the question prompter the `AskUserQuestion` tool uses for the
    /// *next* turn. The driver calls this immediately before each `run_turn`.
    fn set_question_prompter(&self, prompter: Box<dyn QuestionPrompter>);

    /// A clone of the in-flight turn's abort signal. The pump fires it on
    /// [`EngineCommand::Cancel`] while `run_turn` is blocked.
    fn abort_signal(&self) -> HookAbortSignal;

    /// One registry owned by the engine session, shared by every turn driver.
    fn background_tasks(&self) -> Option<runtime::background_tasks::BackgroundTasks> {
        None
    }

    /// Switch the active model. Returns `(display_model, available_models)`; the
    /// driver emits [`EngineEvent::ModelChanged`].
    fn set_model(&self, model: &str) -> Result<(String, Vec<String>), String>;

    /// Switch the active permission mode; the driver emits
    /// [`EngineEvent::PermissionModeChanged`].
    fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), String>;

    /// Run a slash command, returning its text output (emitted as
    /// [`EngineEvent::Notice`]).
    fn handle_slash_command(&self, line: &str) -> Result<String, String>;

    /// Tear the session down (persist, drop). Called on [`EngineCommand::Close`].
    fn close(&self);
}

/// The renderer-side handle to a running engine session.
///
/// This is the *entire* renderer-side contract: `commands.send(cmd)` to drive
/// the engine, `events.recv()` to observe it. Both are plain
/// [`std::sync::mpsc`] endpoints so a tokio-free renderer (the REPL) can block
/// on `events.recv()` directly.
pub struct EngineHandle {
    /// Send [`EngineCommand`]s into the engine (prompt, cancel, answer, …).
    pub commands: std_mpsc::Sender<EngineCommand>,
    /// Receive [`EngineEvent`]s from the engine (deltas, requests, completion, …).
    pub events: std_mpsc::Receiver<EngineEvent>,
    stopped: std_mpsc::Receiver<()>,
}

impl EngineHandle {
    /// Close and await engine teardown, including delegate-owned resources.
    /// The caller must release any other lifecycle/delegate owners first.
    #[must_use]
    pub fn shutdown(self, timeout: std::time::Duration) -> bool {
        let _ = self.commands.send(EngineCommand::Close);
        drop(self.commands);
        self.stopped.recv_timeout(timeout).is_ok()
    }
}

/// The pump. Spawns a dedicated engine thread that owns a Tokio runtime, routes
/// [`EngineCommand`]s to the [`EngineDelegate`], and streams the delegate's
/// callbacks back out as [`EngineEvent`]s. Contains no engine- or
/// renderer-specific logic.
pub struct EngineSession;

impl EngineSession {
    /// Start driving `delegate` on its own thread and return the renderer-side
    /// [`EngineHandle`]. The engine thread lives until an
    /// [`EngineCommand::Close`] is received or the command channel is dropped.
    #[must_use]
    pub fn spawn(delegate: Arc<dyn EngineDelegate>) -> EngineHandle {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<EngineCommand>();
        let (evt_tx, evt_rx) = std_mpsc::channel::<EngineEvent>();
        let (stopped_tx, stopped_rx) = std_mpsc::channel();

        std::thread::Builder::new()
            .name("engine-session".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("engine-session tokio runtime");
                rt.block_on(drive(delegate, cmd_rx, evt_tx));
                drop(rt);
                let _ = stopped_tx.send(());
            })
            .expect("spawn engine-session thread");

        EngineHandle {
            commands: cmd_tx,
            events: evt_rx,
            stopped: stopped_rx,
        }
    }
}

/// One in-flight request awaiting a renderer answer. Keyed by [`RequestId`] in
/// the shared table so the pump can route the matching answer command back to
/// the blocked prompter.
enum PendingAnswer {
    Permission(oneshot::Sender<PermissionPromptDecision>),
    Question(oneshot::Sender<Result<Vec<QuestionPromptAnswer>, String>>),
}

/// Shared awaiting table + id allocator, threaded through the prompt adapters
/// and the pump for a single turn.
#[derive(Clone, Default)]
struct RequestTable {
    pending: Arc<Mutex<HashMap<RequestId, PendingAnswer>>>,
    next_id: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
    interaction: runtime::PromptQueue,
}

impl RequestTable {
    fn alloc(&self) -> RequestId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn insert(&self, id: RequestId, answer: PendingAnswer) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.cancelled.load(Ordering::SeqCst) {
            pending.insert(id, answer);
        }
    }

    fn take(&self, id: RequestId) -> Option<PendingAnswer> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
    }

    /// Dropping the answer senders wakes a worker blocked in a permission or
    /// question prompt when the enclosing turn is cancelled.
    fn cancel_all(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

/// The command loop. Runs on the engine thread's Tokio runtime.
async fn drive(
    delegate: Arc<dyn EngineDelegate>,
    cmd_rx: std_mpsc::Receiver<EngineCommand>,
    evt_tx: std_mpsc::Sender<EngineEvent>,
) {
    let subagents = SubagentRelay::to_session(evt_tx.clone());
    let tasks = delegate.background_tasks().unwrap_or_default();
    // A cancelled prompt can reply after the next turn starts. Never reuse
    // its id for a new question or permission request in this engine session.
    let next_request_id = Arc::new(AtomicU64::new(0));
    // Bridge the std command receiver into a Tokio channel so the per-turn pump
    // can `select!` on it. A tiny forwarder thread does the blocking `recv`.
    let (tcmd_tx, mut tcmd_rx) = tokio_mpsc::unbounded_channel::<EngineCommand>();
    std::thread::Builder::new()
        .name("engine-cmd-forward".into())
        .spawn(move || {
            while let Ok(cmd) = cmd_rx.recv() {
                if tcmd_tx.send(cmd).is_err() {
                    break;
                }
            }
        })
        .expect("spawn engine-cmd-forward thread");

    let _ = evt_tx.send(EngineEvent::State(EngineState::Idle));

    while let Some(cmd) = tcmd_rx.recv().await {
        match cmd {
            EngineCommand::Prompt { blocks } => {
                if run_one_turn(
                    &delegate,
                    blocks,
                    &evt_tx,
                    &mut tcmd_rx,
                    &subagents,
                    &next_request_id,
                    &tasks,
                )
                .await
                {
                    break;
                }
            }
            EngineCommand::SetModel { model } => match delegate.set_model(&model) {
                Ok((model, available)) => {
                    let _ = evt_tx.send(EngineEvent::ModelChanged { model, available });
                }
                Err(message) => {
                    let _ = evt_tx.send(EngineEvent::Error { message });
                }
            },
            EngineCommand::SetPermissionMode { mode } => match delegate.set_permission_mode(mode) {
                Ok(()) => {
                    let _ = evt_tx.send(EngineEvent::PermissionModeChanged { mode });
                }
                Err(message) => {
                    let _ = evt_tx.send(EngineEvent::Error { message });
                }
            },
            EngineCommand::SlashCommand { line } => match delegate.handle_slash_command(&line) {
                Ok(text) => {
                    if !text.is_empty() {
                        let _ = evt_tx.send(EngineEvent::Notice { text });
                    }
                }
                Err(message) => {
                    let _ = evt_tx.send(EngineEvent::Error { message });
                }
            },
            EngineCommand::BackgroundTask { action } => {
                handle_background_task(action, &tasks, &evt_tx)
            }
            // No turn is in flight here, so these are stale/no-ops. During a turn
            // they are consumed by the `select!` inside `run_one_turn`.
            EngineCommand::Cancel
            | EngineCommand::PermissionAnswer { .. }
            | EngineCommand::QuestionAnswer { .. } => {}
            EngineCommand::Close => {
                break;
            }
        }
    }
    tasks.shutdown();
    delegate.close();
}

fn handle_background_task(
    action: engine_events::BackgroundTaskAction,
    tasks: &runtime::background_tasks::BackgroundTasks,
    tx: &std_mpsc::Sender<EngineEvent>,
) {
    match action {
        engine_events::BackgroundTaskAction::Watch => {
            let tx = tx.clone();
            tasks.set_sink(move |event| {
                let _ = tx.send(EngineEvent::BackgroundTask(event));
            });
        }
        engine_events::BackgroundTaskAction::Stop { id } => {
            if let Err(message) = tasks.stop(&id) {
                let _ = tx.send(EngineEvent::BackgroundTaskError { id, message });
            }
        }
    }
}

/// Drive a single [`EngineCommand::Prompt`]: install the adapters, run the turn
/// on a blocking task, and concurrently service answer/cancel commands until it
/// finishes. Returns `true` if a [`EngineCommand::Close`] arrived mid-turn, so
/// the caller tears the session down after the (now aborted) turn drains.
async fn run_one_turn(
    delegate: &Arc<dyn EngineDelegate>,
    blocks: Vec<ContentBlock>,
    evt_tx: &std_mpsc::Sender<EngineEvent>,
    tcmd_rx: &mut tokio_mpsc::UnboundedReceiver<EngineCommand>,
    subagents: &SubagentRelay,
    next_request_id: &Arc<AtomicU64>,
    tasks: &runtime::background_tasks::BackgroundTasks,
) -> bool {
    let table = RequestTable {
        next_id: Arc::clone(next_request_id),
        ..RequestTable::default()
    };
    let label = turn_label(&blocks);

    // The question prompter is installed on the tool executor (via the delegate)
    // before the turn; the permission prompter + observer are passed into the
    // blocking turn below.
    delegate.set_question_prompter(Box::new(QuestionAdapter {
        tx: evt_tx.clone(),
        table: table.clone(),
    }));

    let abort = delegate.abort_signal();
    let _ = evt_tx.send(EngineEvent::TurnStarted { label });
    let _ = evt_tx.send(EngineEvent::State(EngineState::Running));

    let delegate = delegate.clone();
    let turn_tx = evt_tx.clone();
    let turn_table = table.clone();
    let subagents = subagents.clone();
    let turn_tasks = tasks.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let mut observer = ObserverAdapter::new(turn_tx.clone())
            .with_subagent_relay(subagents)
            .with_background_tasks(turn_tasks);
        let mut prompter = PrompterAdapter {
            tx: turn_tx,
            table: turn_table,
        };
        delegate.run_turn(blocks, &mut observer, &mut prompter)
    });

    let mut closing = false;
    loop {
        tokio::select! {
            biased;
            cmd = tcmd_rx.recv(), if !closing => match cmd {
                Some(EngineCommand::PermissionAnswer { id, decision }) => {
                    if let Some(PendingAnswer::Permission(tx)) = table.take(id) {
                        let _ = tx.send(decision);
                    }
                }
                Some(EngineCommand::QuestionAnswer { id, answers }) => {
                    if let Some(PendingAnswer::Question(tx)) = table.take(id) {
                        let _ = tx.send(answers);
                    }
                }
                Some(EngineCommand::BackgroundTask { action }) => handle_background_task(action, tasks, evt_tx),
                // Close aborts the turn AND ends the session once it drains.
                Some(EngineCommand::Close) | None => {
                    closing = true;
                    tasks.begin_shutdown();
                    abort.abort();
                    table.cancel_all();
                }
                // Cancel aborts the in-flight turn; it then
                // finishes (cancelled) and we fall through to the `result` arm.
                Some(EngineCommand::Cancel) => {
                    abort.abort();
                    table.cancel_all();
                }
                // A Prompt arriving mid-turn is a renderer bug (renderers serialize
                // turns). Ignore it rather than interleave.
                Some(_) => {}
            },
            result = &mut handle => {
                match result {
                    Ok(Ok(complete)) => {
                        let _ = evt_tx.send(EngineEvent::TurnComplete(complete));
                    }
                    Ok(Err(error)) => {
                        // The event channel carries a string message; the typed
                        // classification is consumed by the direct (ACP) caller,
                        // not this renderer-facing pump.
                        let _ = evt_tx.send(EngineEvent::Error {
                            message: error.to_string(),
                        });
                    }
                    Err(join_error) => {
                        let _ = evt_tx.send(EngineEvent::Error {
                            message: format!("engine turn panicked: {join_error}"),
                        });
                    }
                }
                let _ = evt_tx.send(EngineEvent::State(EngineState::Idle));
                break;
            }
        }
    }
    closing
}

/// Short human label for a turn (first non-empty line of the first text block).
fn turn_label(blocks: &[ContentBlock]) -> String {
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            if let Some(line) = text.lines().find(|l| !l.trim().is_empty()) {
                return line.chars().take(80).collect();
            }
        }
    }
    String::new()
}

/// `RuntimeObserver` → [`EngineEvent`] (fire-and-forget). All seven runtime
/// callbacks map straight to an event.
///
/// Public so an out-of-process renderer that runs its own turn — the ACP
/// server, which drives one [`EngineDelegate::run_turn`] per `session/prompt`
/// — reuses the exact same runtime→event translation the in-process pump uses,
/// instead of re-implementing a `RuntimeObserver` (which would force a renderer
/// crate to name `runtime::RuntimeObserver`, defeating the seam). The ACP path
/// owns the receiving end of `tx` and maps the [`EngineEvent`]s onto its wire.
pub struct ObserverAdapter {
    tx: std_mpsc::Sender<EngineEvent>,
    subagents: Option<SubagentRelay>,
    tasks: runtime::background_tasks::BackgroundTasks,
}

impl ObserverAdapter {
    /// Bind an observer to the same task owner used by the session pump.
    #[must_use]
    pub fn for_delegate(tx: std_mpsc::Sender<EngineEvent>, delegate: &dyn EngineDelegate) -> Self {
        Self::new(tx).with_background_tasks(delegate.background_tasks().unwrap_or_default())
    }

    /// Build an adapter that forwards every runtime callback to `tx` as an
    /// [`EngineEvent`]. `tx` is a plain [`std::sync::mpsc::Sender`] so the
    /// receiving renderer can block-recv without a Tokio runtime (the ACP path
    /// bridges it into its async `select!` via a small forwarder thread, the
    /// same shape the pump uses for its command channel).
    #[must_use]
    pub fn new(tx: std_mpsc::Sender<EngineEvent>) -> Self {
        Self {
            tasks: runtime::background_tasks::BackgroundTasks::default(),
            tx,
            subagents: None,
        }
    }

    /// Reuse one registry across the whole session instead of one turn.
    #[must_use]
    pub fn with_background_tasks(
        mut self,
        tasks: runtime::background_tasks::BackgroundTasks,
    ) -> Self {
        self.tasks = tasks;
        self
    }

    /// Also report what spawned sub-agents do, as [`EngineEvent::Subagent`]
    /// through `relay`. Without this, sub-agents run unobserved.
    #[must_use]
    pub fn with_subagent_relay(mut self, relay: SubagentRelay) -> Self {
        self.subagents = Some(relay);
        self
    }
}

/// Session-lifetime route for [`EngineEvent::Subagent`].
///
/// A background sub-agent outlives the turn that spawned it, so its events
/// cannot ride the turn's event channel alone: that channel has to close when
/// the turn ends (the renderer answers the turn once it drains). The relay
/// sends into the attached turn's channel while one is attached — keeping a
/// synchronous child's events in order with the parent's — and into the
/// session channel returned by [`SubagentRelay::new`] otherwise.
#[derive(Clone)]
pub struct SubagentRelay {
    route: Arc<Mutex<SubagentRoute>>,
}

struct SubagentRoute {
    turn: Option<std_mpsc::Sender<EngineEvent>>,
    session: std_mpsc::Sender<EngineEvent>,
}

impl SubagentRelay {
    /// A relay plus the receiving end of its session channel. The channel
    /// stays open while the relay or any sub-agent it reaches is alive.
    #[must_use]
    pub fn new() -> (Self, std_mpsc::Receiver<EngineEvent>) {
        let (session, rx) = std_mpsc::channel();
        (Self::to_session(session), rx)
    }

    fn to_session(session: std_mpsc::Sender<EngineEvent>) -> Self {
        Self {
            route: Arc::new(Mutex::new(SubagentRoute {
                turn: None,
                session,
            })),
        }
    }

    /// Route events into `turn` until the returned guard drops. Drop the guard
    /// before waiting for `turn`'s channel to close.
    #[must_use]
    pub fn attach_turn(&self, turn: std_mpsc::Sender<EngineEvent>) -> SubagentTurnGuard {
        self.lock().turn = Some(turn);
        SubagentTurnGuard {
            relay: self.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SubagentRoute> {
        self.route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, event: runtime::SubagentEvent) {
        let route = self.lock();
        let event = EngineEvent::Subagent(event);
        let event = match &route.turn {
            Some(turn) => match turn.send(event) {
                Ok(()) => return,
                Err(std_mpsc::SendError(event)) => event,
            },
            None => event,
        };
        let _ = route.session.send(event);
    }
}

/// Detaches the turn from a [`SubagentRelay`] on drop.
pub struct SubagentTurnGuard {
    relay: SubagentRelay,
}

impl Drop for SubagentTurnGuard {
    fn drop(&mut self) {
        self.relay.lock().turn = None;
    }
}

impl RuntimeObserver for ObserverAdapter {
    fn on_compaction(&mut self, event: &runtime::CompactionProgress) {
        let _ = self.tx.send(EngineEvent::Compaction(event.clone()));
    }

    fn on_notice(&mut self, text: &str) {
        let _ = self.tx.send(EngineEvent::Notice { text: text.into() });
    }

    fn on_thinking_delta(&mut self, delta: &str) {
        let _ = self
            .tx
            .send(EngineEvent::ThinkingDelta { text: delta.into() });
    }

    fn on_text_delta(&mut self, delta: &str) {
        let _ = self.tx.send(EngineEvent::TextDelta { text: delta.into() });
    }

    fn on_tool_use(&mut self, id: &str, name: &str, input: &str) {
        let _ = self.tx.send(EngineEvent::ToolCall {
            id: id.into(),
            name: name.into(),
            input: input.into(),
        });
    }

    fn on_tool_started(&mut self, id: &str, name: &str, input: &str) {
        let _ = self.tx.send(EngineEvent::ToolStarted {
            id: id.into(),
            name: name.into(),
            input: input.into(),
        });
    }

    fn on_tool_result(&mut self, tool_use_id: &str, tool_name: &str, output: &str, is_error: bool) {
        let _ = self.tx.send(EngineEvent::ToolResult {
            id: tool_use_id.into(),
            name: tool_name.into(),
            output: output.into(),
            is_error,
        });
    }

    fn on_model(&mut self, wire_model: &str) {
        let _ = self.tx.send(EngineEvent::ModelResolved {
            wire_model: wire_model.into(),
        });
    }

    fn on_usage(&mut self, usage: &TokenUsage) {
        let _ = self.tx.send(EngineEvent::Usage(*usage));
    }

    fn on_prompt_cache(&mut self, event: &runtime::PromptCacheEvent) {
        let _ = self.tx.send(EngineEvent::PromptCache(event.clone()));
    }

    fn on_permission_denied(&mut self, id: &str, name: &str, input: &str, reason: &str) {
        let _ = self.tx.send(EngineEvent::PermissionDenied {
            id: id.into(),
            name: name.into(),
            input: input.into(),
            reason: reason.into(),
        });
    }

    fn on_message_stop(&mut self) {
        let _ = self.tx.send(EngineEvent::MessageComplete);
    }

    fn tool_progress_sink(&self) -> Option<runtime::ProgressSink> {
        // Live tool progress fires from deep inside tool execution, off this
        // thread, so it can't ride the `&mut self` hooks above. Hand the runtime
        // a `Send + Sync` sink that forwards to the same session event channel.
        // `std::sync::mpsc::Sender` is `Send` but not `Sync`, so wrap it so the
        // closure satisfies `ProgressSink`'s `Send + Sync` bound.
        let tx = Arc::new(Mutex::new(self.tx.clone()));
        Some(runtime::ProgressSink::new(move |event| {
            let _ = tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(EngineEvent::ToolProgress(event));
        }))
    }

    fn hook_progress_sink(&self) -> Option<runtime::HookProgressSink> {
        // Plugin-hook progress fires from the pre/post-tool hook runners, which
        // the runtime forwards through a `Send + Sync` reporter — so, exactly as
        // for `tool_progress_sink`, hand it a sink wrapping the same session
        // event channel (`std::sync::mpsc::Sender` is `Send` but not `Sync`, so
        // wrap it to satisfy the `Send + Sync` bound).
        let tx = Arc::new(Mutex::new(self.tx.clone()));
        Some(runtime::HookProgressSink::new(move |event| {
            let _ = tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(EngineEvent::HookProgress(event));
        }))
    }

    fn retry_sink(&self) -> Option<runtime::RetrySink> {
        // Same shape and same reason as `hook_progress_sink`: the emitter is
        // the HTTP transport on its own task, so the channel is wrapped to be
        // `Send + Sync`. Reporting retries through the seam is what stops the
        // transport writing to the terminal behind the renderer's back.
        let tx = Arc::new(Mutex::new(self.tx.clone()));
        Some(runtime::RetrySink::new(move |event| {
            let _ = tx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .send(EngineEvent::Retry(event));
        }))
    }

    fn subagent_sink(&self) -> Option<runtime::SubagentSink> {
        let relay = self.subagents.clone()?;
        Some(runtime::SubagentSink::new(move |event| relay.emit(event)))
    }

    fn background_tasks(&self) -> Option<runtime::background_tasks::BackgroundTasks> {
        Some(self.tasks.clone())
    }
}

/// Emit [`EngineEvent::PermissionRequest`] and await a oneshot until the pump routes the matching
/// [`EngineCommand::PermissionAnswer`] back.
struct PrompterAdapter {
    tx: std_mpsc::Sender<EngineEvent>,
    table: RequestTable,
}

impl PermissionPrompter for PrompterAdapter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        let reply = self.begin_decision(request);
        tokio::task::block_in_place(|| futures::executor::block_on(reply))
    }

    fn begin_decision(
        &mut self,
        request: &PermissionRequest,
    ) -> runtime::PromptReply<PermissionPromptDecision> {
        let table = self.table.clone();
        let tx = self.tx.clone();
        let request = request.clone();
        self.table.interaction.enqueue(move || async move {
            let (answer_tx, answer_rx) = oneshot::channel();
            let _lease = table.show_prompt(&tx, PendingAnswer::Permission(answer_tx), |id| {
                EngineEvent::PermissionRequest { id, request }
            });
            answer_rx
                .await
                .unwrap_or_else(|_| PermissionPromptDecision::Deny {
                    reason: "engine session closed before the permission prompt was answered"
                        .into(),
                })
        })
    }
}

/// Questions and permissions share one interaction queue. Waiting for a reply
/// yields to other tools; only ownership of the input surface is exclusive.
struct QuestionAdapter {
    tx: std_mpsc::Sender<EngineEvent>,
    table: RequestTable,
}

impl QuestionPrompter for QuestionAdapter {
    fn ask(
        &mut self,
        request: &QuestionPromptRequest,
    ) -> Result<Vec<QuestionPromptAnswer>, String> {
        let reply = self.begin_question(request);
        tokio::task::block_in_place(|| futures::executor::block_on(reply))
    }

    fn begin_question(
        &mut self,
        request: &QuestionPromptRequest,
    ) -> runtime::PromptReply<Result<Vec<QuestionPromptAnswer>, String>> {
        let table = self.table.clone();
        let tx = self.tx.clone();
        let request = request.clone();
        self.table.interaction.enqueue(move || async move {
            let (answer_tx, answer_rx) = oneshot::channel();
            let _lease = table.show_prompt(&tx, PendingAnswer::Question(answer_tx), |id| {
                EngineEvent::QuestionRequest { id, request }
            });
            answer_rx.await.unwrap_or_else(|_| {
                Err("engine session closed before the question was answered".to_string())
            })
        })
    }
}

/// Releases input ownership and removes an unanswered request even if its
/// future is dropped on cancellation or a provider stream failure.
struct PromptLease {
    table: RequestTable,
    tx: std_mpsc::Sender<EngineEvent>,
    id: RequestId,
}

impl Drop for PromptLease {
    fn drop(&mut self) {
        self.table.take(self.id);
        let _ = self.tx.send(EngineEvent::State(EngineState::Running));
    }
}

impl RequestTable {
    fn show_prompt(
        &self,
        tx: &std_mpsc::Sender<EngineEvent>,
        answer: PendingAnswer,
        event: impl FnOnce(RequestId) -> EngineEvent,
    ) -> PromptLease {
        let id = self.alloc();
        if !self.cancelled.load(Ordering::SeqCst) {
            self.insert(id, answer);
            let _ = tx.send(EngineEvent::State(EngineState::AwaitingInput));
            let _ = tx.send(event(id));
        }
        PromptLease {
            table: self.clone(),
            tx: tx.clone(),
            id,
        }
    }
}
