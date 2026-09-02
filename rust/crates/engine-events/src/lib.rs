//! The sole engine↔renderer seam for the sudocode (`scode`) engine.
//!
//! # Why this crate exists
//!
//! One engine (the sudocode Rust runtime) historically grew 3–4 mutually
//! incompatible translation layers because there was **no single internal
//! event abstraction**: turn progress was derived independently by three code
//! paths from three different internal representations
//! (`api::StreamEvent`, `runtime::AssistantEvent`, `runtime::TurnSummary`),
//! and each external consumer bolted on its own decoder.
//!
//! This crate defines that single abstraction. [`EngineEvent`] (engine →
//! renderer) and [`EngineCommand`] (renderer → engine) are the **only** types
//! that cross the seam. The seam genuinely bisects sudocode into
//! `[engine core = producer] | [renderers = consumers]`:
//!
//! * The in-process REPL consumes [`EngineEvent`] directly (it is NOT an ACP
//!   client).
//! * `engine-acp` serializes the same enum to Zed's Agent-Client-Protocol for
//!   the one place a process boundary exists (moss, sudowork over stdio/ws).
//!
//! There is exactly one way across the boundary, so no future "weird
//! integration point" can grow beside it.
//!
//! # Invariants
//!
//! * `engine-events` depends on `runtime` ONLY to re-export the value types
//!   that ride inside the seam (see re-exports below). `runtime` must never
//!   depend back on `engine-events` (cycle).
//! * Every callback / field of the pre-seam interfaces maps to exactly one
//!   variant here — see the doc comment on each variant for its origin.

// Payload value types re-exported from `runtime` so renderers get every
// field of an `EngineEvent` / `EngineCommand` without ever naming `runtime`
// (or the lower `api`) directly. These are plain data — no behaviour crosses
// the seam.
pub use runtime::{
    // Prompt-cache + auto-compaction telemetry (AssistantEvent::PromptCache,
    // TurnSummary::auto_compaction).
    AutoCompactionEvent,
    // The content-block vocabulary a renderer sends back in an
    // `EngineCommand::Prompt` (text, images, …).
    ContentBlock,
    PermissionMode,
    PermissionPromptDecision,
    // Permission channel payloads (was `runtime::PermissionPrompter`).
    PermissionRequest,
    PromptCacheEvent,
    // Question channel payloads (was `runtime::QuestionPrompter`); the field /
    // option / kind types are re-exported too so a renderer can fully
    // destructure a `QuestionRequest` without naming `runtime`.
    QuestionField,
    QuestionKind,
    QuestionOption,
    QuestionPromptAnswer,
    QuestionPromptRequest,
    // Incremental + cumulative token usage (AssistantEvent::Usage,
    // TurnSummary::{turn_usage,session_usage}).
    TokenUsage,
};

/// Monotonic identifier correlating a [`EngineEvent::PermissionRequest`] /
/// [`EngineEvent::QuestionRequest`] emitted by the engine with the matching
/// [`EngineCommand::PermissionAnswer`] / [`EngineCommand::QuestionAnswer`]
/// sent back by the renderer.
///
/// The engine allocates ids; renderers echo them verbatim. Ids are unique
/// within a single engine session (one in-flight request table).
pub type RequestId = u64;

/// The engine's lifecycle state, surfaced to renderers via
/// [`EngineEvent::State`].
///
/// [`EngineState::AwaitingInput`] is the trigger the kernel
/// `AgentState::AwaitingInput` primitive was built to receive: the engine
/// enters it the instant it emits a [`EngineEvent::PermissionRequest`] or
/// [`EngineEvent::QuestionRequest`], and returns to [`EngineState::Running`]
/// once the matching answer command arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// No turn in flight; waiting for the next [`EngineCommand::Prompt`].
    Idle,
    /// A turn is executing (model streaming, tools running).
    Running,
    /// A turn is blocked on the renderer answering a permission or question
    /// request.
    AwaitingInput,
}

/// End-of-turn aggregate — absorbs `runtime::TurnSummary` minus the message
/// vectors (which are streamed incrementally as [`EngineEvent::TextDelta`],
/// [`EngineEvent::ToolResult`], and [`EngineEvent::PromptCache`] and so need
/// not be re-sent here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnComplete {
    /// Number of model round-trips (assistant iterations) in this turn.
    pub iterations: usize,
    /// Token usage summed across every assistant message in this turn.
    pub turn_usage: TokenUsage,
    /// Cumulative token usage for the whole session so far.
    pub session_usage: TokenUsage,
    /// `true` when the turn ended because it was cancelled
    /// ([`EngineCommand::Cancel`] / abort signal) rather than completing.
    pub cancelled: bool,
    /// Wire model id from the API response (the last iteration's
    /// `message_start`). Use this — not the configured model — for context
    /// window / capability lookups, because the configured model may be an
    /// alias like "auto".
    pub response_model: Option<String>,
    /// Auto-compaction applied during the turn, if any.
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Engine → renderer. The sole outward type crossing the seam.
///
/// Every variant documents the pre-seam callback / field it subsumes so the
/// bisection is auditable: nothing reaches a renderer except through one of
/// these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// A new turn has begun. `label` is a short human description (e.g. the
    /// first line of the prompt) for progress display.
    TurnStarted { label: String },
    /// Lifecycle transition. See [`EngineState`].
    State(EngineState),
    /// Wire model id resolved from the API response's `message_start`
    /// (was `AssistantEvent::Model`, previously only reaching `TurnSummary`).
    ModelResolved { wire_model: String },
    /// Incremental thinking/reasoning text (was
    /// `RuntimeObserver::on_thinking_delta`).
    ThinkingDelta { text: String },
    /// Incremental assistant text (was `RuntimeObserver::on_text_delta`).
    TextDelta { text: String },
    /// The model requested a tool call (was `RuntimeObserver::on_tool_use`).
    /// `input` is the raw JSON arguments string.
    ToolCall {
        id: String,
        name: String,
        input: String,
    },
    /// A tool finished (was `RuntimeObserver::on_tool_result`). `output` is the
    /// tool's textual result; `is_error` distinguishes failures.
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    /// Incremental token usage for the in-flight assistant message (was
    /// `AssistantEvent::Usage`, previously only reaching `TurnSummary`).
    Usage(TokenUsage),
    /// Prompt-cache telemetry (was `AssistantEvent::PromptCache`).
    PromptCache(PromptCacheEvent),
    /// Automatic session compaction happened mid-turn.
    AutoCompaction(AutoCompactionEvent),
    /// The engine needs the renderer to approve/deny a tool invocation (was
    /// the synchronous `PermissionPrompter::decide` callback). Emitting this
    /// moves the engine into [`EngineState::AwaitingInput`]; the renderer
    /// replies with [`EngineCommand::PermissionAnswer`] carrying the same
    /// [`RequestId`].
    PermissionRequest {
        id: RequestId,
        request: PermissionRequest,
    },
    /// The engine needs the renderer to answer a structured question (was the
    /// synchronous `QuestionPrompter::ask` callback / the non-standard
    /// `_scode/ask_user_question` ACP extension). Also enters
    /// [`EngineState::AwaitingInput`]; answered with
    /// [`EngineCommand::QuestionAnswer`].
    QuestionRequest {
        id: RequestId,
        request: QuestionPromptRequest,
    },
    /// The active model changed (in response to [`EngineCommand::SetModel`]).
    /// `available` lists the models the renderer may switch to.
    ModelChanged {
        model: String,
        available: Vec<String>,
    },
    /// The active permission mode changed (in response to
    /// [`EngineCommand::SetPermissionMode`]).
    PermissionModeChanged { mode: PermissionMode },
    /// The turn finished (or was cancelled). Absorbs `runtime::TurnSummary`.
    TurnComplete(TurnComplete),
    /// A turn or command failed. `message` is renderer-facing text.
    Error { message: String },
    /// Free-form informational text: slash-command output, a compaction
    /// notice, or a friendly (non-fatal) message.
    Notice { text: String },
}

/// Renderer → engine. The sole inward type crossing the seam.
///
/// Input was historically the worst source of "weird access points" — moss's
/// `control_request → set_model`, the `_scode/ask_user_question` round-trip,
/// each renderer's ad-hoc prompt path. They all collapse into these variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCommand {
    /// Start a turn with the given content blocks (was
    /// `run_turn_with_blocks`).
    Prompt { blocks: Vec<ContentBlock> },
    /// Cancel the in-flight turn (was `abort_signal.abort()`).
    Cancel,
    /// Switch the active model; the engine replies with
    /// [`EngineEvent::ModelChanged`].
    SetModel { model: String },
    /// Switch the active permission mode; the engine replies with
    /// [`EngineEvent::PermissionModeChanged`].
    SetPermissionMode { mode: PermissionMode },
    /// Answer to a prior [`EngineEvent::PermissionRequest`] with a matching
    /// [`RequestId`].
    PermissionAnswer {
        id: RequestId,
        decision: PermissionPromptDecision,
    },
    /// Answer to a prior [`EngineEvent::QuestionRequest`] with a matching
    /// [`RequestId`].
    QuestionAnswer {
        id: RequestId,
        answers: Vec<QuestionPromptAnswer>,
    },
    /// Run a slash command (`/model`, `/compact`, …); the engine replies with
    /// [`EngineEvent::Notice`] (or a more specific event).
    SlashCommand { line: String },
    /// Tear down the session. No further events follow.
    Close,
}
