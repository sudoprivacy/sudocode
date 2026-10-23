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
//!   ACP   ──▶ (send EngineCommand, recv EngineEvent)   (the pump)      (engine-host)
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
use std::sync::{Arc, Mutex, OnceLock};

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

use crate::subagent_completion::{CompletionState, EventSink};

/// The engine-side contract: one live session's worth of "things a turn can do".
///
/// Implement this to plug an engine into the seam. `engine-host` implements it
/// over its `ConversationRuntime`; CLI and ACP share the implementation. Held as
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
    /// * preserve the abort signal reset by the pump before input preparation;
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

    /// Prepare model-specific input after admission, before the model turn.
    fn prepare_prompt(
        &self,
        blocks: Vec<ContentBlock>,
        _image_policy: engine_events::ImageInputPolicy,
    ) -> Result<Vec<ContentBlock>, RuntimeError> {
        Ok(blocks)
    }

    /// Execute session management under the same admission and commit boundary.
    fn run_management(
        &self,
        _line: &str,
        _build: &engine_events::ManagementContext,
        _observer: &mut dyn RuntimeObserver,
    ) -> Result<bool, RuntimeError> {
        Err(RuntimeError::new("session management is unsupported"))
    }

    /// Attach completion persistence during engine composition, before execution.
    fn attach_completions(&self, _state: &CompletionState) -> Result<(), String> {
        Ok(())
    }

    /// Run one tool by name outside any turn, through the hooks and permission
    /// policy a model's call passes, without writing to the conversation.
    /// Returns the tool's output and whether it failed.
    fn run_direct_tool(
        &self,
        _name: &str,
        _input: &str,
        _prompter: &mut dyn PermissionPrompter,
    ) -> Result<(String, bool), String> {
        Err("this engine does not run direct tool calls".into())
    }

    /// Bind a host's trace identity after turn admission succeeds.
    fn set_trace_id(&self, _trace_id: &str) {}

    /// Install the question prompter the `AskUserQuestion` tool uses for the
    /// *next* turn. The driver calls this immediately before each `run_turn`.
    fn set_question_prompter(&self, prompter: Box<dyn QuestionPrompter>);

    /// A clone of the in-flight turn's abort signal. The pump fires it on
    /// [`EngineCommand::Cancel`] while `run_turn` is blocked. This accessor must
    /// not acquire the session mutex held by the work being cancelled.
    fn abort_signal(&self) -> HookAbortSignal;

    /// One registry owned by the engine session, shared by every turn driver.
    fn background_tasks(&self) -> Option<runtime::background_tasks::BackgroundTasks> {
        None
    }

    /// Fence every main and child tool before cancellation. The host waits for
    /// the turn and actual workers before acknowledging human ownership.
    fn begin_tool_pause(&self) -> Result<u64, String> {
        self.background_tasks()
            .map(|tasks| tasks.begin_pause())
            .ok_or_else(|| "This engine does not support tool pause".to_string())
    }

    fn wait_tool_pause(&self, epoch: u64, timeout: std::time::Duration) -> Result<(), String> {
        self.background_tasks()
            .ok_or_else(|| "This engine does not support tool pause".to_string())?
            .wait_paused(epoch, timeout)
    }

    fn resume_tool_operations(&self, epoch: u64) -> Result<(), String> {
        self.background_tasks()
            .ok_or_else(|| "This engine does not support tool pause".to_string())?
            .resume_operations(epoch)
    }

    fn tool_control(&self) -> (u64, bool) {
        self.background_tasks()
            .map_or((0, false), |tasks| tasks.tool_control())
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

/// The pump. Spawns a dedicated engine thread on the shared Tokio executor, routes
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
        Self::spawn_with_completions(delegate, Arc::default())
    }

    /// Use the same driver with a durable completion journal supplied by the host.
    #[must_use]
    pub(crate) fn spawn_with_completions(
        delegate: Arc<dyn EngineDelegate>,
        completions: Arc<CompletionState>,
    ) -> EngineHandle {
        let (cmd_tx, cmd_rx) = std_mpsc::channel::<EngineCommand>();
        let (evt_tx, evt_rx) = std_mpsc::channel::<EngineEvent>();
        let (stopped_tx, stopped_rx) = std_mpsc::channel();

        std::thread::Builder::new()
            .name("engine-session".into())
            .spawn(move || {
                // One executor for every session, including multi-session ACP.
                // A new renderer must not multiply worker pools per conversation.
                static EXECUTOR: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
                let runtime = EXECUTOR.get_or_init(|| {
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                        .expect("engine-session tokio runtime")
                });
                runtime.block_on(drive(delegate, cmd_rx, EventSink::new(evt_tx, completions)));
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

#[derive(Clone, Copy)]
enum ShutdownReason {
    ExplicitClose,
    Disconnected,
}

/// The command loop. Runs on the engine thread's Tokio runtime.
async fn drive(
    delegate: Arc<dyn EngineDelegate>,
    cmd_rx: std_mpsc::Receiver<EngineCommand>,
    evt_tx: EventSink,
) {
    let subagents = SubagentRelay::new(evt_tx.clone());

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

    let mut shutdown = ShutdownReason::Disconnected;
    loop {
        let cmd = tokio::select! {
            cmd = tcmd_rx.recv() => { let Some(cmd) = cmd else { break; }; cmd },
            () = evt_tx.completions.delivery_changed() => { evt_tx.completions.deliver(&evt_tx); continue; }
        };
        let tasks = delegate.background_tasks().unwrap_or_default();
        match cmd {
            EngineCommand::Completion { id, action } => handle_completion(id, action, &evt_tx),
            EngineCommand::CancelSubagent {
                request_id,
                agent_id,
            } => handle_subagent_cancel(request_id, &agent_id, &evt_tx),
            command @ (EngineCommand::Prompt { .. } | EngineCommand::Management { .. }) => {
                let (input, context) = TurnInput::from_command(command);
                if let Err(message) = evt_tx.completions.begin(context.as_ref()) {
                    // Admission failed before the turn began; do not mutate the
                    // previous completion transaction.
                    let _ = evt_tx.reject_prompt(message);
                    continue;
                }
                if let Some(context) = &context {
                    delegate.set_trace_id(&context.trace_id);
                }
                if let Some(reason) = run_one_turn(
                    &delegate,
                    input,
                    &evt_tx,
                    &mut tcmd_rx,
                    &subagents,
                    &next_request_id,
                    &tasks,
                )
                .await
                {
                    shutdown = reason;
                    break;
                }
            }
            command @ (EngineCommand::SetModel { .. }
            | EngineCommand::SetPermissionMode { .. }
            | EngineCommand::SlashCommand { .. }) => {
                change_setting(delegate.as_ref(), command, &evt_tx);
            }
            EngineCommand::BackgroundTask { action } => {
                handle_background_task(action, &tasks, &evt_tx);
            }
            EngineCommand::DirectTool {
                request_id,
                name,
                input,
            } => {
                let call = DirectCall {
                    request_id,
                    name,
                    input,
                };
                if let Some(reason) = run_direct_tool(
                    &delegate,
                    call,
                    &evt_tx,
                    &mut tcmd_rx,
                    &next_request_id,
                    &tasks,
                )
                .await
                {
                    shutdown = reason;
                    break;
                }
            }
            // Cancellation also reaches a lifecycle operation, such as manual
            // compaction, that holds the session's shared abort signal.
            EngineCommand::Cancel => delegate.abort_signal().abort(),
            EngineCommand::PermissionAnswer { .. } | EngineCommand::QuestionAnswer { .. } => {}
            EngineCommand::Close => {
                shutdown = ShutdownReason::ExplicitClose;
                break;
            }
        }
    }
    if let Some(tasks) = delegate.background_tasks() {
        tasks.shutdown();
    }
    // Always reap owned work, but preserve the existing persistence contract:
    // dropping the handle after a read-only command or failed compaction must
    // not save a newly assembled prompt snapshot into its resumed transcript.
    if matches!(shutdown, ShutdownReason::ExplicitClose) {
        delegate.close();
    }
}

fn change_setting(delegate: &dyn EngineDelegate, command: EngineCommand, sink: &EventSink) {
    let result = match command {
        EngineCommand::SetModel { model } => delegate
            .set_model(&model)
            .map(|(model, available)| Some(EngineEvent::ModelChanged { model, available })),
        EngineCommand::SetPermissionMode { mode } => delegate
            .set_permission_mode(mode)
            .map(|()| Some(EngineEvent::PermissionModeChanged { mode })),
        EngineCommand::SlashCommand { line } => delegate
            .handle_slash_command(&line)
            .map(|text| (!text.is_empty()).then_some(EngineEvent::Notice { text })),
        _ => unreachable!("setting command"),
    };
    match result {
        Ok(Some(event)) => {
            let _ = sink.send(event);
        }
        Ok(None) => {}
        Err(message) => {
            let _ = sink.send(EngineEvent::Error { message });
        }
    }
}

fn handle_completion(id: Option<u64>, action: engine_events::CompletionAction, sink: &EventSink) {
    let result = sink.completions.command(action);
    if let Some(id) = id {
        let _ = sink.send(EngineEvent::CommandResult { id, result });
    }
}

fn handle_subagent_cancel(id: u64, agent_id: &str, sink: &EventSink) {
    let result = sink.completions.cancel_subagent(agent_id);
    let _ = sink.send(EngineEvent::CommandResult { id, result });
}

fn handle_background_task(
    action: engine_events::BackgroundTaskAction,
    tasks: &runtime::background_tasks::BackgroundTasks,
    tx: &EventSink,
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

struct TurnInput {
    blocks: Vec<ContentBlock>,
    management: Option<(String, engine_events::ManagementContext)>,
    image_policy: engine_events::ImageInputPolicy,
}

impl TurnInput {
    fn from_command(command: EngineCommand) -> (Self, Option<engine_events::PromptContext>) {
        let (blocks, context, management) = match command {
            EngineCommand::Prompt { blocks, context } => (blocks, context, None),
            EngineCommand::Management {
                line,
                context,
                build,
            } => (Vec::new(), context, Some((line, build))),
            _ => unreachable!(),
        };
        let image_policy = context
            .as_ref()
            .map_or(engine_events::ImageInputPolicy::Native, |context| {
                context.image_policy
            });
        (
            Self {
                blocks,
                management,
                image_policy,
            },
            context,
        )
    }
}

/// Drive a single [`EngineCommand::Prompt`]: install the adapters, run the turn
/// on a blocking task, and concurrently service answer/cancel commands until it
/// finishes. Returns the shutdown reason if a close or disconnection arrived
/// mid-turn, so the caller tears the session down after the aborted turn drains.
async fn run_one_turn(
    delegate: &Arc<dyn EngineDelegate>,
    input: TurnInput,
    evt_tx: &EventSink,
    tcmd_rx: &mut tokio_mpsc::UnboundedReceiver<EngineCommand>,
    subagents: &SubagentRelay,
    next_request_id: &Arc<AtomicU64>,
    tasks: &runtime::background_tasks::BackgroundTasks,
) -> Option<ShutdownReason> {
    let table = RequestTable {
        next_id: Arc::clone(next_request_id),
        ..RequestTable::default()
    };
    let label = input.management.as_ref().map_or_else(
        || turn_label(&input.blocks),
        |(line, _)| line.chars().take(80).collect(),
    );

    // The question prompter is installed on the tool executor (via the delegate)
    // before the turn; the permission prompter + observer are passed into the
    // blocking turn below.
    delegate.set_question_prompter(Box::new(QuestionAdapter {
        tx: evt_tx.clone(),
        table: table.clone(),
    }));

    let abort = delegate.abort_signal();
    abort.reset();
    let _ = evt_tx.send(EngineEvent::TurnStarted { label });
    let _ = evt_tx.send(EngineEvent::State(EngineState::Running));

    let delegate = delegate.clone();
    let turn_tx = evt_tx.clone();
    let turn_table = table.clone();
    let subagents = subagents.clone();
    let turn_tasks = tasks.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let mut observer = ObserverAdapter::with_sink(turn_tx.clone())
            .with_subagent_relay(subagents)
            .with_background_tasks(turn_tasks);
        let mut prompter = PrompterAdapter {
            tx: turn_tx,
            table: turn_table,
        };
        execute_input(delegate.as_ref(), input, &mut observer, &mut prompter)
    });

    let mut shutdown = None;
    loop {
        tokio::select! {
            biased;
            cmd = tcmd_rx.recv(), if shutdown.is_none() => match cmd {
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
                Some(EngineCommand::Completion { id, action }) => handle_completion(id, action, evt_tx),
                Some(EngineCommand::CancelSubagent { request_id, agent_id }) => handle_subagent_cancel(request_id, &agent_id, evt_tx),
                // Both paths abort and reap; only explicit Close persists.
                command @ (Some(EngineCommand::Close) | None) => {
                    shutdown = Some(if command.is_some() {
                        ShutdownReason::ExplicitClose
                    } else {
                        ShutdownReason::Disconnected
                    });
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
                Some(EngineCommand::DirectTool { request_id, .. }) => refuse_busy(request_id, evt_tx),
                // A Prompt arriving mid-turn is a renderer bug (renderers serialize
                // turns). Ignore it rather than interleave.
                Some(_) => {}
            },
            () = evt_tx.completions.delivery_changed() => evt_tx.completions.deliver(evt_tx),
            result = &mut handle => {
                table.cancel_all();
                match result {
                    Ok(Ok(complete)) => {
                        let _ = evt_tx.send(EngineEvent::TurnComplete(complete));
                    }
                    Ok(Err(error)) => {
                        let _ = evt_tx.send(EngineEvent::TurnFailed {
                            message: error.to_string(),
                            context_window: error.is_context_window_blocked(),
                        });
                    }
                    Err(join_error) => {
                        let _ = evt_tx.send(EngineEvent::TurnFailed {
                            message: format!("engine turn panicked: {join_error}"),
                            context_window: false,
                        });
                    }
                }
                let _ = evt_tx.send(EngineEvent::State(EngineState::Idle));
                break;
            }
        }
    }
    shutdown
}

/// One [`EngineCommand::DirectTool`].
struct DirectCall {
    request_id: u64,
    name: String,
    input: String,
}

/// A direct call that arrives while other work holds the engine is refused, not
/// queued: nothing ran, so the caller has a definite answer.
fn refuse_busy(request_id: u64, sink: &EventSink) {
    let _ = sink.send(EngineEvent::CommandResult {
        id: request_id,
        result: Err("the session is busy; nothing was run".into()),
    });
}

/// Run one [`EngineCommand::DirectTool`] on a blocking task while servicing its
/// permission prompts through the same request table a turn uses. Returns the
/// shutdown reason if a close or disconnection arrived meanwhile.
async fn run_direct_tool(
    delegate: &Arc<dyn EngineDelegate>,
    call: DirectCall,
    evt_tx: &EventSink,
    tcmd_rx: &mut tokio_mpsc::UnboundedReceiver<EngineCommand>,
    next_request_id: &Arc<AtomicU64>,
    tasks: &runtime::background_tasks::BackgroundTasks,
) -> Option<ShutdownReason> {
    let table = RequestTable {
        next_id: Arc::clone(next_request_id),
        ..RequestTable::default()
    };
    let abort = delegate.abort_signal();
    abort.reset();
    let DirectCall {
        request_id,
        name,
        input,
    } = call;
    let worker = Arc::clone(delegate);
    let mut prompter = PrompterAdapter {
        tx: evt_tx.clone(),
        table: table.clone(),
    };
    let mut handle =
        tokio::task::spawn_blocking(move || worker.run_direct_tool(&name, &input, &mut prompter));
    let mut shutdown = None;
    loop {
        tokio::select! {
            biased;
            cmd = tcmd_rx.recv(), if shutdown.is_none() => match cmd {
                Some(EngineCommand::PermissionAnswer { id, decision }) => {
                    if let Some(PendingAnswer::Permission(tx)) = table.take(id) {
                        let _ = tx.send(decision);
                    }
                }
                Some(EngineCommand::DirectTool { request_id, .. }) => refuse_busy(request_id, evt_tx),
                Some(EngineCommand::BackgroundTask { action }) => handle_background_task(action, tasks, evt_tx),
                Some(EngineCommand::Completion { id, action }) => handle_completion(id, action, evt_tx),
                Some(EngineCommand::CancelSubagent { request_id, agent_id }) => handle_subagent_cancel(request_id, &agent_id, evt_tx),
                command @ (Some(EngineCommand::Close) | None) => {
                    shutdown = Some(if command.is_some() {
                        ShutdownReason::ExplicitClose
                    } else {
                        ShutdownReason::Disconnected
                    });
                    abort.abort();
                    table.cancel_all();
                }
                Some(EngineCommand::Cancel) => {
                    abort.abort();
                    table.cancel_all();
                }
                // The host serializes turns and direct calls on one lane, so a
                // prompt arriving here is a renderer bug. Ignore it, as a turn
                // does, rather than interleave.
                Some(_) => {}
            },
            result = &mut handle => {
                table.cancel_all();
                let result = match result {
                    Ok(Ok((content, is_error))) => Ok(serde_json::json!({
                        "content": content,
                        "isError": is_error,
                    })),
                    Ok(Err(error)) => Err(error),
                    Err(join_error) => Err(format!("direct tool call panicked: {join_error}")),
                };
                // A permission prompt moves the reported state; the session is
                // idle again either way. Say so before the reply, while the
                // caller still holds the session's event listener, so the next
                // turn cannot receive this call's state.
                let _ = evt_tx.send(EngineEvent::State(EngineState::Idle));
                let _ = evt_tx.send(EngineEvent::CommandResult { id: request_id, result });
                break;
            }
        }
    }
    shutdown
}

fn execute_input(
    delegate: &dyn EngineDelegate,
    input: TurnInput,
    observer: &mut dyn RuntimeObserver,
    prompter: &mut dyn PermissionPrompter,
) -> Result<TurnComplete, RuntimeError> {
    if let Some((line, build)) = input.management {
        let cancelled = delegate.run_management(&line, &build, observer)?;
        return Ok(TurnComplete {
            iterations: 0,
            turn_usage: TokenUsage::default(),
            session_usage: TokenUsage::default(),
            cancelled,
            response_model: None,
            auto_compaction: None,
        });
    }
    let blocks = delegate.prepare_prompt(input.blocks, input.image_policy)?;
    delegate.run_turn(blocks, observer, prompter)
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
/// Only the shared driver constructs observers, for model and management work.
struct ObserverAdapter {
    tx: EventSink,
    subagents: Option<SubagentRelay>,
    tasks: runtime::background_tasks::BackgroundTasks,
}

impl ObserverAdapter {
    fn with_sink(tx: EventSink) -> Self {
        Self {
            tasks: runtime::background_tasks::BackgroundTasks::default(),
            tx,
            subagents: None,
        }
    }

    /// Reuse one registry across the whole session instead of one turn.
    #[must_use]
    fn with_background_tasks(mut self, tasks: runtime::background_tasks::BackgroundTasks) -> Self {
        self.tasks = tasks;
        self
    }

    /// Also report what spawned sub-agents do, as [`EngineEvent::Subagent`]
    /// through `relay`. Without this, sub-agents run unobserved.
    #[must_use]
    fn with_subagent_relay(mut self, relay: SubagentRelay) -> Self {
        self.subagents = Some(relay);
        self
    }
}

/// One session event route, retained by children that outlive their parent turn.
#[derive(Clone)]
struct SubagentRelay {
    tx: EventSink,
}

impl SubagentRelay {
    fn new(tx: EventSink) -> Self {
        Self { tx }
    }

    fn emit(&self, event: runtime::SubagentEvent) {
        let _ = self.tx.send(EngineEvent::Subagent(event));
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
        let journal = relay.tx.completions.journal();
        Some(
            runtime::SubagentSink::new(move |event| relay.emit(event))
                .with_completion_journal(journal),
        )
    }

    fn background_tasks(&self) -> Option<runtime::background_tasks::BackgroundTasks> {
        let journal = self
            .subagents
            .as_ref()
            .and_then(|relay| relay.tx.completions.journal());
        Some(self.tasks.clone().with_completion(journal))
    }
}

/// Emit [`EngineEvent::PermissionRequest`] and await a oneshot until the pump routes the matching
/// [`EngineCommand::PermissionAnswer`] back.
#[derive(Clone)]
struct PrompterAdapter {
    tx: EventSink,
    table: RequestTable,
}

impl PermissionPrompter for PrompterAdapter {
    fn delegation_sink(&self) -> Option<runtime::PermissionPromptSink> {
        let bridge = self.clone();
        Some(runtime::PermissionPromptSink::new(move |request| {
            bridge.clone().begin_decision(&request)
        }))
    }

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
    tx: EventSink,
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
    tx: EventSink,
    id: RequestId,
}

impl Drop for PromptLease {
    fn drop(&mut self) {
        self.table.take(self.id);
        if !self.table.cancelled.load(Ordering::SeqCst) {
            let _ = self.tx.send(EngineEvent::State(EngineState::Running));
        }
    }
}

impl RequestTable {
    fn show_prompt(
        &self,
        tx: &EventSink,
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
