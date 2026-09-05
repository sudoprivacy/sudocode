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
//! # The one subtlety: synchronous prompts on an async pump
//!
//! A turn's permission / question prompts are **synchronous** callbacks
//! (`PermissionPrompter::decide` must return a decision inline so the tool loop
//! can proceed), but the *answer* arrives asynchronously as an
//! [`EngineCommand::PermissionAnswer`] on the command channel. The bridge — a
//! monotonic [`RequestId`], a `HashMap<RequestId, oneshot::Sender>` awaiting
//! table, and `block_in_place(|| rx.blocking_recv())` — is entirely internal to
//! this module (the proven mechanism copied from the ACP `AcpPermissionBridge`).
//! Renderers and delegates never see it.
//!
//! Because `block_in_place` requires a multi-threaded Tokio worker, the
//! **contract on [`EngineDelegate::run_turn`]** is: run the turn on a
//! multi-threaded Tokio runtime (i.e. `your_runtime.block_on(conversation
//! .run_turn(...))` where `your_runtime` is multi-thread). The existing CLI /
//! ACP delegate already does exactly this.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc as tokio_mpsc;
use tokio::sync::oneshot;

use engine_events::{
    ContentBlock, EngineCommand, EngineEvent, EngineState, PermissionMode,
    PermissionPromptDecision, PermissionRequest, QuestionPromptAnswer, QuestionPromptRequest,
    RequestId, TurnComplete,
};
use runtime::{HookAbortSignal, PermissionPrompter, QuestionPrompter, RuntimeObserver, TokenUsage};

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
    /// Returns the end-of-turn aggregate, or a renderer-facing error string.
    fn run_turn(
        &self,
        blocks: Vec<ContentBlock>,
        observer: &mut dyn RuntimeObserver,
        prompter: &mut dyn PermissionPrompter,
    ) -> Result<TurnComplete, String>;

    /// Install the question prompter the `AskUserQuestion` tool uses for the
    /// *next* turn. The driver calls this immediately before each `run_turn`.
    fn set_question_prompter(&self, prompter: Box<dyn QuestionPrompter>);

    /// A clone of the in-flight turn's abort signal. The pump fires it on
    /// [`EngineCommand::Cancel`] while `run_turn` is blocked.
    fn abort_signal(&self) -> HookAbortSignal;

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

        std::thread::Builder::new()
            .name("engine-session".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("engine-session tokio runtime");
                rt.block_on(drive(delegate, cmd_rx, evt_tx));
            })
            .expect("spawn engine-session thread");

        EngineHandle {
            commands: cmd_tx,
            events: evt_rx,
        }
    }
}

/// One in-flight request awaiting a renderer answer. Keyed by [`RequestId`] in
/// the shared table so the pump can route the matching answer command back to
/// the blocked prompter.
enum PendingAnswer {
    Permission(oneshot::Sender<PermissionPromptDecision>),
    Question(oneshot::Sender<Vec<QuestionPromptAnswer>>),
}

/// Shared awaiting table + id allocator, threaded through the prompt adapters
/// and the pump for a single turn.
#[derive(Clone, Default)]
struct RequestTable {
    pending: Arc<Mutex<HashMap<RequestId, PendingAnswer>>>,
    next_id: Arc<AtomicU64>,
}

impl RequestTable {
    fn alloc(&self) -> RequestId {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn insert(&self, id: RequestId, answer: PendingAnswer) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, answer);
    }

    fn take(&self, id: RequestId) -> Option<PendingAnswer> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
    }
}

/// The command loop. Runs on the engine thread's Tokio runtime.
async fn drive(
    delegate: Arc<dyn EngineDelegate>,
    cmd_rx: std_mpsc::Receiver<EngineCommand>,
    evt_tx: std_mpsc::Sender<EngineEvent>,
) {
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
                if run_one_turn(&delegate, blocks, &evt_tx, &mut tcmd_rx).await {
                    delegate.close();
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
            // No turn is in flight here, so these are stale/no-ops. During a turn
            // they are consumed by the `select!` inside `run_one_turn`.
            EngineCommand::Cancel
            | EngineCommand::PermissionAnswer { .. }
            | EngineCommand::QuestionAnswer { .. } => {}
            EngineCommand::Close => {
                delegate.close();
                break;
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
) -> bool {
    let table = RequestTable::default();
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
    let mut handle = tokio::task::spawn_blocking(move || {
        let mut observer = ObserverAdapter {
            tx: turn_tx.clone(),
        };
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
            cmd = tcmd_rx.recv() => match cmd {
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
                // Close aborts the turn AND ends the session once it drains.
                Some(EngineCommand::Close) => {
                    closing = true;
                    abort.abort();
                }
                // Cancel / a dropped channel abort the in-flight turn; it then
                // finishes (cancelled) and we fall through to the `result` arm.
                Some(EngineCommand::Cancel) | None => abort.abort(),
                // A Prompt arriving mid-turn is a renderer bug (renderers serialize
                // turns). Ignore it rather than interleave.
                Some(_) => {}
            },
            result = &mut handle => {
                match result {
                    Ok(Ok(complete)) => {
                        let _ = evt_tx.send(EngineEvent::TurnComplete(complete));
                    }
                    Ok(Err(message)) => {
                        let _ = evt_tx.send(EngineEvent::Error { message });
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
struct ObserverAdapter {
    tx: std_mpsc::Sender<EngineEvent>,
}

impl RuntimeObserver for ObserverAdapter {
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

    fn on_message_stop(&mut self) {
        // MessageStop has no distinct renderer effect on its own; the turn
        // boundary is carried by TurnComplete.
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
}

/// `PermissionPrompter::decide` → emit [`EngineEvent::PermissionRequest`], park
/// on a oneshot until the pump routes the matching
/// [`EngineCommand::PermissionAnswer`] back.
struct PrompterAdapter {
    tx: std_mpsc::Sender<EngineEvent>,
    table: RequestTable,
}

impl PermissionPrompter for PrompterAdapter {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        let id = self.table.alloc();
        let (answer_tx, answer_rx) = oneshot::channel();
        self.table.insert(id, PendingAnswer::Permission(answer_tx));

        let _ = self.tx.send(EngineEvent::State(EngineState::AwaitingInput));
        let _ = self.tx.send(EngineEvent::PermissionRequest {
            id,
            request: request.clone(),
        });

        // Park this tool-loop worker until the renderer answers. `block_in_place`
        // hands the worker back to Tokio so the rest of the runtime keeps making
        // progress; the answer arrives from the pump's separate runtime.
        let decision = tokio::task::block_in_place(|| answer_rx.blocking_recv()).unwrap_or(
            PermissionPromptDecision::Deny {
                reason: "engine session closed before the permission prompt was answered".into(),
            },
        );

        let _ = self.tx.send(EngineEvent::State(EngineState::Running));
        decision
    }
}

/// `QuestionPrompter::ask` → emit [`EngineEvent::QuestionRequest`], park on a
/// oneshot until the pump routes the matching [`EngineCommand::QuestionAnswer`].
struct QuestionAdapter {
    tx: std_mpsc::Sender<EngineEvent>,
    table: RequestTable,
}

impl QuestionPrompter for QuestionAdapter {
    fn ask(
        &mut self,
        request: &QuestionPromptRequest,
    ) -> Result<Vec<QuestionPromptAnswer>, String> {
        let id = self.table.alloc();
        let (answer_tx, answer_rx) = oneshot::channel();
        self.table.insert(id, PendingAnswer::Question(answer_tx));

        let _ = self.tx.send(EngineEvent::State(EngineState::AwaitingInput));
        let _ = self.tx.send(EngineEvent::QuestionRequest {
            id,
            request: request.clone(),
        });

        let answers = tokio::task::block_in_place(|| answer_rx.blocking_recv())
            .map_err(|_| "engine session closed before the question was answered".to_string())?;

        let _ = self.tx.send(EngineEvent::State(EngineState::Running));
        Ok(answers)
    }
}
