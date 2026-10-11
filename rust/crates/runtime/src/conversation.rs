use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::Stream;
use serde_json::{Map, Value};
use telemetry::SessionTracer;

mod response;
mod tool_context;
mod tool_execution;

use crate::compact::{
    autocompact_buffer_tokens, compact_session_sync, estimate_block_tokens,
    estimate_session_tokens, prune_tool_results, unchanged_compaction, CompactionAttemptOptions,
    CompactionConfig, CompactionError, CompactionOutcome, CompactionReport, CompactionResult,
    CompactionSummarySource, ContextBudget, PreparedCompaction, COMPACTION_POLICY_VERSION,
};
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{
    HookAbortSignal, HookProgressEvent, HookProgressReporter, HookRunResult, HookRunner,
};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::prompt::SystemPrompt;
use crate::session::{ContentBlock, ConversationMessage, MessageRole, Session, SessionError};
use crate::usage::{TokenUsage, UsageAggregation, UsageTracker};

const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";

/// Stop attempting auto-compaction after this many consecutive turns where the
/// compaction routine returned `removed_message_count == 0` (i.e. it ran but
/// couldn't shrink the conversation any further — typically because the
/// preserved-tail window already covers everything above the threshold, or
/// the boundary heuristics walked the keep-from index all the way back).
///
/// Without this circuit-breaker, a session that's structurally pinned above
/// `auto_compaction_input_tokens_threshold` retries compact_session() on
/// every turn for the rest of the session. The work is local & cheap, but
/// the user-visible auto-compaction event would also fire every turn with
/// `removed_message_count: 0`, which is noise (and would spam future
/// telemetry / ACP session events).
///
/// 3 matches CC's `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES` — CC observed
/// 1,279 sessions hammering 50+ futile retry attempts (one session: 3,272)
/// for a global 250K wasted API-call equivalent per day. sudocode's
/// compact_session is purely local so the wasted work is bounded, but the
/// noise-floor argument still applies.
const MAX_CONSECUTIVE_AUTO_COMPACT_NOOPS: u8 = 3;

/// Message used in synthetic tool results when a turn is interrupted.
const INTERRUPT_MESSAGE: &str = "Interrupted · What should Sudo Code do instead?";

/// Cancellation can win before the blocking tool returns its execution result.
/// Use the same constructor and model projection as an executor-side abort so
/// synthetic results cannot drift from the bash result contract.
fn interrupted_tool_output(tool_name: &str) -> String {
    if tool_name.eq_ignore_ascii_case("bash") {
        crate::bash::interrupted_bash_output(
            "Command interrupted by user",
            "interrupted",
            None,
            None,
        )
        .model_output()
        .to_string()
    } else {
        INTERRUPT_MESSAGE.to_string()
    }
}

const EMPTY_POST_TOOL_DELIVERABLE_REMINDER: &str = "\
<system-reminder>
The previous model response was empty after a tool completed. The user requested a file deliverable, but the current turn has not produced a matching final file yet. Continue the same task now: create or execute whatever is needed to produce the requested file, then verify it exists before ending the turn.
</system-reminder>";

/// Prefix of the user-side date announcement injected on the first turn.
/// The current date deliberately lives in a content block instead of the
/// system prompt so the system blocks stay byte-stable across days (the
/// dynamic system block would otherwise miss its prompt cache every new
/// day). Also used as the marker when scanning the session for an existing
/// announcement.
const DATE_CONTEXT_REMINDER_PREFIX: &str = "<system-reminder>Today's date is ";
/// Marker present in the date-rollover reminder text; a rollover reminder
/// also counts as a valid date announcement when scanning the session.
const DATE_ROLLOVER_REMINDER_MARKER: &str = "The local calendar date has changed";

/// Prefix of the user-side model announcement injected on the first turn.
/// The active model deliberately lives in a content block instead of the
/// system prompt so the system blocks stay byte-stable across a `/model`
/// switch (the dynamic system block would otherwise miss its prompt cache
/// every switch). Also used as the marker when scanning the session for an
/// existing announcement.
const MODEL_CONTEXT_REMINDER_PREFIX: &str = "<system-reminder>You are running as ";
/// Marker present in the model-change reminder text; a change reminder also
/// counts as a valid model announcement when scanning the session.
const MODEL_CHANGE_REMINDER_MARKER: &str = "The active model has changed";

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: SystemPrompt,
    pub messages: Vec<ConversationMessage>,
    /// Optional trace ID for end-to-end request tracking.
    /// Passed through to the HTTP layer as X-Request-ID header.
    pub trace_id: Option<String>,
    /// Tool names discovered via ToolSearch before compaction removed the
    /// ToolSearch results. Merged with message-scanned discoveries so
    /// these tools keep `defer_loading: false` after compaction.
    pub pre_compact_discovered_tools: std::collections::BTreeSet<String>,
}

/// Controls a non-streaming, text-only model completion. Tool schemas can be
/// retained as context without enabling a tool execution loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextCompletionOptions {
    pub max_tokens: u32,
    pub include_tools: bool,
    pub cache_prefix: bool,
    /// Whether this completion asks the model to think.
    ///
    /// It is not only an output-cost knob: the `thinking` parameter is part of
    /// Anthropic's cache key. A request that replays a prefix the turn stream
    /// cached while *omitting* `thinking` reads none of it. Measured on a live
    /// route (`ladder/tools/cache_prefix_probe.py`), same conversation shape,
    /// one factor changed:
    ///
    ///   thinking on both turns ..... turn 2 read 3025 / write  115
    ///   thinking dropped on turn 2 . turn 2 read    0 / write 3092
    ///
    /// Both returned 200 — the blocks in history are accepted either way, so
    /// nothing fails loudly. It is purely a silent full re-write.
    ///
    /// So a completion that exists to *preserve* a prefix has to match the
    /// stream it borrows from, and one that builds its own prefix is free to
    /// turn thinking off and save the output tokens.
    pub thinking_enabled: bool,
}

/// Provider-neutral completion data; consumers decide whether the finish
/// reason and content are acceptable for their task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCompletion {
    pub text: String,
    pub usage: Option<TokenUsage>,
    pub stop_reason: Option<String>,
    pub has_tool_calls: bool,
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantEvent {
    /// Wire model ID from the API response (emitted on message_start).
    Model(String),
    /// A distinct provider thinking block, including an empty signed block.
    /// Deltas append within this boundary; adjacent blocks must never merge.
    ThinkingStart,
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    /// Encrypted thinking, carried so it can be replayed. Nothing renders it.
    RedactedThinking {
        data: String,
    },
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
        thought_signature: Option<String>,
    },
    Usage(TokenUsage),
    PromptCache(PromptCacheEvent),
    MessageStop,
}

/// Prompt-cache telemetry captured from the provider response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// A boxed asynchronous stream of assistant events, produced by [`ApiClient::stream`].
///
/// Dropping the stream cancels the underlying HTTP request, ensuring unused
/// tokens are not consumed when a turn is aborted.
pub type AssistantEventStream =
    Pin<Box<dyn Stream<Item = Result<AssistantEvent, RuntimeError>> + Send>>;

/// Minimal streaming API contract required by [`ConversationRuntime`].
///
/// Implementations return an asynchronous stream of events instead of a
/// collected `Vec`, enabling the runtime to race each event against an
/// abort signal for instant cancellation.
#[async_trait]
pub trait ApiClient: Send {
    /// Whether this session forbids model HTTP outside its Nexus mount.
    fn requires_model_mount(&self) -> bool {
        false
    }

    fn model_catalog(&self) -> Option<crate::model_discovery::ModelCatalog> {
        None
    }
    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError>;

    /// Provider-facing model ID for the active route. Config aliases, display
    /// labels and model names reported by older responses are not routing IDs.
    fn wire_model_id(&self) -> Option<&str> {
        None
    }

    /// Reasoning effort this client sends. Subagents inherit it (an agent
    /// definition may override), matching CC's
    /// `agentDefinition.effort ?? state.effortValue`.
    fn reasoning_effort(&self) -> Option<&str> {
        None
    }

    /// Whether this client enables extended thinking.
    ///
    /// Only fork children inherit it. CC disables thinking for ordinary
    /// subagents "to control output token costs", and inherits it for forks
    /// specifically "to match the parent's API request prefix for prompt cache
    /// hits" — a diverging request shape costs the cache.
    fn thinking_enabled(&self) -> bool {
        false
    }

    /// The routing key this client sends as `metadata.user_id`. Subagents
    /// inherit it so a conversation and the agents it spawns stay pinned to
    /// one upstream account, which is what keeps the prompt cache warm.
    fn routing_session_id(&self) -> Option<&str> {
        None
    }

    /// Complete a text request using this client's configured model route.
    /// Request conversion and transport live in the shared API layer.
    async fn complete_text(
        &mut self,
        _request: ApiRequest,
        _options: TextCompletionOptions,
    ) -> Result<TextCompletion, RuntimeError> {
        Err(RuntimeError::compaction_path_not_supported(
            "text completion not supported by this API client",
        ))
    }

    /// Shared standard-compaction adapter. Clients implement `complete_text`,
    /// not separate compaction transports. Retained for existing API consumers.
    async fn send_compaction(
        &mut self,
        _model: &str,
        system_prompt: &str,
        messages: Vec<ConversationMessage>,
        max_tokens: u32,
    ) -> Result<String, RuntimeError> {
        let mut prompt = SystemPrompt::default();
        prompt.append_static_section(system_prompt);
        let response = self
            .complete_text(
                ApiRequest {
                    system_prompt: prompt,
                    messages,
                    trace_id: None,
                    pre_compact_discovered_tools: std::collections::BTreeSet::default(),
                },
                TextCompletionOptions {
                    max_tokens,
                    include_tools: false,
                    cache_prefix: false,
                    // This request builds its own prefix: a compaction-specific
                    // system prompt, no tools, and `build_compaction_messages`
                    // has already stripped the thinking blocks. There is no
                    // cached prefix to match, so thinking here would only add
                    // output tokens to a summarization.
                    thinking_enabled: false,
                },
            )
            .await?;
        crate::compact::validate_completion(response)
    }

    /// Shared cache-preserving adapter over the older message prefix.
    ///
    /// Every field of this request exists to be byte-identical to the turn
    /// stream it borrows the cached prefix from — same system prompt, same
    /// tools, same messages, one user turn appended. `thinking` is part of that
    /// key too, so it has to come from the client rather than be hardcoded off:
    /// omitting it re-wrote the entire prefix on a live route while still
    /// returning 200, which made the one compaction path designed to preserve
    /// the cache the most expensive one in the client.
    async fn send_cache_safe_compaction(
        &mut self,
        mut request: ApiRequest,
        compaction_prompt: &str,
        max_tokens: u32,
    ) -> Result<String, RuntimeError> {
        request
            .messages
            .push(ConversationMessage::user_text(compaction_prompt));
        let thinking_enabled = self.thinking_enabled();
        let response = self
            .complete_text(
                request,
                TextCompletionOptions {
                    max_tokens,
                    include_tools: true,
                    cache_prefix: true,
                    thinking_enabled,
                },
            )
            .await?;
        crate::compact::validate_completion(response)
    }

    /// Install (or clear) the sink the transport reports retries to.
    ///
    /// Called once per turn with the current observer's sink, because the
    /// renderer a turn belongs to changes while the client does not. Clients
    /// whose transport has no retry loop ignore it.
    fn set_retry_sink(&mut self, _sink: Option<crate::conversation::RetrySink>) {}

    /// The context-window budget for a request this client would send.
    ///
    /// The runtime's in-turn guard and the engine host's preflight both
    /// compact against whatever this returns, so it must describe the
    /// request this client will actually build. Three of the four numbers
    /// are derivable from the model alone and the default handles them; the
    /// fourth, `overhead_tokens`, is not — it includes the tool definitions
    /// the client attaches to every request, which the runtime cannot see.
    /// The default therefore accounts only for the system prompt and a
    /// client that attaches tools should override, as `EngineApiClient` and
    /// `ProviderRuntimeClient` do. Under-reporting the overhead makes the
    /// budget too generous and lets the guard miss an overflow it should
    /// have caught.
    fn context_budget(&self, model: &str, system_prompt: &SystemPrompt) -> ContextBudget {
        let overhead_tokens = if system_prompt.is_empty() {
            0
        } else {
            estimate_block_tokens(&ContentBlock::Text {
                text: system_prompt.render(),
            })
        };
        ContextBudget {
            context_limit: crate::model_capabilities::context_window_or_default(model) as usize,
            // The `max_tokens` a request is sent with, not the model's
            // nominal output ceiling: for gpt-5.4 (128K nominal, 64K
            // requested) and opus (64K nominal, 32K requested) those differ,
            // and the provider only ever adds the requested number.
            max_output_tokens: crate::model_capabilities::request_max_output_tokens(model) as usize,
            overhead_tokens,
            buffer_tokens: autocompact_buffer_tokens(model) as usize,
        }
    }

    /// Budget the schemas selected for this exact request, including tools
    /// revealed in the retained history or before an earlier compaction.
    fn context_budget_for_request(&self, model: &str, request: &ApiRequest) -> ContextBudget {
        self.context_budget(model, &request.system_prompt)
    }

    /// The request-specific budget for a summary completion. Its output cap
    /// and schema selection can differ from the next task request.
    fn context_budget_for_completion(
        &self,
        model: &str,
        request: &ApiRequest,
        options: TextCompletionOptions,
    ) -> ContextBudget {
        let mut budget = self.context_budget_for_request(model, request);
        budget.max_output_tokens = options.max_tokens as usize;
        if !options.include_tools {
            let system =
                (!request.system_prompt.is_empty()).then(|| request.system_prompt.render());
            // Match the API's serialized field estimator without depending on
            // that crate. The absent tools field serializes as `null` (2 tokens).
            budget.overhead_tokens =
                serde_json::to_vec(&system).map_or(0, |bytes| bytes.len() / 4 + 1) + 2;
        }
        budget
    }

    /// Resolve discovery before freezing one compaction run's request limits.
    async fn prepare_compaction_route(&mut self) -> Result<(), RuntimeError> {
        Ok(())
    }

    /// Request-shaping route state included in target-achievement fingerprints.
    fn compaction_route_fingerprint(&self, _request: &ApiRequest) -> String {
        format!(
            "{:?}|{}|{:?}",
            self.wire_model_id(),
            self.thinking_enabled(),
            self.reasoning_effort()
        )
    }
}

/// Optional observer for runtime events emitted while processing a turn.
///
/// All methods default to empty so existing implementations
/// (e.g. `SdkSessionObserver`) are unaffected when new hooks are added. The
/// streaming loop in `run_turn_with_blocks` forwards each [`AssistantEvent`]
/// to the matching hook in real time, so a renderer built on this trait sees
/// every event as it arrives (not only the end-of-turn [`TurnSummary`]
/// aggregate). This is the producer side of the engine↔renderer seam: the
/// `engine-core` adapter that turns these callbacks into
/// `engine_events::EngineEvent`s forwards all seven `AssistantEvent` variants,
/// so every hook below must be forwarded from the loop.
pub trait RuntimeObserver {
    /// Non-fatal runtime maintenance notices, forwarded through the engine seam.
    fn on_notice(&mut self, _text: &str) {}

    /// Context maintenance lifecycle, including failures before a model request.
    fn on_compaction(&mut self, _event: &CompactionProgress) {}

    fn on_thinking_delta(&mut self, _delta: &str) {}

    fn on_text_delta(&mut self, _delta: &str) {}

    fn on_tool_use(&mut self, _id: &str, _name: &str, _input: &str) {}

    /// Execution actually acquired a slot, after hooks and authorization.
    /// Requested calls can wait behind a writer or the concurrency limit.
    fn on_tool_started(&mut self, _id: &str, _name: &str, _input: &str) {}

    /// A policy or hook denied a specific invocation, without requiring UI.
    fn on_permission_denied(&mut self, _id: &str, _name: &str, _input: &str, _reason: &str) {}

    fn on_tool_result(
        &mut self,
        _tool_use_id: &str,
        _tool_name: &str,
        _output: &str,
        _is_error: bool,
    ) {
    }

    /// Wire model id resolved from `message_start` (`AssistantEvent::Model`).
    fn on_model(&mut self, _wire_model: &str) {}

    /// Incremental token usage for the in-flight assistant message
    /// (`AssistantEvent::Usage`).
    fn on_usage(&mut self, _usage: &TokenUsage) {}

    /// Prompt-cache telemetry (`AssistantEvent::PromptCache`).
    fn on_prompt_cache(&mut self, _event: &PromptCacheEvent) {}

    /// End of one assistant message in the stream (`AssistantEvent::MessageStop`).
    fn on_message_stop(&mut self) {}

    /// Optional `Send + Sync` sink for live tool-execution progress (streaming
    /// `bash` output, MCP progress notifications).
    ///
    /// Default `None`: existing observers (incl. the ACP `SdkSessionObserver`)
    /// see no live progress, exactly as before. An observer that wants it (the
    /// seam's `engine-core` adapter) returns a sink; the runtime hands it to the
    /// tool executor via [`ToolDispatchContext::progress_sink`], which installs
    /// it around a single tool call. Progress can't ride the other `&mut self`
    /// hooks: it fires from deep inside tool execution, off the loop thread, so
    /// it needs a `Send + Sync` value, not a borrow of the observer.
    fn tool_progress_sink(&self) -> Option<ProgressSink> {
        None
    }

    /// Optional `Send + Sync` sink for live plugin-hook progress (the
    /// PreToolUse / PostToolUse / PostToolUseFailure lifecycle lines that used
    /// to be eprintln'd by the CLI's build-time hook reporter).
    ///
    /// Mirrors [`tool_progress_sink`](Self::tool_progress_sink): default `None`
    /// leaves existing observers unchanged, while the seam's `engine-core`
    /// adapter returns a sink so hook progress rides the seam as
    /// `EngineEvent::HookProgress`. Installed into the runtime's
    /// `hook_progress_reporter` at the start of each turn (see
    /// `run_turn_with_blocks`), so the pre/post-tool hook runners forward every
    /// lifecycle event through it. Like tool progress it can't ride the `&mut
    /// self` hooks above: the reporter is invoked from the (synchronous) hook
    /// runner, so it needs a `Send + Sync` value, not a borrow of the observer.
    fn hook_progress_sink(&self) -> Option<HookProgressSink> {
        None
    }

    /// Sink for the HTTP transport's retry loop, installed into the API client
    /// at the start of each turn (see `run_turn_with_blocks`). Same reason as
    /// `hook_progress_sink` for not being a `&mut self` hook: the transport
    /// reports from its own async task, so it needs an owned `Send + Sync`
    /// value. A renderer that returns `None` leaves the client's own default
    /// behaviour in place.
    fn retry_sink(&self) -> Option<RetrySink> {
        None
    }

    /// Sink for what spawned sub-agents are doing (their text, thinking, tool
    /// calls and lifecycle). Default `None`: sub-agents run without an
    /// observer, exactly as before. A renderer that returns a sink gets the
    /// events via [`ToolDispatchContext::subagent_sink`] → the Agent tool; see
    /// [`crate::subagent_events`].
    fn subagent_sink(&self) -> Option<crate::subagent_events::SubagentSink> {
        None
    }

    /// Session-owned background task lifecycle and cancellation.
    fn background_tasks(&self) -> Option<crate::background_tasks::BackgroundTasks> {
        None
    }
}

/// A live progress report from a running tool. Structured, not rendered — the
/// renderer above the seam formats it; the runtime and tool executor only
/// report data (no ANSI, no terminal writes).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolProgressEvent {
    /// Streaming `bash` output progress.
    Bash {
        /// The most recent output line (already extracted; may be empty).
        last_line: String,
        /// Cumulative line count so far.
        total_lines: usize,
        /// Cumulative byte count so far.
        total_bytes: usize,
    },
    /// MCP tool progress notification.
    Mcp {
        /// Human-readable status, if the server sent one.
        message: Option<String>,
        /// Progress value.
        progress: f64,
        /// Total, if known (enables a percentage).
        total: Option<f64>,
    },
}

/// A `Send + Sync` sink a renderer installs (via
/// [`RuntimeObserver::tool_progress_sink`]) to receive [`ToolProgressEvent`]s
/// during tool execution.
///
/// Threaded through [`ToolDispatchContext`] so the tool executor installs it
/// **narrowly, at dispatch time**. A thread-local progress callback set for the
/// whole async turn would be invisible once the turn future hops worker threads;
/// installing it right before the (synchronous-ish) tool call keeps it on the
/// executing thread.
#[derive(Clone)]
pub struct ProgressSink(std::sync::Arc<dyn Fn(ToolProgressEvent) + Send + Sync>);

impl ProgressSink {
    /// Wrap a progress handler.
    pub fn new(f: impl Fn(ToolProgressEvent) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    /// Report one progress event to the renderer.
    pub fn emit(&self, event: ToolProgressEvent) {
        (self.0)(event);
    }
}

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink(..)")
    }
}

/// A `Send + Sync` sink a renderer installs (via
/// [`RuntimeObserver::hook_progress_sink`]) to receive [`HookProgressEvent`]s
/// as plugin hooks run. The seam analogue of [`ProgressSink`] for the
/// pre/post-tool hook lifecycle: the runtime wraps it in a
/// [`HookProgressReporter`] and installs it for the turn, so each hook outcome
/// is reported as structured data (the renderer above the seam formats it).
#[derive(Clone)]
pub struct HookProgressSink(std::sync::Arc<dyn Fn(HookProgressEvent) + Send + Sync>);

impl HookProgressSink {
    /// Wrap a hook-progress handler.
    pub fn new(f: impl Fn(HookProgressEvent) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    /// Report one hook-progress event to the renderer.
    pub fn emit(&self, event: HookProgressEvent) {
        (self.0)(event);
    }
}

impl std::fmt::Debug for HookProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HookProgressSink(..)")
    }
}

/// What the HTTP transport is doing while it retries a failed request.
///
/// A provider 429 or 5xx is retried with backoff, and from the outside that is
/// indistinguishable from the model being slow — several seconds of nothing,
/// repeatedly. The transport therefore reports what it is doing, and this is
/// the shape that report takes on its way to a renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryEvent {
    /// A request failed and the transport is waiting before trying again.
    /// `attempt` is 1-based.
    Waiting {
        attempt: u32,
        max_retries: u32,
        reason: String,
    },
    /// The wait is over and the request is going back out.
    Resumed,
}

/// A `Send + Sync` sink a renderer installs (via
/// [`RuntimeObserver::retry_sink`]) to receive [`RetryEvent`]s from the HTTP
/// transport's retry loop.
///
/// The seam analogue of [`HookProgressSink`], and it exists for the same
/// reason: the emitter is neither the observer nor on the observer's thread —
/// here it is the transport, below the runtime — so it needs an owned
/// `Send + Sync` value rather than a borrow. The runtime hands it to the API
/// client for the turn; the client adapts it to whatever notifier its
/// transport wants.
#[derive(Clone)]
pub struct RetrySink(std::sync::Arc<dyn Fn(RetryEvent) + Send + Sync>);

impl RetrySink {
    /// Wrap a retry handler.
    pub fn new(f: impl Fn(RetryEvent) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    /// Report one retry event to the renderer.
    pub fn emit(&self, event: RetryEvent) {
        (self.0)(event);
    }
}

impl std::fmt::Debug for RetrySink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetrySink(..)")
    }
}

/// Adapter that turns a [`HookProgressSink`] (from the observer) into a
/// [`HookProgressReporter`] the runtime can install for the turn. Every
/// `on_event` callback is forwarded to the sink, so plugin-hook progress
/// reaches the renderer through the seam instead of a build-time stderr sink.
struct SinkHookReporter(HookProgressSink);

impl crate::hooks::HookProgressReporter for SinkHookReporter {
    fn on_event(&mut self, event: &HookProgressEvent) {
        self.0.emit(event.clone());
    }
}

/// XML tag identifying a fork subagent's inherited directive message.
/// Populated by `tools::build_fork_child_message` (writer) and consumed
/// by [`ToolDispatchContext::is_inside_fork_child`] (reader). Living
/// here — rather than in the tools crate — keeps runtime as the SSOT
/// for the tag string; tools contains the rules text that wraps it.
pub const FORK_BOILERPLATE_TAG: &str = "fork-boilerplate";

/// Per-tool-call context threaded from the runtime tool loop into
/// [`ToolExecutor::execute_with_context`]. Carries the parent's
/// in-flight assistant message plus the session history up to the
/// currently-executing tool_use so the fork subagent path can:
/// 1. Clone the parent's assistant message into the child's initial
///    session prefix (matching CC-fork's `buildForkedMessages` shape
///    for prompt-cache-identical prefixes).
/// 2. Detect fork-inside-fork recursion via the boilerplate tag left
///    in the child's own inherited user message.
///
/// Ordinary subagent spawns also inherit the current model from this
/// context. [`ToolExecutor`]'s default `execute_with_context` forwards to
/// `execute` without reading `ctx`.
#[derive(Debug, Clone, Default)]
pub struct ToolDispatchContext {
    /// The parent's assistant message that emitted the currently-
    /// executing tool_use. `None` when the caller isn't inside a
    /// parent's tool loop (test harnesses, direct executor invocations).
    /// Its model is stamped from the current runtime, not a prior response;
    /// subagents inherit this model unless their invocation overrides it.
    pub parent_assistant_message: Option<ConversationMessage>,
    /// The parent session's full message history at dispatch time,
    /// including the assistant message that just emitted this tool_use.
    /// Populated by the runtime tool loop from `Session::messages`.
    /// Consumed by [`Self::is_inside_fork_child`].
    pub parent_session_messages: Vec<ConversationMessage>,
    /// The current session's `tool-results/` directory — the sole handle
    /// the `read_tool_output` tool needs to page an offloaded oversized
    /// result back in. The runtime resolves it from `Session`; the model
    /// only ever names the opaque tool_use id, so the physical path never
    /// crosses the model boundary. `None` for in-memory sessions.
    pub tool_results_dir: Option<std::path::PathBuf>,
    /// Optional live-progress sink for streaming tools (`bash`/MCP). `None`
    /// when no renderer wants live progress (the observer returned no sink, or
    /// there is no observer). The tool executor installs it narrowly around a
    /// single tool call — see [`ProgressSink`].
    pub progress_sink: Option<ProgressSink>,
    /// The parent's reasoning effort, so a spawned subagent runs at the same
    /// effort unless its agent definition overrides it.
    pub parent_reasoning_effort: Option<String>,
    /// Whether the parent has extended thinking on. Consumed only by the fork
    /// path; ordinary subagents keep thinking off regardless.
    pub parent_thinking_enabled: bool,
    /// The parent's routing key, inherited verbatim by spawned subagents.
    pub parent_routing_session_id: Option<String>,
    /// Host policy inherited by every child, including fallback and summary calls.
    pub parent_requires_model_mount: bool,
    /// Where a spawned sub-agent reports what it is doing, if the renderer
    /// asked for that (see [`RuntimeObserver::subagent_sink`]).
    pub subagent_sink: Option<crate::subagent_events::SubagentSink>,
    /// Shared by this session and its children, independent of a single turn.
    pub background_tasks: Option<crate::background_tasks::BackgroundTasks>,
    /// The `tool_use` id of this invocation, including concurrent calls.
    pub tool_use_id: Option<String>,
    /// The parent session's active permission mode. A spawned sub-agent runs
    /// under this mode rather than an unconditional full-access policy, so a
    /// read-only session spawns read-only workers and a workspace-write
    /// session cannot escalate through a child. `None` when the caller is not
    /// inside a parent tool loop (test harnesses, direct executor calls); the
    /// spawn path then falls back to a conservative default.
    pub parent_permission_mode: Option<crate::permissions::PermissionMode>,
    /// The parent's approval route. Workers keep their inherited policy and
    /// send only approval requests through this shared input queue.
    pub permission_sink: Option<crate::permissions::PermissionPromptSink>,
}

impl ToolDispatchContext {
    /// Returns `true` when the parent session's history contains a user
    /// message tagged with [`FORK_BOILERPLATE_TAG`] — meaning this
    /// dispatch is happening inside a fork child. Mirrors CC-fork's
    /// `isInForkChild(messages)` in `forkSubagent.ts`.
    #[must_use]
    pub fn is_inside_fork_child(&self) -> bool {
        let needle = format!("<{FORK_BOILERPLATE_TAG}>");
        self.parent_session_messages.iter().any(|m| {
            matches!(m.role, MessageRole::User)
                && m.blocks.iter().any(|b| match b {
                    ContentBlock::Text { text } => text.contains(&needle),
                    _ => false,
                })
        })
    }
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
///
/// Execution is `async` + `&self`: a tool is a pure `(input, ctx) -> result`
/// operation with no need for `&mut` on the dispatcher, and the conversation
/// loop overlaps a concurrency-safe batch by polling several
/// `execute_with_context` futures together on one thread (I/O interleaving,
/// not thread parallelism — the blocking syscall inside each future is the
/// only thing offloaded to a `spawn_blocking` pool, so the dispatcher never
/// crosses a thread boundary and needs no `Sync`). Any per-invocation mutable
/// state (spinner, prompter) lives behind single-threaded interior mutability
/// in the impl.
pub trait ToolExecutor: Send {
    /// Scheduling capability of the effective (post-hook) invocation.
    /// Dynamic executors may add metadata-backed tools without changing the
    /// conversation scheduler. Permission checks remain independent.
    fn is_concurrency_safe(&self, tool_name: &str, input: &str) -> bool {
        crate::tool_concurrency::builtin_is_concurrency_safe(tool_name, input)
    }

    async fn execute_with_attachments(
        &self,
        tool_name: &str,
        input: &str,
        ctx: &ToolDispatchContext,
    ) -> Result<crate::image_input::ToolOutput, ToolError> {
        let output = self.execute_with_context(tool_name, input, ctx).await?;
        let model = ctx
            .parent_assistant_message
            .as_ref()
            .and_then(|m| m.model.as_deref())
            .unwrap_or("");
        crate::image_input::ToolOutput::from_dispatch(tool_name, output, model)
    }

    async fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError>;

    /// Dispatch with per-call context. Default forwards to
    /// [`ToolExecutor::execute`]. Override to consult `ctx`
    /// (e.g. the Agent tool's fork branch reads
    /// `ctx.parent_assistant_message` to build the child's prefix).
    async fn execute_with_context(
        &self,
        tool_name: &str,
        input: &str,
        _ctx: &ToolDispatchContext,
    ) -> Result<String, ToolError> {
        self.execute(tool_name, input).await
    }

    /// Setup-only hook to hand the dispatcher the turn's abort signal. Stays
    /// `&mut self`: it is called once while the runtime still owns the
    /// dispatcher exclusively (before any turn), so it never races the
    /// `&self` concurrent `execute` path.
    fn set_abort_signal(&mut self, _abort_signal: HookAbortSignal) {}
}

/// Error returned when a tool invocation fails locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// Error returned when a conversation turn cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
    kind: RuntimeErrorKind,
    /// `true` when the underlying failure is transport-transient (rate limit,
    /// 5xx, gateway timeout, dropped/truncated stream) and the same request may
    /// succeed on a retry. Derived once, at the api→runtime boundary, from
    /// [`api::ApiError::is_retryable`] (the `ErrorAction::Transport` bucket) —
    /// so callers branch on this typed bit instead of re-deriving retryability
    /// by string-matching the rendered message.
    retryable: bool,
}

/// Coarse classification of a [`RuntimeError`], for the few failures the
/// runtime can recover from itself rather than surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeErrorKind {
    Generic,
    /// This completion shape is unsupported; another compaction shape may work.
    CompactionPathNotSupported,
    /// A provider completed its response, but its summary cannot be installed.
    InvalidCompactionSummary,
    /// The request was rejected — locally by the API client's preflight or
    /// by the provider — because it does not fit the model's context window.
    ContextWindowBlocked,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RuntimeErrorKind::Generic,
            retryable: false,
        }
    }

    #[must_use]
    pub fn invalid_compaction_summary(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RuntimeErrorKind::InvalidCompactionSummary,
            retryable: false,
        }
    }

    #[must_use]
    pub fn is_invalid_compaction_summary(&self) -> bool {
        self.kind == RuntimeErrorKind::InvalidCompactionSummary
    }

    #[must_use]
    pub fn compaction_path_not_supported(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RuntimeErrorKind::CompactionPathNotSupported,
            retryable: false,
        }
    }

    #[must_use]
    pub fn is_compaction_path_not_supported(&self) -> bool {
        self.kind == RuntimeErrorKind::CompactionPathNotSupported
    }

    /// An error whose cause is the request exceeding the context window.
    /// `run_turn` reacts to this by compacting history and retrying once.
    #[must_use]
    pub fn context_window_blocked(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RuntimeErrorKind::ContextWindowBlocked,
            retryable: false,
        }
    }

    /// Set the transport-retryable classification. Chainable so the api→runtime
    /// boundary can stamp it in one place: `RuntimeError::new(msg).retryable(a)`.
    #[must_use]
    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    #[must_use]
    pub fn is_context_window_blocked(&self) -> bool {
        self.kind == RuntimeErrorKind::ContextWindowBlocked
    }

    /// `true` when this is a transport-transient failure worth retrying. The
    /// typed successor to string-matching the message for "timeout"/"503"/etc.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.retryable
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Summary of one completed (or cancelled) runtime turn, including tool
/// results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    /// Total token usage for all assistant messages in this turn.
    /// This is the sum of usage from each model request triggered by the user message.
    pub turn_usage: TokenUsage,
    /// Cumulative token usage for the entire session from start to now.
    pub session_usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
    /// `true` when the turn was interrupted by the abort signal.  Partial
    /// progress (user message, streamed assistant text, synthetic tool
    /// results, interruption marker) has already been committed to the
    /// session so the model has full context on the next turn.
    pub cancelled: bool,
    /// Wire model ID from the API response (the last iteration's
    /// `message_start` event). Use this — not `config.model` — for
    /// context window / capability lookups, because `config.model` may
    /// be an alias like "auto" that doesn't map to any capabilities entry.
    pub response_model: Option<String>,
}

/// Which path produced a [`CompactionResult`] from
/// [`ConversationRuntime::compact_with_method`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionMethod {
    /// The model summarised the removed messages (`send_compaction`).
    LlmSummary,
    /// The LLM call was unavailable or failed; the local structural
    /// summary was used instead.
    LocalHeuristic,
}

impl CompactionMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LlmSummary => "llm summary",
            Self::LocalHeuristic => "local heuristic",
        }
    }
}

/// How many times a single turn may compact its own history.
///
/// This was once per turn, which is what let a many-step turn die mid-flight:
/// the salvage fired on the first rejection and the second overflow ended the
/// turn. A long tool-using turn legitimately needs more than one pass.
///
/// The allowance is bounded rather than open because every compaction is an
/// LLM round-trip the user pays for and waits on. Eight is chosen from what
/// a compaction actually buys: each pass replaces everything before
/// `CompactionConfig::preserve_recent_messages` with one summary, so a turn
/// that needs a ninth pass has already summarised summaries eight times and
/// is not making progress — what is left is the preserved tail, and no
/// further pass can shrink it. Spending the allowance is therefore the
/// signal that the overflow is structural, and it surfaces as a real error
/// instead of an unbounded compaction loop. This in-turn allowance covers
/// eight runs, each capped at two completed responses and four model HTTP
/// attempts. Pre-send and post-turn maintenance use their own run scopes.
const MAX_TURN_COMPACTIONS: usize = 8;

/// The config every guard in this file compacts with.
///
/// `max_estimated_tokens: 0` disables [`should_compact`]'s size heuristic —
/// its default 10K gate — leaving only the message-count floor
/// (`preserve_recent_messages`). That is deliberate: the caller has already
/// decided from its own context budget that this session must shrink, so
/// re-asking a coarser question here could only override that decision with
/// a worse-informed one. What it does *not* bypass is the tail protection:
/// with nothing removable, compaction still reports back a no-op.
fn forced_compaction_config() -> CompactionConfig {
    CompactionConfig {
        max_estimated_tokens: 0,
        ..CompactionConfig::default()
    }
}

/// Which guard asked for a compaction. Recorded on every attempt so a
/// context-overflow report can be diagnosed from the session log: until this
/// existed only the engine host's preflight emitted an event, and the two
/// paths that actually run during a turn were silent — making a compaction
/// that ran and worked indistinguishable from one that never happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    /// The per-turn preflight, before the turn starts.
    Preflight,
    /// The in-turn budget check, before dispatching an iteration's request.
    InTurnBudget,
    /// Salvage after the provider rejected a request as too large.
    ProviderRejection,
    /// The post-turn usage-threshold check.
    PostTurnUsage,
}

impl CompactionTrigger {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::InTurnBudget => "in_turn_budget",
            Self::ProviderRejection => "provider_rejection",
            Self::PostTurnUsage => "post_turn_usage",
        }
    }
}

/// One context-maintenance operation. UI events, never model messages or
/// tools offered to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    Started,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CompactionProgress {
    pub id: String,
    pub trigger: &'static str,
    pub status: CompactionStatus,
    pub before_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<CompactionReport>,
}

impl CompactionProgress {
    #[must_use]
    pub fn started(trigger: &'static str, before_tokens: usize) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self {
            id: format!(
                "compaction-{}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_millis(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ),
            trigger,
            status: CompactionStatus::Started,
            before_tokens,
            after_tokens: None,
            report: None,
        }
    }
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
    pub report: Option<CompactionReport>,
}

#[derive(Clone, Copy)]
struct CompactionRunOptions {
    allow_pruning: bool,
    allow_repeat_skip: bool,
}

#[derive(Clone)]
enum CompactionSourceRevision {
    InMemory,
    Durable(String),
}

impl From<Option<String>> for CompactionSourceRevision {
    fn from(revision: Option<String>) -> Self {
        revision.map_or(Self::InMemory, Self::Durable)
    }
}

impl CompactionSourceRevision {
    fn as_deref(&self) -> Option<&str> {
        match self {
            Self::InMemory => None,
            Self::Durable(revision) => Some(revision),
        }
    }
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    // Borrow a local Arc during dispatch so completion hooks can mutate the
    // session while sibling futures share &T. Mutex requires only T: Send,
    // not Sync; the executor stays owned even if a caller drops the turn future.
    tool_executor: Arc<Mutex<T>>,
    permission_policy: PermissionPolicy,
    system_prompt: SystemPrompt,
    /// Date (`YYYY-MM-DD`) the session currently treats as "today". The
    /// system prompt itself carries no date (a per-day field there would
    /// break the prompt-cache prefix every new day); instead, when `Some`,
    /// [`ConversationRuntime::run_turn_with_blocks`] announces the date via
    /// a `<system-reminder>` content block on the first user turn and
    /// prepends a rollover reminder when the local date changes mid-session.
    prompt_known_date: Option<String>,
    /// The model this session last announced to the assistant. Mirrors
    /// `prompt_known_date`: `run_turn_with_blocks` announces the active model
    /// via a `<system-reminder>` content block on the first user turn and
    /// prepends a change reminder when the active model differs mid-session
    /// (e.g. after `/model`). Keeps model identity out of the cached system
    /// prompt entirely.
    prompt_known_model: Option<String>,
    /// Override for "today" used in tests. Always `None` outside tests.
    #[cfg(test)]
    today_override: Option<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    /// Consecutive turns where `maybe_auto_compact` ran and returned a no-op
    /// (compact_session removed zero messages). Reset to 0 on any successful
    /// compaction. When this reaches `MAX_CONSECUTIVE_AUTO_COMPACT_NOOPS`,
    /// `maybe_auto_compact` short-circuits for the rest of the session.
    consecutive_auto_compact_noops: u8,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<HookProgressSink>,
    session_tracer: Option<SessionTracer>,
    /// File operation tracker for the current turn.
    file_tracker: crate::file_tracker::TurnFileTracker,
    /// Current turn ID for file tracking.
    current_turn_id: Option<String>,
    /// User request intent for the current turn.
    user_request_intent: Option<crate::file_intent::UserRequestIntent>,
    /// Trace ID for the current request (passed from ACP _meta.traceId).
    trace_id: Option<String>,
    compaction_pending_input_tokens: usize,
    last_compaction_report: Option<CompactionReport>,
    /// Absent until a source read succeeds; in-memory sources need no hash.
    compaction_source_revision: Option<CompactionSourceRevision>,
    /// An unreadable or changed durable source forbids writing this snapshot.
    session_source_stale: bool,
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: SystemPrompt,
    ) -> Self {
        Self::new_with_features(
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            &RuntimeFeatureConfig::default(),
        )
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_features(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: SystemPrompt,
        feature_config: &RuntimeFeatureConfig,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        let workspace_root = session
            .workspace_root()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        Self {
            session,
            api_client,
            tool_executor: Arc::new(Mutex::new(tool_executor)),
            permission_policy,
            system_prompt,
            prompt_known_date: None,
            prompt_known_model: None,
            #[cfg(test)]
            today_override: None,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            consecutive_auto_compact_noops: 0,
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
            file_tracker: crate::file_tracker::TurnFileTracker::new(workspace_root),
            current_turn_id: None,
            user_request_intent: None,
            trace_id: None,
            compaction_pending_input_tokens: 0,
            last_compaction_report: None,
            compaction_source_revision: None,
            session_source_stale: false,
        }
    }

    /// Records the date (`YYYY-MM-DD`) the session should treat as "today".
    /// The runtime announces it to the model via a `<system-reminder>`
    /// content block on the first user turn, and each later turn compares it
    /// against the local date, emitting a rollover reminder when the date
    /// has changed — the system prompt itself never carries the date, so its
    /// prompt-cache prefix stays warm across days.
    #[must_use]
    pub fn with_session_known_date(mut self, date: impl Into<String>) -> Self {
        self.prompt_known_date = Some(date.into());
        self
    }

    /// Date currently treated as "when the cached system prompt was frozen".
    /// Exposed so the CLI can propagate this state across runtime rebuilds —
    /// the runtime advances it after firing a date-rollover reminder, and a
    /// rebuild that didn't carry it forward would reset the state and re-fire
    /// the reminder (or suppress it entirely when the rebuild stamps today's
    /// date over the original known date).
    #[must_use]
    pub fn prompt_known_date(&self) -> Option<&str> {
        self.prompt_known_date.as_deref()
    }

    /// Announce the model this session starts on. Mirrors
    /// [`Self::with_session_known_date`]: the model is carried in a first-turn
    /// `<system-reminder>` content block, never in the cached system prompt.
    #[must_use]
    pub fn with_session_known_model(mut self, model: impl Into<String>) -> Self {
        self.prompt_known_model = Some(model.into());
        self
    }

    /// Configured model label for this runtime, which may be a provider alias.
    /// The last announced wire model is read separately from the transcript.
    #[must_use]
    pub fn prompt_known_model(&self) -> Option<&str> {
        self.prompt_known_model.as_deref()
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.tool_executor_mut()
            .set_abort_signal(hook_abort_signal.clone());
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    /// Set the trace ID for the next request.
    /// This is called before run_turn to pass the traceId from ACP _meta.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Set the trace ID in-place (non-builder pattern).
    pub fn set_trace_id(&mut self, trace_id: impl Into<String>) {
        self.trace_id = Some(trace_id.into());
    }

    /// Returns the date the runtime should treat as "today" for the
    /// purpose of inter-turn date-change detection.
    fn current_local_date(&self) -> String {
        #[cfg(test)]
        {
            if let Some(ref overridden) = self.today_override {
                return overridden.clone();
            }
        }
        crate::time::today_local()
    }

    /// Keep the model informed of the current date WITHOUT ever putting the
    /// date into the (cache-prefix) system prompt:
    ///
    /// - When the session does not yet carry a date announcement (first turn,
    ///   or after compaction dropped the message that carried it), a
    ///   `<system-reminder>` content block stating today's date is PREPENDED to
    ///   the user's content.
    /// - When the local date no longer matches the session's known date, a
    ///   rollover reminder is prepended and the known date advanced so the
    ///   reminder fires only once per rollover.
    ///
    /// Both shapes live in user-side content blocks, leaving the system
    /// prompt byte-stable across days so its prompt-cache prefix stays warm.
    ///
    /// **Prepended, never appended** — the invariant every injector here holds:
    /// harness-injected content does not TRAIL user-authored content in the
    /// same message. Adjacent text blocks reach the model with nothing between
    /// them, so a reminder placed after the user's text silently extends what
    /// "this text" refers to; an instruction like *"send exactly this text: X"*
    /// then has the model copy the reminder into the tool argument, and harness
    /// context leaves the process as user-authored content. That is
    /// sudocode#623, observed on a cross-org A2A hop and reproducing only on
    /// the turn that carries an announcement — which is this one.
    ///
    /// The first-turn shape used to append, to keep the turn label reading as
    /// what the user typed. That reason did not hold: `turn_label`
    /// (`engine-core::session`, its only caller) is computed from the blocks
    /// handed to the turn, upstream of every injector here.
    fn inject_date_context(&mut self, blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
        let Some(known) = self.prompt_known_date.clone() else {
            return blocks;
        };
        let today = self.current_local_date();
        if today != known {
            let reminder = ContentBlock::Text {
                text: format!(
                    "<system-reminder>The local calendar date has changed since this session started. \
                     The previously announced date was {known}; today is now {today}. \
                     Treat {today} as the current date for any reasoning that depends on it.</system-reminder>"
                ),
            };
            self.prompt_known_date = Some(today);
            let mut combined = Vec::with_capacity(blocks.len() + 1);
            combined.push(reminder);
            combined.extend(blocks);
            return combined;
        }
        if self.session_has_date_context() {
            return blocks;
        }
        let mut combined = Vec::with_capacity(blocks.len() + 1);
        combined.push(ContentBlock::Text {
            text: format!("{DATE_CONTEXT_REMINDER_PREFIX}{today}.</system-reminder>"),
        });
        combined.extend(blocks);
        combined
    }

    /// `true` when some message already announced the current date to the
    /// model — either the first-turn announcement or a rollover reminder.
    /// Scanning the session (rather than tracking a flag) self-heals after
    /// compaction removes the message that carried the announcement.
    fn session_has_date_context(&self) -> bool {
        self.session.messages.iter().any(|message| {
            message.blocks.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::Text { text }
                        if text.contains(DATE_CONTEXT_REMINDER_PREFIX)
                            || text.contains(DATE_ROLLOVER_REMINDER_MARKER)
                )
            })
        })
    }

    /// Keep the assistant informed of which model it is WITHOUT ever putting
    /// the model into the (cache-prefix) system prompt, mirroring
    /// [`Self::inject_date_context`]. Both shapes live in user-side content
    /// blocks, leaving the system prompt byte-stable across a `/model` switch
    /// so its prompt-cache prefix stays warm.
    fn inject_model_context(&mut self, blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
        if self.prompt_known_model.is_none() {
            return blocks;
        }
        let active = self
            .api_client
            .wire_model_id()
            .map_or_else(|| self.active_model(), str::to_string);
        if active.is_empty() {
            return blocks;
        }
        // Rebuilding the runtime stamps the newly selected model into its
        // config. Only the transcript records what the assistant was last
        // told, including after resume or switching back to an earlier model.
        let known = self.session_model_context().map(str::to_string);
        if known.as_deref() == Some(active.as_str()) {
            return blocks;
        }
        let text = match known {
            Some(known) => format!(
                "<system-reminder>The active model has changed since this session started. \
                     You were {known}; you are now running as {active}. \
                     Any earlier text in this conversation that named a different model is stale. \
                     When asked which model you are, answer {active}.</system-reminder>"
            ),
            None => format!(
                "{MODEL_CONTEXT_REMINDER_PREFIX}{active}. \
                 This is the model ID selected by the host for this request. \
                 Earlier model labels in the conversation may be stale; \
                 when asked which model is selected, report this ID.</system-reminder>"
            ),
        };
        let mut combined = Vec::with_capacity(blocks.len() + 1);
        combined.push(ContentBlock::Text { text });
        combined.extend(blocks);
        combined
    }

    /// Fallback model label for clients that do not expose a wire model ID.
    /// Prefer the session's requested model, then the runtime configuration.
    fn active_model(&self) -> String {
        self.session
            .model
            .clone()
            .or_else(|| self.prompt_known_model.clone())
            .unwrap_or_default()
    }

    /// The model the runtime is actually running on — the config SSOT model it
    /// was built with (`prompt_known_model`, set from `RuntimeConfig.model` and
    /// re-set on every `/model` rebuild). Used to size the auto-compaction
    /// context window and to tag assistant messages. Unlike [`Self::active_model`]
    /// this prefers `prompt_known_model` over the persisted `session.model`,
    /// because on resume `session.model` still records the transcript's old
    /// model while the runtime already runs on the current config default.
    /// Empty only when a unit test builds a runtime without announcing a model.
    fn running_model(&self) -> &str {
        self.prompt_known_model
            .as_deref()
            .or(self.session.model.as_deref())
            .unwrap_or_default()
    }

    /// Last model announced in retained user-side context. Search backwards so
    /// an older announcement cannot hide a later switch or a switch back.
    fn session_model_context(&self) -> Option<&str> {
        self.session
            .messages
            .iter()
            .rev()
            .filter(|message| message.role == MessageRole::User)
            .flat_map(|message| message.blocks.iter().rev())
            .find_map(|block| {
                let ContentBlock::Text { text } = block else {
                    return None;
                };
                let announced = text
                    .strip_prefix(MODEL_CONTEXT_REMINDER_PREFIX)
                    .or_else(|| {
                        text.strip_prefix("<system-reminder>")?
                            .strip_prefix(MODEL_CHANGE_REMINDER_MARKER)?
                            .split_once("; you are now running as ")
                            .map(|(_, model)| model)
                    })?;
                announced.split_once(". ").map(|(model, _)| model)
            })
    }

    /// Drain any coordinator-mode `<task-notification>` XML blocks
    /// that background sub-agents deposited since the previous
    /// turn.  When any arrive, prepend a single Text block carrying
    /// the formatted batch to the FRONT of this turn's user
    /// content.
    ///
    /// Non-coord sessions short-circuit inside
    /// [`crate::coordinator_notification::drain`] (env-var
    /// fast-path) — zero disk I/O and zero allocation for the
    /// overwhelmingly common case.
    ///
    /// Workspace root is the turn's scoped root (the process cwd outside
    /// ACP), matching the per-workspace `.sudocode-inbox/coordinator.jsonl`
    /// convention that every emit site uses (same root).
    fn prepend_pending_task_notifications(&self, blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
        let workspace_root = crate::workspace_root::current_workspace_root_or_default();
        let notifications =
            crate::coordinator_notification::drain(&workspace_root).unwrap_or_default();
        // The interactive event bridge can already carry the same completion.
        // Consume the legacy coordinator queue without injecting it twice.
        let notifications: Vec<_> = notifications.into_iter().filter(|notification| {
            !blocks.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains(notification.as_str())))
        }).collect();
        if notifications.is_empty() {
            return blocks;
        }
        let prefix = crate::coordinator_notification::format_drain_batch(&notifications);
        let mut combined = Vec::with_capacity(blocks.len() + 1);
        combined.push(ContentBlock::Text { text: prefix });
        combined.extend(blocks);
        combined
    }

    /// Run a session health probe to verify the runtime is functional after compaction.
    /// Returns Ok(()) if healthy, Err if the session appears broken.
    async fn run_session_health_probe(&mut self) -> Result<(), String> {
        // Check if we have basic session integrity
        if self.session.messages.is_empty() && self.session.compaction.is_some() {
            return Err("compacted session has no active history; restore a pre-compaction transcript before continuing".into());
        }

        // Verify tool executor is responsive with a non-destructive probe
        // Using glob_search with a pattern that won't match anything
        let probe_input = r#"{"pattern": "*.health-check-probe-"}"#;
        // A new turn can compact before its first tool batch. Do not reuse the
        // previous batch's cancellation generation for this independent probe.
        let abort = self.hook_abort_signal.for_current_turn();
        self.tool_executor_mut().set_abort_signal(abort);
        match self
            .tool_executor_mut()
            .execute("glob_search", probe_input)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Tool executor probe failed: {e}")),
        }
    }

    /// Preserve partial progress when a turn is cancelled.
    ///
    /// 1. Build an assistant message from whatever events were collected
    ///    before the abort signal fired.  An `[interrupted]` text block is
    ///    appended so the model can see which turn was cancelled.
    /// 2. Push the partial assistant message to the session.
    /// 3. For every `tool_use` block in that partial message, generate a
    ///    synthetic `tool_result` with `is_error: true` so the API contract
    ///    (every `tool_use` must have a matching `tool_result`) is maintained.
    /// 4. Cleanup draft files created during this turn.
    fn finalize_cancelled_turn(&mut self, events: Vec<AssistantEvent>) {
        // Build partial assistant message from whatever events arrived.
        let mut text = String::new();
        let mut blocks = Vec::new();
        for event in events {
            match event {
                AssistantEvent::ThinkingStart => start_thinking_block(&mut text, &mut blocks),
                AssistantEvent::Thinking {
                    thinking,
                    signature,
                } => {
                    flush_text_block(&mut text, &mut blocks);
                    push_thinking_block(&mut blocks, thinking, signature);
                }
                AssistantEvent::RedactedThinking { data } => {
                    flush_text_block(&mut text, &mut blocks);
                    blocks.push(ContentBlock::RedactedThinking { data });
                }
                AssistantEvent::TextDelta(delta) => text.push_str(&delta),
                AssistantEvent::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                } => {
                    flush_text_block(&mut text, &mut blocks);
                    blocks.push(ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                    });
                }
                AssistantEvent::Model(_)
                | AssistantEvent::Usage(_)
                | AssistantEvent::PromptCache(_)
                | AssistantEvent::MessageStop => {}
            }
        }
        finish_assistant_blocks(&mut text, &mut blocks);

        // Append an [interrupted] marker to the assistant message so the
        // model knows this turn was cancelled.  This stays attached to the
        // assistant turn (not a separate user message) so attribution is
        // correct.
        blocks.push(ContentBlock::Text {
            text: format!("\n\n[{INTERRUPT_MESSAGE}]"),
        });

        // Extract tool_use ids before pushing the assistant message.
        let pending_tool_ids: Vec<(String, String)> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();

        // Push the partial assistant message.
        let _ = self
            .session
            .push_message(ConversationMessage::assistant(blocks));

        // Generate synthetic error tool_results for any tool_use blocks
        // that never got real results.
        for (tool_use_id, tool_name) in pending_tool_ids {
            let _ = self.session.push_message(ConversationMessage::tool_result(
                tool_use_id,
                tool_name.clone(),
                interrupted_tool_output(&tool_name),
                true,
            ));
        }

        // Cleanup draft files created during this turn.
        let cleaned = self.cleanup_current_turn_drafts();
        if !cleaned.is_empty() {
            // Log cleaned files for debugging (could be sent to observer in future)
            // Note: This is silent cleanup, no token consumption
        }
        self.finish_current_turn_tracking();
    }

    fn finish_current_turn_tracking(&mut self) {
        self.file_tracker.end_turn();
        self.current_turn_id = None;
        self.user_request_intent = None;
    }

    /// If a tool output exceeds the offload threshold, spill the full bytes to
    /// `<session-dir>/tool-results/<tool_use_id>` (via the session `FsBackend`,
    /// backend-agnostic) and replace it in-transcript with a head preview + a
    /// deterministic `<persisted-output>` marker. Persisting the *replaced*
    /// message is itself the content-replacement: resume replays it verbatim,
    /// so the prompt-cache prefix stays byte-identical. The full bytes live only
    /// in the offloaded file (the read-more side-channel); the transcript is the
    /// prompt SSOT.
    fn maybe_offload_tool_output(
        &self,
        tool_use_id: &str,
        tool_name: &str,
        output: String,
    ) -> String {
        let Some(threshold) = offload_threshold_for(tool_name) else {
            return output;
        };
        if output.len() <= threshold {
            return output;
        }
        match self
            .session
            .offload_tool_result(tool_use_id, output.as_bytes())
        {
            Ok((_path, total)) => {
                // The physical path never leaves the engine; the model only
                // ever sees the opaque tool_use id. The full result is paged
                // back in as byte windows via `read_tool_output`, continuing
                // from where this preview stops (offset={end}) — or located
                // first with its `pattern` seek.
                let end = offload_preview_end(&output);
                format!(
                    "<persisted-output bytes={total} id=\"{tool_use_id}\">\nOutput too large ({total} bytes). Preview (first {end} bytes):\n{}\n…\n[Read the rest with read_tool_output(id=\"{tool_use_id}\", offset={end}) — continue from each response's nextOffset until it is null — or pass pattern=<regex> to find the offsets of specific lines first.]\n</persisted-output>",
                    &output[..end]
                )
            }
            // Offload failed (e.g. no persistence path) — never send an
            // unbounded prompt; fall back to a bounded truncation (shared with bash).
            Err(_) => crate::bash::truncate_output(&output, threshold),
        }
    }

    fn cancelled_summary(
        &mut self,
        assistant_messages: Vec<ConversationMessage>,
        tool_results: Vec<ConversationMessage>,
        prompt_cache_events: Vec<PromptCacheEvent>,
        iterations: usize,
    ) -> TurnSummary {
        let cleaned = self.cleanup_current_turn_drafts();
        if !cleaned.is_empty() {
            // Log cleaned files for debugging (could be sent to observer in future)
            // Note: This is silent cleanup, no token consumption
        }
        self.finish_current_turn_tracking();
        let turn_usage = sum_assistant_message_usage(&assistant_messages);
        let session_usage = self.usage_tracker.cumulative_usage();
        TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            turn_usage,
            session_usage,
            auto_compaction: None,
            cancelled: true,
            response_model: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub async fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        prompter: Option<&mut dyn PermissionPrompter>,
        observer: Option<&mut dyn RuntimeObserver>,
    ) -> Result<TurnSummary, RuntimeError> {
        let text = user_input.into();
        self.run_turn_with_blocks(vec![ContentBlock::Text { text }], prompter, observer)
            .await
    }

    /// Run a conversation turn with pre-built content blocks (e.g. text +
    /// image).  [`run_turn`](Self::run_turn) is a convenience wrapper that
    /// creates a single `Text` block and delegates here.
    ///
    /// The stream returned by the API client is consumed event-by-event using
    /// [`tokio::select!`], racing each event against the hook abort signal.
    /// When cancellation fires the stream is dropped immediately, which closes
    /// the underlying HTTP connection and stops token consumption.
    #[allow(clippy::too_many_lines)]
    /// Run a turn, then tear down what was installed *for* that turn.
    ///
    /// Two such things: the hook-progress reporter and the API client's retry
    /// sink. Both are rebuilt from the observer at the start of every turn, so
    /// neither may survive it — and the reporter used to, because nothing
    /// cleared it. Anything holding a clone of the renderer's event channel
    /// pins that channel open after the turn has already returned. The REPL
    /// never notices; the ACP server waits for exactly that channel to close
    /// before answering `session/prompt`, and so waits forever.
    ///
    /// The reporter is only cleared when this turn installed it, since one can
    /// also be supplied at build time. The retry sink has no build-time form,
    /// so it is always cleared.
    pub async fn run_turn_with_blocks(
        &mut self,
        blocks: Vec<ContentBlock>,
        prompter: Option<&mut dyn PermissionPrompter>,
        observer: Option<&mut dyn RuntimeObserver>,
    ) -> Result<TurnSummary, RuntimeError> {
        if self.session_source_stale {
            self.refresh_compaction_source().map_err(|error| {
                RuntimeError::new(format!(
                    "session source could not be refreshed; durable history preserved: {error}"
                ))
            })?;
        }
        let installs_hook_reporter = observer
            .as_deref()
            .and_then(RuntimeObserver::hook_progress_sink)
            .is_some();
        let catalog = self.api_client.model_catalog();
        let turn = self.run_turn_with_blocks_inner(blocks, prompter, observer);
        let summary = if let Some(catalog) = catalog {
            catalog.refresh_in_background();
            catalog.scope(turn).await
        } else {
            turn.await
        };
        if summary.is_err() {
            self.finish_current_turn_tracking();
        }
        if installs_hook_reporter {
            self.hook_progress_reporter = None;
        }
        self.api_client.set_retry_sink(None);
        // A completed turn is a durable fact, so the ENGINE records it rather
        // than each host remembering to after calling in. The CLI did it from
        // its call site, and the co-host — a later caller of the same engine —
        // simply did not have that line: it ran real turns and kept the whole
        // transcript in memory, so nothing survived the agent.
        //
        // Here it is true by construction for any host, present or future. Only
        // on success, which is the behaviour the CLI's call site had: a failed
        // turn leaves the session as the close-time save finds it, rather than
        // committing a half-turn as though it completed.
        if summary.is_ok() {
            self.ensure_session_persistence_allowed().map_err(|error| {
                RuntimeError::new(format!(
                    "refusing to persist a stale session; durable history preserved: {error}"
                ))
            })?;
            if let Some(path) = self.session.persistence_path() {
                self.session.save_to_path(path).map_err(|error| {
                    RuntimeError::new(format!("failed to persist session: {error}"))
                })?;
            }
        }
        summary
    }

    async fn run_turn_with_blocks_inner(
        &mut self,
        blocks: Vec<ContentBlock>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
        mut observer: Option<&mut dyn RuntimeObserver>,
    ) -> Result<TurnSummary, RuntimeError> {
        let blocks = self.inject_date_context(blocks);
        let blocks = self.inject_model_context(blocks);
        // Coordinator-mode push: drain any `<task-notification>` XML
        // blocks that background sub-agents deposited into the
        // coordinator's inbox since the previous turn, and prepend
        // them as an extra Text block AT THE FRONT of this turn's
        // user message.  Wiring lives here (runtime layer) instead
        // of at each caller because every entry point routes through
        // `run_turn_with_blocks` — CLI REPL, one-shot `--print`,
        // ACP stdio/WebSocket/SDK, MCP servers — and duplicating
        // the drain across all of them was the exact anti-pattern
        // Ethan flagged when the CLI-only wiring shipped in
        // PR #282.  Non-coord sessions get 0 disk cost via the
        // `is_coordinator_mode()` fast-path inside `drain()`.
        let blocks = self.prepend_pending_task_notifications(blocks);
        let label = blocks
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();

        if self.session.compaction.is_some() {
            if let Err(error) = self.run_session_health_probe().await {
                return Err(RuntimeError::new(format!(
                    "Session health probe failed after compaction: {error}. \
                     The session may be in an inconsistent state. \
                     Consider starting a fresh session with /session new."
                )));
            }
        }

        let mut prepared_blocks = Vec::with_capacity(blocks.len());
        for block in blocks {
            let block = match block {
                ContentBlock::Image { data, mime_type } => {
                    let model = self.running_model().to_string();
                    crate::image_input::prepare_image(&data, &mime_type, &model)
                        .map_err(RuntimeError::new)?
                }
                other => other,
            };
            prepared_blocks.push(block);
        }

        // Start file tracking for this turn
        let turn_id = format!(
            "turn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        self.current_turn_id = Some(turn_id.clone());
        self.file_tracker.start_turn(turn_id.clone());

        // Analyze user request for file intent
        self.user_request_intent = Some(crate::file_intent::UserRequestIntent::analyze(&label));

        self.record_turn_started(&label);
        // Start a fresh per-turn usage accumulator so the status line bills the
        // whole turn (all tool-loop requests), not just the last request.
        self.usage_tracker.begin_turn();
        // Wall-clock start of the turn: recorded at turn end into the tracker
        // (the status line's `+Ns`/`Σs`) and stamped onto the turn's last
        // assistant message so it persists and re-sums on resume.
        let turn_started_at = std::time::Instant::now();
        self.session
            .push_user_blocks(prepared_blocks)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        self.compaction_pending_input_tokens = 0;

        // Hook workers share the current observer's progress sink, so slow
        // hooks never hold the event loop while reporting lifecycle changes.
        if let Some(sink) = observer
            .as_deref()
            .and_then(RuntimeObserver::hook_progress_sink)
        {
            self.hook_progress_reporter = Some(sink);
        }

        // Retry reporting follows the observer, so it is set unconditionally —
        // passing `None` when this observer wants none is what stops a previous
        // turn's renderer from still receiving events.
        self.api_client
            .set_retry_sink(observer.as_deref().and_then(RuntimeObserver::retry_sink));

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut retried_empty_post_tool_deliverable = false;
        let mut turn_compactions = 0usize;
        let mut recorded_compaction_budget_exhausted = false;
        let mut overflow_compaction: Option<AutoCompactionEvent> = None;
        let mut response_model: Option<String>;

        loop {
            if self.hook_abort_signal.is_aborted() {
                self.finalize_cancelled_turn(Vec::new());
                let turn_usage = sum_assistant_message_usage(&assistant_messages);
                let session_usage = self.usage_tracker.cumulative_usage();
                return Ok(TurnSummary {
                    assistant_messages,
                    tool_results,
                    prompt_cache_events,
                    iterations,
                    turn_usage,
                    session_usage,
                    auto_compaction: None,
                    cancelled: true,
                    response_model: None,
                });
            }

            iterations += 1;
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }

            // Compact before dispatching, not after the provider rejects.
            // A turn that takes many tool-call steps grows its own history
            // while it runs; the per-turn preflight ran once, before any of
            // those steps existed. Without this the first the runtime hears
            // of the overflow is a rejection, and the single salvage below
            // is all a long turn ever gets.
            while self.next_request_exceeds_budget() {
                if turn_compactions < MAX_TURN_COMPACTIONS {
                    let attempt = self
                        .compact_in_place(
                            forced_compaction_config(),
                            CompactionTrigger::InTurnBudget,
                            runtime_observer_mut(&mut observer),
                        )
                        .await;
                    if self.hook_abort_signal.is_aborted()
                        && !(attempt.is_err()
                            && self
                                .last_compaction_report
                                .as_ref()
                                .is_some_and(|report| report.outcome == CompactionOutcome::Failed))
                    {
                        return Ok(self.cancelled_summary(
                            assistant_messages,
                            tool_results,
                            prompt_cache_events,
                            iterations,
                        ));
                    }
                    if let Some(event) =
                        attempt.map_err(|error| RuntimeError::new(error.to_string()))?
                    {
                        overflow_compaction =
                            merge_auto_compaction(overflow_compaction, Some(event));
                    } else {
                        return Err(RuntimeError::context_window_blocked(
                            "Context cannot be compacted safely; history preserved. No model request sent."));
                    }
                    turn_compactions += 1;
                } else if !recorded_compaction_budget_exhausted {
                    self.record_compaction_budget_exhausted(CompactionTrigger::InTurnBudget);
                    recorded_compaction_budget_exhausted = true;
                }
                if turn_compactions >= MAX_TURN_COMPACTIONS && self.next_request_exceeds_budget() {
                    return Err(RuntimeError::context_window_blocked(
                        "Compaction retry budget exhausted; history preserved. No model request sent."));
                }
            }

            let (budget, _) = self.safe_history_budget(&self.session);
            if !budget.fits_buffered(estimate_session_tokens(&self.session)) {
                return Err(RuntimeError::context_window_blocked(
                    "Context remains too large after compaction; history preserved. No model request sent."));
            }

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
                trace_id: self.trace_id.clone(),
                pre_compact_discovered_tools: self
                    .session
                    .compaction
                    .as_ref()
                    .map(|c| c.pre_compact_discovered_tools.clone())
                    .unwrap_or_default(),
            };
            // Race the API stream (which includes the retry loop) against
            // the abort signal so ESC/Ctrl-C cancels even during retries.
            let stream_result = {
                let abort = &self.hook_abort_signal;
                tokio::select! {
                    biased;
                    () = abort.cancelled() => {
                        self.finalize_cancelled_turn(Vec::new());
                        let turn_usage = sum_assistant_message_usage(&assistant_messages);
                        let session_usage = self.usage_tracker.cumulative_usage();
                        return Ok(TurnSummary {
                            assistant_messages,
                            tool_results,
                            prompt_cache_events,
                            iterations,
                            turn_usage,
                            session_usage,
                            auto_compaction: None,
                            cancelled: true,
                            response_model: None,
                        });
                    }
                    result = self.api_client.stream(request) => result,
                }
            };
            let stream = match stream_result {
                Ok(stream) => stream,
                Err(error) => {
                    // A context-window rejection is the one request failure
                    // the runtime can fix on its own: compact the history and
                    // resend. Shares the turn's compaction allowance with the
                    // proactive check above; when compaction can no longer
                    // remove anything the error is real and must surface.
                    if error.is_context_window_blocked() && turn_compactions < MAX_TURN_COMPACTIONS
                    {
                        let attempt = self
                            .compact_in_place(
                                forced_compaction_config(),
                                CompactionTrigger::ProviderRejection,
                                runtime_observer_mut(&mut observer),
                            )
                            .await;
                        if self.hook_abort_signal.is_aborted()
                            && !(attempt.is_err()
                                && self.last_compaction_report.as_ref().is_some_and(|report| {
                                    report.outcome == CompactionOutcome::Failed
                                }))
                        {
                            return Ok(self.cancelled_summary(
                                assistant_messages,
                                tool_results,
                                prompt_cache_events,
                                iterations,
                            ));
                        }
                        if let Some(event) =
                            attempt.map_err(|error| RuntimeError::new(error.to_string()))?
                        {
                            turn_compactions += 1;
                            overflow_compaction =
                                merge_auto_compaction(overflow_compaction, Some(event));
                            continue;
                        }
                    }
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            };

            let response = self
                .execute_response(stream, iterations, &mut observer, &mut prompter)
                .await;
            let events = response.events;
            let (mut assistant_message, usage, turn_prompt_cache_events, iter_response_model) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error)
                        if assistant_messages.last().is_some_and(has_pending_tool_uses)
                            && error.message == "assistant stream produced no content" =>
                    {
                        self.record_empty_post_tool_completion(iterations);
                        let has_unfinished_deliverable = self
                            .has_unfinished_requested_deliverable_after_tool_empty(
                                &assistant_messages,
                                &tool_results,
                            );
                        if has_unfinished_deliverable && !retried_empty_post_tool_deliverable {
                            retried_empty_post_tool_deliverable = true;
                            self.session
                                .push_user_text(EMPTY_POST_TOOL_DELIVERABLE_REMINDER)
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                            continue;
                        }
                        if has_unfinished_deliverable {
                            let error = RuntimeError::new(
                                "model returned an empty response after tool use before producing the requested file deliverable",
                            );
                            self.record_turn_failed(iterations, &error);
                            return Err(error);
                        }
                        let attempt = self
                            .maybe_auto_compact(runtime_observer_mut(&mut observer))
                            .await;
                        if self.hook_abort_signal.is_aborted()
                            && !(attempt.is_err()
                                && self.last_compaction_report.as_ref().is_some_and(|report| {
                                    report.outcome == CompactionOutcome::Failed
                                }))
                        {
                            return Ok(self.cancelled_summary(
                                assistant_messages,
                                tool_results,
                                prompt_cache_events,
                                iterations,
                            ));
                        }
                        let auto_compaction = match attempt {
                            Ok(event) => merge_auto_compaction(overflow_compaction, event),
                            Err(error) => {
                                self.record_turn_failed(iterations, &error);
                                return Err(error);
                            }
                        };
                        self.file_tracker.end_turn();
                        self.current_turn_id = None;
                        self.user_request_intent = None;
                        let turn_usage = sum_assistant_message_usage(&assistant_messages);
                        let session_usage = self.usage_tracker.cumulative_usage();
                        let summary = TurnSummary {
                            assistant_messages,
                            tool_results,
                            prompt_cache_events,
                            iterations,
                            turn_usage,
                            session_usage,
                            auto_compaction,
                            cancelled: false,
                            response_model: None,
                        };
                        self.record_turn_completed(&summary);
                        return Ok(summary);
                    }
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            response_model = iter_response_model;
            assistant_message.model = {
                let running = self.running_model();
                (!running.is_empty()).then(|| running.to_string())
            };
            if let Some(usage) = usage {
                self.usage_tracker.record(usage);
            }
            prompt_cache_events.extend(turn_prompt_cache_events);
            let pending_tool_uses = assistant_message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => Some((id.clone(), name.clone(), input.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.record_assistant_iteration(
                iterations,
                &assistant_message,
                pending_tool_uses.len(),
            );

            let exchange_start = response
                .assistant_index
                .unwrap_or(self.session.messages.len());
            match response.assistant_index {
                Some(index) => self
                    .session
                    .update_assistant_message(index, assistant_message.clone()),
                None => self.session.push_message(assistant_message.clone()),
            }
            .map_err(|error| RuntimeError::new(error.to_string()))?;
            assistant_messages.push(assistant_message.clone());

            for (index, message) in response.results.into_iter().enumerate() {
                if index >= response.persisted_results {
                    self.session
                        .push_message(message.clone())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                }
                self.record_tool_finished(iterations, &message);
                tool_results.push(message);
            }
            if let Some(error) = response.error {
                if let Some(path) = self.session.persistence_path() {
                    self.session.save_to_path(path).map_err(|failure| RuntimeError::new(format!("failed to persist interrupted tools: {failure}; provider error: {error}")))?;
                }
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }
            if response.cancelled || self.hook_abort_signal.is_aborted() {
                return Ok(self.cancelled_summary(
                    assistant_messages,
                    tool_results,
                    prompt_cache_events,
                    iterations,
                ));
            }
            if let Some(action) = response.context_action {
                self.apply_tool_context_action(action, exchange_start, &mut observer)?;
            }
            if pending_tool_uses.is_empty() {
                break;
            }
        }

        let attempt = self
            .maybe_auto_compact(runtime_observer_mut(&mut observer))
            .await;
        if self.hook_abort_signal.is_aborted()
            && !(attempt.is_err()
                && self
                    .last_compaction_report
                    .as_ref()
                    .is_some_and(|report| report.outcome == CompactionOutcome::Failed))
        {
            return Ok(self.cancelled_summary(
                assistant_messages,
                tool_results,
                prompt_cache_events,
                iterations,
            ));
        }
        let auto_compaction = match attempt {
            Ok(event) => merge_auto_compaction(overflow_compaction, event),
            Err(error) => {
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }
        };

        self.finish_current_turn_tracking();

        // Record the turn's wall-clock time: into the tracker (status line) and
        // stamped onto the last assistant message in the persisted session, so
        // it survives to resume and re-sums into the cumulative total.
        let turn_duration_ms = turn_started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.usage_tracker.record_turn_duration(turn_duration_ms);
        if let Some(message) = self
            .session
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        {
            message.duration_ms = Some(turn_duration_ms);
        }

        let turn_usage = sum_assistant_message_usage(&assistant_messages);
        let session_usage = self.usage_tracker.cumulative_usage();
        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            turn_usage,
            session_usage,
            auto_compaction,
            cancelled: false,
            response_model,
        };
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    /// Build a validated replacement without mutating the live history.
    pub async fn compact(
        &mut self,
        config: CompactionConfig,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, CompactionError> {
        self.compact_with_method(config, custom_instructions)
            .await
            .map(|(result, _)| result)
    }

    /// Manual compaction bypasses automatic pressure thresholds. Failure is
    /// explicit and leaves the original session available for retry.
    pub async fn compact_with_method(
        &mut self,
        config: CompactionConfig,
        custom_instructions: Option<&str>,
    ) -> Result<(CompactionResult, CompactionMethod), CompactionError> {
        let run_id =
            CompactionProgress::started("manual", estimate_session_tokens(&self.session)).id;
        self.compact_with_method_for_run(config, custom_instructions, run_id)
            .await
    }

    /// Reuse the engine's Started event ID for the report and usage receipts.
    pub async fn compact_with_method_for_run(
        &mut self,
        mut config: CompactionConfig,
        custom_instructions: Option<&str>,
        run_id: String,
    ) -> Result<(CompactionResult, CompactionMethod), CompactionError> {
        config.max_estimated_tokens = 0;
        let source = self.session.clone();
        let result = self
            .run_compaction(
                &source,
                config,
                custom_instructions,
                CompactionRunOptions {
                    allow_pruning: false,
                    allow_repeat_skip: true,
                },
                run_id,
            )
            .await?;
        Ok((result, CompactionMethod::LlmSummary))
    }

    /// Install only after the owning engine has durably committed the candidate.
    pub fn install_compacted_session(&mut self, session: Session) {
        self.session = session;
        self.usage_tracker.clear_context_usage();
    }

    pub fn set_compaction_report(&mut self, report: CompactionReport) {
        self.session.last_compaction_report = Some(report.clone());
        self.last_compaction_report = Some(report);
    }

    pub fn mark_compaction_committed(&mut self, result: &CompactionResult) {
        self.last_compaction_report.clone_from(&result.report);
        self.usage_tracker.clear_context_usage();
    }

    /// Refuse ordinary persistence while the durable source could not be
    /// validated or reloaded. A successful refresh must precede another write.
    pub fn ensure_session_persistence_allowed(&self) -> Result<(), SessionError> {
        if self.session_source_stale {
            Err(SessionError::SourceChanged)
        } else {
            Ok(())
        }
    }

    /// Reload a changed durable source without writing the stale transcript.
    /// Keep this run's known billing in memory until it can be safely saved.
    pub fn refresh_compaction_source(&mut self) -> Result<(), SessionError> {
        // Failed reloads must also disable any later stale metadata write.
        self.compaction_source_revision = None;
        self.session_source_stale = true;
        let Some(path) = self
            .session
            .persistence_path()
            .map(std::path::Path::to_path_buf)
        else {
            self.session_source_stale = false;
            return Ok(());
        };
        let fs = self.session.fs_handle();
        let mut refreshed = Session::load_from_path_with(fs.as_ref(), &path)?.with_fs_backend(fs);
        refreshed.merge_maintenance_usage(&self.session.maintenance_usage);
        self.usage_tracker.refresh_session_totals(&refreshed);
        self.session = refreshed;
        self.session_source_stale = false;
        Ok(())
    }

    pub fn set_compaction_pending_input_tokens(&mut self, tokens: usize) {
        self.compaction_pending_input_tokens = tokens;
    }

    #[must_use]
    pub fn last_compaction_report(&self) -> Option<&CompactionReport> {
        self.last_compaction_report.as_ref()
    }

    fn history_request(&self, source: &Session) -> ApiRequest {
        ApiRequest {
            system_prompt: self.system_prompt.clone(),
            messages: source.messages.clone(),
            trace_id: self.trace_id.clone(),
            pre_compact_discovered_tools: source
                .compaction
                .as_ref()
                .map(|c| c.pre_compact_discovered_tools.clone())
                .unwrap_or_default(),
        }
    }

    #[must_use]
    pub fn context_budget_for_next_request(&self) -> ContextBudget {
        let catalog = self.api_client.model_catalog();
        let _scope = catalog
            .as_ref()
            .map(crate::model_discovery::ModelCatalog::enter);
        self.api_client.context_budget_for_request(
            &self.compaction_model(),
            &self.history_request(&self.session),
        )
    }

    fn safe_history_budget(&self, source: &Session) -> (ContextBudget, usize) {
        let catalog = self.api_client.model_catalog();
        let _scope = catalog
            .as_ref()
            .map(crate::model_discovery::ModelCatalog::enter);
        let budget = self
            .api_client
            .context_budget_for_request(&self.compaction_model(), &self.history_request(source));
        let safe = budget
            .history_budget()
            .saturating_sub(self.compaction_pending_input_tokens);
        (budget, safe)
    }

    fn compaction_fingerprint(&self, source: &Session, custom: Option<&str>) -> String {
        use sha2::{Digest, Sha256};
        let request = self.history_request(source);
        let (budget, safe) = self.safe_history_budget(source);
        let content: Vec<_> = source
            .messages
            .iter()
            .map(|m| (&m.role, &m.blocks))
            .collect();
        // Debug content excludes all usage/model/duration metadata, which cannot change the request.
        let material = format!(
            "{:?}|{:?}|{:?}|{}|{}|{:?}|{}|{}|{}",
            content,
            crate::compact::render_todo_continuity_block(source.fs_handle()),
            custom,
            self.system_prompt.render(),
            self.api_client.compaction_route_fingerprint(&request),
            budget,
            safe,
            self.compaction_pending_input_tokens,
            COMPACTION_POLICY_VERSION
        );
        format!("{:x}", Sha256::digest(material.as_bytes()))
    }

    fn compaction_retention(&self, source: &Session, config: CompactionConfig) -> CompactionConfig {
        let (budget, safe) = self.safe_history_budget(source);
        let tokens = (budget.context_limit * 16 / 100)
            .min(safe / 2)
            .min(estimate_session_tokens(source) / 5);
        config.with_token_retention(source, tokens)
    }

    /// One frozen-source run. All provider sends and fallback paths share this scope.
    async fn run_compaction(
        &mut self,
        source: &Session,
        config: CompactionConfig,
        custom: Option<&str>,
        options: CompactionRunOptions,
        run_id: String,
    ) -> Result<CompactionResult, CompactionError> {
        self.last_compaction_report = None;
        self.compaction_source_revision = None;
        let mut source = source.clone();
        let abort = self.hook_abort_signal.clone();
        let mut source_changed = false;
        let preparation = match source.capture_durable_revision_for_history() {
            Ok((revision, _)) => {
                let new_usage = self
                    .session
                    .merge_maintenance_usage(&source.maintenance_usage);
                self.record_compaction_maintenance_usage(new_usage);
                self.compaction_source_revision = Some(revision.into());
                self.session_source_stale = false;
                tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(RuntimeError::new("cancelled")),
                    result = self.api_client.prepare_compaction_route() => result,
                }
            }
            Err(error) => {
                self.session_source_stale = true;
                source_changed = matches!(error, SessionError::SourceChanged);
                let refresh = if source_changed {
                    self.refresh_compaction_source().err()
                } else {
                    None
                };
                Err(RuntimeError::new(refresh.map_or_else(
                    || format!("source revision could not be read: {error}"),
                    |refresh| format!("{error}; latest source could not be reloaded: {refresh}"),
                )))
            }
        };
        if let Err(error) = preparation {
            let before = estimate_session_tokens(&source);
            let (budget, safe) = self.safe_history_budget(&source);
            self.set_compaction_report(CompactionReport {
                run_id,
                before_history: before,
                after_history: if source_changed {
                    estimate_session_tokens(&self.session)
                } else {
                    before
                },
                target_history: Some((before / 2).min(safe)),
                ideal_history: Some((before * 30 / 100).min(safe).min(before / 2)),
                source_safe_history_budget: safe,
                safe_history_budget: safe,
                actual_fixed_overhead: budget.overhead_tokens,
                method: None,
                outcome: if abort.is_aborted() && !source_changed {
                    CompactionOutcome::Cancelled
                } else {
                    CompactionOutcome::Failed
                },
                reason: Some(
                    if source_changed {
                        "source_changed"
                    } else if abort.is_aborted() {
                        "cancelled"
                    } else if self.compaction_source_revision.is_none() {
                        "source_read_failed"
                    } else {
                        "route_preparation_failed"
                    }
                    .into(),
                ),
                attempts: 0,
                completed_responses: 0,
                estimate_source: "local_history_estimate_v1".into(),
            });
            return Err(CompactionError::ApiError(error.to_string()));
        }
        let catalog = self.api_client.model_catalog();
        let operation = self.run_compaction_inner(&source, config, custom, options, run_id);
        if let Some(catalog) = catalog {
            catalog.scope(operation).await
        } else {
            operation.await
        }
    }

    // Finalize receipts for both completion and cancellation before returning to the owner.
    #[allow(clippy::too_many_lines)]
    async fn run_compaction_inner(
        &mut self,
        source: &Session,
        config: CompactionConfig,
        custom: Option<&str>,
        options: CompactionRunOptions,
        run_id: String,
    ) -> Result<CompactionResult, CompactionError> {
        use crate::compaction_scope::CompactionRequestScope;
        let before = estimate_session_tokens(source);
        let (budget, safe) = self.safe_history_budget(source);
        let target = (before / 2).min(safe);
        let ideal = (before * 30 / 100).min(target);
        let mut report = CompactionReport {
            run_id: run_id.clone(),
            before_history: before,
            after_history: before,
            target_history: Some(target),
            ideal_history: Some(ideal),
            source_safe_history_budget: safe,
            safe_history_budget: safe,
            actual_fixed_overhead: budget.overhead_tokens,
            method: None,
            outcome: CompactionOutcome::Failed,
            reason: None,
            attempts: 0,
            completed_responses: 0,
            estimate_source: "local_history_estimate_v1".into(),
        };
        let scope = CompactionRequestScope::new(run_id);
        let abort = self.hook_abort_signal.clone();
        let operation =
            self.run_compaction_attempts(source, config, custom, options, &scope, &mut report);
        let mut result = scope
            .scope(async {
                tokio::select! {
                    biased;
                    () = abort.cancelled() => Err(CompactionError::ApiError("cancelled".into())),
                    result = operation => result,
                }
            })
            .await;
        let snapshot = scope.snapshot();
        report.attempts = snapshot.attempts as usize;
        report.completed_responses = snapshot.completed_responses as usize;
        let new_usage = self.session.merge_maintenance_usage(&snapshot.receipts);
        self.record_compaction_maintenance_usage(new_usage);
        match &mut result {
            Ok(candidate) => {
                candidate.source_revision = self
                    .compaction_source_revision
                    .as_ref()
                    .and_then(CompactionSourceRevision::as_deref)
                    .map(str::to_owned);
                candidate
                    .compacted_session
                    .merge_maintenance_usage(&snapshot.receipts);
                if report.outcome == CompactionOutcome::TargetMet {
                    candidate.compacted_session.compaction_achievement =
                        Some(crate::session::CompactionAchievement {
                            fingerprint: self
                                .compaction_fingerprint(&candidate.compacted_session, custom),
                            before_history: before,
                            target_history: report.target_history.unwrap_or(target),
                            after_history: report.after_history,
                            policy_version: COMPACTION_POLICY_VERSION.into(),
                        });
                }
                candidate.report = Some(report.clone());
                candidate.compacted_session.last_compaction_report = Some(report.clone());
            }
            Err(error) => {
                report.outcome = if self.hook_abort_signal.is_aborted() {
                    CompactionOutcome::Cancelled
                } else {
                    CompactionOutcome::Failed
                };
                report.after_history = before;
                report.reason = Some(
                    match error {
                        _ if self.hook_abort_signal.is_aborted() => "cancelled",
                        CompactionError::InvalidSummary(text)
                            if text.contains("target not met") =>
                        {
                            "summary_target_unmet"
                        }
                        CompactionError::InvalidSummary(text)
                            if text.contains("protected history") =>
                        {
                            "retention_limited"
                        }
                        CompactionError::InvalidSummary(text)
                            if text.contains("exceed") && text.contains("context") =>
                        {
                            "required_context_exceeds_budget"
                        }
                        CompactionError::ApiError(text)
                            if text.contains("context budget")
                                || text.contains("output reservation") =>
                        {
                            "required_context_exceeds_budget"
                        }
                        _ if snapshot.attempts >= 4
                            || snapshot.completed_responses >= 2
                            || snapshot.outer_retries >= 2 =>
                        {
                            "attempts_exhausted"
                        }
                        CompactionError::InvalidSummary(_) => "invalid_summary",
                        _ => "provider_error",
                    }
                    .into(),
                );
                self.session.last_compaction_report = Some(report.clone());
                self.last_compaction_report = Some(report.clone());
                if !snapshot.receipts.is_empty() {
                    self.persist_compaction_maintenance();
                    report = self.last_compaction_report.clone().unwrap_or(report);
                }
            }
        }
        self.last_compaction_report = Some(report);
        result
    }

    // One run owns pruning, quality attempts and fallback; none may reset its quotas.
    #[allow(clippy::too_many_lines)]
    async fn run_compaction_attempts(
        &mut self,
        source: &Session,
        config: CompactionConfig,
        custom: Option<&str>,
        options: CompactionRunOptions,
        scope: &crate::compaction_scope::CompactionRequestScope,
        report: &mut CompactionReport,
    ) -> Result<CompactionResult, CompactionError> {
        let before = estimate_session_tokens(source);
        let (budget, _) = self.safe_history_budget(source);
        if !budget.fits_buffered(self.compaction_pending_input_tokens) {
            return Err(CompactionError::InvalidSummary(
                "fixed request overhead and reservations exceed the context window".into(),
            ));
        }
        let target = report.target_history.unwrap_or_default();
        let ideal = report.ideal_history.unwrap_or_default();
        let fingerprint = self.compaction_fingerprint(source, custom);
        if options.allow_repeat_skip
            && source.compaction_achievement.as_ref().is_some_and(|a| {
                a.fingerprint == fingerprint
                    && a.policy_version == COMPACTION_POLICY_VERSION
                    && a.after_history == before
                    && a.after_history <= a.target_history
                    && a.target_history <= a.before_history / 2
            })
            && before <= report.safe_history_budget
        {
            report.outcome = CompactionOutcome::Skipped;
            report.target_history = None;
            report.ideal_history = None;
            report.reason = Some("unchanged_achieved_target".into());
            return Ok(unchanged_compaction(source));
        }
        let mut frozen = source.clone();
        if options.allow_pruning && prune_tool_results(&mut frozen) > 0 {
            let after = estimate_session_tokens(&frozen);
            let (current_budget, current_safe) = self.safe_history_budget(&frozen);
            if after < before && after <= target.min(current_safe) {
                report.outcome = CompactionOutcome::TargetMet;
                report.method = Some("tool_pruning".into());
                report.after_history = after;
                report.safe_history_budget = current_safe;
                report.target_history = Some((before / 2).min(current_safe));
                report.ideal_history =
                    Some((before * 30 / 100).min(report.target_history.unwrap_or_default()));
                report.actual_fixed_overhead = current_budget.overhead_tokens;
                let mut result = unchanged_compaction(&frozen);
                result.summary_source = CompactionSummarySource::ToolPruning;
                return Ok(result);
            }
        }
        let config = self.compaction_retention(&frozen, config);
        let prepared = PreparedCompaction::new(&frozen, config, target, ideal);
        if !prepared.has_source() {
            if options.allow_repeat_skip && before <= report.safe_history_budget {
                report.outcome = CompactionOutcome::Skipped;
                report.target_history = None;
                report.ideal_history = None;
                report.reason = Some("nothing_compactable".into());
                return Ok(unchanged_compaction(source));
            }
            return Err(CompactionError::InvalidSummary(
                "no summarizable source and request is oversized".into(),
            ));
        }
        if prepared.raw_summary_budget == 0 {
            return Err(CompactionError::InvalidSummary(
                "protected history makes target infeasible".into(),
            ));
        }
        let mut cap = prepared.raw_summary_budget;
        let mut cache_safe = true;
        let model = self.compaction_model();
        let mut quality_responses = 0;
        loop {
            if cap == 0 {
                return Err(CompactionError::InvalidSummary(
                    "protected history makes revised target infeasible".into(),
                ));
            }
            let completed_before = scope.snapshot().completed_responses;
            let attempt = prepared
                .attempt(
                    &mut self.api_client,
                    &model,
                    &self.system_prompt,
                    CompactionAttemptOptions {
                        custom,
                        cache_safe,
                        visible_cap: cap,
                        quality_retry: quality_responses > 0,
                    },
                )
                .await;
            let mut snapshot = scope.snapshot();
            if snapshot.completed_responses == completed_before
                && (attempt.is_ok()
                    || attempt
                        .as_ref()
                        .err()
                        .is_some_and(RuntimeError::is_invalid_compaction_summary))
            {
                scope
                    .record_completion(None)
                    .map_err(|e| CompactionError::InvalidSummary(e.to_string()))?;
                snapshot = scope.snapshot();
            }
            let completed = snapshot.completed_responses > completed_before;
            if completed {
                quality_responses += 1;
            }
            match attempt {
                Ok(candidate) => {
                    let after = estimate_session_tokens(&candidate.compacted_session);
                    let (budget, current_safe) =
                        self.safe_history_budget(&candidate.compacted_session);
                    let current_target = (before / 2).min(current_safe);
                    report.safe_history_budget = current_safe;
                    report.actual_fixed_overhead = budget.overhead_tokens;
                    report.target_history = Some(current_target);
                    report.ideal_history = Some((before * 30 / 100).min(current_target));
                    if !prepared.todo_unchanged() {
                        return Err(CompactionError::InvalidSummary(
                            "Todo changed during compaction; retry with fresh source".into(),
                        ));
                    }
                    if after < before && after <= current_target {
                        report.after_history = after;
                        report.method = Some("llm_summary".into());
                        report.outcome = CompactionOutcome::TargetMet;
                        return Ok(candidate);
                    }
                    if quality_responses >= 2 || snapshot.completed_responses >= 2 {
                        return Err(CompactionError::InvalidSummary(format!(
                            "target not met: candidate {after}, target {current_target}"
                        )));
                    }
                    cap = cap
                        .saturating_sub(after.saturating_sub(current_target))
                        .min(cap * 3 / 4);
                }
                Err(error) => {
                    let message = error.to_string();
                    if cache_safe
                        && !completed
                        && (error.is_context_window_blocked()
                            || error.is_compaction_path_not_supported())
                    {
                        cache_safe = false;
                        continue;
                    }
                    if error.is_retryable()
                        && !error.is_context_window_blocked()
                        && snapshot.attempts < 4
                        && snapshot.completed_responses < 2
                        && crate::compaction_scope::claim_outer_retry()
                    {
                        tokio::time::sleep(std::time::Duration::from_secs(
                            1 << snapshot.outer_retries,
                        ))
                        .await;
                        continue;
                    }
                    if completed && error.is_invalid_compaction_summary() {
                        if cache_safe
                            && (message.contains("truncated")
                                || message.contains("visible output ceiling"))
                        {
                            cache_safe = false;
                        }
                        if quality_responses >= 2 || snapshot.completed_responses >= 2 {
                            return Err(CompactionError::InvalidSummary(message));
                        }
                        // A second completed response gets a tighter visible
                        // allowance even when changing provider request shape.
                        cap = cap * 3 / 4;
                        continue;
                    }
                    return Err(CompactionError::ApiError(message));
                }
            }
        }
    }

    /// Synchronous compaction (local heuristic only, no LLM call).
    #[must_use]
    pub fn compact_sync(&self, config: CompactionConfig) -> CompactionResult {
        compact_session_sync(&self.session, config)
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The system prompt sent with every request of this runtime.
    #[must_use]
    pub fn system_prompt(&self) -> &SystemPrompt {
        &self.system_prompt
    }

    #[must_use]
    pub fn api_client(&self) -> &C {
        &self.api_client
    }

    pub fn api_client_mut(&mut self) -> &mut C {
        &mut self.api_client
    }

    pub fn permission_policy_mut(&mut self) -> &mut PermissionPolicy {
        &mut self.permission_policy
    }

    /// Access the hook abort signal for external cancellation.
    #[must_use]
    pub fn hook_abort_signal(&self) -> &HookAbortSignal {
        &self.hook_abort_signal
    }

    pub fn tool_executor_mut(&mut self) -> &mut T {
        Arc::get_mut(&mut self.tool_executor)
            .expect("executor outside dispatch")
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Access the file tracker for the current turn.
    #[must_use]
    pub fn file_tracker(&self) -> &crate::file_tracker::TurnFileTracker {
        &self.file_tracker
    }

    /// Access the file tracker mutably.
    pub fn file_tracker_mut(&mut self) -> &mut crate::file_tracker::TurnFileTracker {
        &mut self.file_tracker
    }

    /// Get the current turn ID.
    #[must_use]
    pub fn current_turn_id(&self) -> Option<&str> {
        self.current_turn_id.as_deref()
    }

    /// Get the user request intent for the current turn.
    #[must_use]
    pub fn user_request_intent(&self) -> Option<&crate::file_intent::UserRequestIntent> {
        self.user_request_intent.as_ref()
    }

    /// Cleanup draft files for the current turn (call on abort).
    /// Returns paths of cleaned files.
    pub fn cleanup_current_turn_drafts(&mut self) -> Vec<std::path::PathBuf> {
        if let Some(turn_id) = self.current_turn_id.clone() {
            self.file_tracker
                .cleanup_turn_drafts(&turn_id, self.session.fs_handle().as_ref())
        } else {
            Vec::new()
        }
    }

    /// Rollback all file operations for the current turn (call on abort).
    /// Returns error messages for failed operations.
    pub fn rollback_current_turn(&mut self) -> Vec<String> {
        if let Some(turn_id) = self.current_turn_id.clone() {
            self.file_tracker
                .rollback_turn(&turn_id, self.session.fs_handle().as_ref())
        } else {
            Vec::new()
        }
    }

    fn has_unfinished_requested_deliverable_after_tool_empty(
        &self,
        assistant_messages: &[ConversationMessage],
        tool_results: &[ConversationMessage],
    ) -> bool {
        let Some(intent) = self.user_request_intent.as_ref() else {
            return false;
        };
        if !intent.expects_deliverable() {
            return false;
        }
        if tool_results_include_requested_deliverable(tool_results, intent) {
            return false;
        }
        assistant_messages
            .last()
            .is_some_and(message_has_generation_tool_use)
    }

    #[must_use]
    pub fn fork_session(&self, branch_name: Option<String>) -> Session {
        self.session.fork(branch_name)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    /// Use the active provider route for capability lookups. Session metadata
    /// can contain a config alias or the model used before a resume/switch.
    /// An unknown model uses generic capability limits, never another model's
    /// identity. Compaction requests reuse the current API client's route.
    fn compaction_model(&self) -> String {
        self.api_client
            .wire_model_id()
            .filter(|model| !model.is_empty())
            .or_else(|| Some(self.running_model()).filter(|model| !model.is_empty()))
            .unwrap_or_default()
            .to_owned()
    }

    /// Whether the request this iteration is about to build would not fit.
    ///
    /// Both available signals are consulted, because each is blind where the
    /// other sees. The local estimate covers history the provider has never
    /// counted — the tool results a long turn keeps pushing — but it is only
    /// a character-count heuristic. The provider's reported context is exact
    /// for everything it has already processed but says nothing about what
    /// was pushed since. Either one over its own budget means compact.
    fn next_request_exceeds_budget(&self) -> bool {
        let (budget, _) = self.safe_history_budget(&self.session);
        !budget.fits_buffered(estimate_session_tokens(&self.session))
            || self.projected_context_tokens() as usize > budget.reported_context_budget()
    }

    /// Best estimate of the context the next request will carry: the context
    /// the provider reported for the latest response, plus everything pushed
    /// since that response (tool results, injected reminders) that no usage
    /// report has counted yet. At the start of a turn there is no reported
    /// context and this is near zero — the per-turn preflight covers that
    /// point; between iterations of a tool loop it is what notices growth
    /// the local estimate undercounts.
    fn projected_context_tokens(&self) -> u32 {
        let reported = self.usage_tracker.current_context_tokens();
        let unreported: usize = self
            .session
            .messages
            .iter()
            .rev()
            .take_while(|message| message.role != MessageRole::Assistant)
            .flat_map(|message| message.blocks.iter())
            .map(estimate_block_tokens)
            .sum();
        reported.saturating_add(u32::try_from(unreported).unwrap_or(u32::MAX))
    }

    async fn maybe_auto_compact(
        &mut self,
        observer: Option<&mut dyn RuntimeObserver>,
    ) -> Result<Option<AutoCompactionEvent>, RuntimeError> {
        let model = self.compaction_model();
        let threshold = auto_compact_threshold_for_model(&model);
        // Compare the context the provider actually processed on the latest
        // response (uncached input + cache reads + cache writes) against the
        // window. The session-wide cumulative input count is the wrong
        // metric on both kinds of provider: with prompt caching it is a few
        // hundred tokens per turn and never reaches the threshold; without
        // caching it grows quadratically, never decreases, and re-compacts
        // after every turn once crossed.
        if self.usage_tracker.current_context_tokens() < threshold {
            return Ok(None);
        }

        // Circuit-breaker: once N consecutive turns have tried + no-op'd,
        // stop attempting. See MAX_CONSECUTIVE_AUTO_COMPACT_NOOPS docs for
        // rationale (CC parity; bounds noise floor when session is
        // structurally pinned above the threshold).
        if self.consecutive_auto_compact_noops >= MAX_CONSECUTIVE_AUTO_COMPACT_NOOPS {
            return Ok(None);
        }

        let event = self
            .compact_in_place(
                forced_compaction_config(),
                CompactionTrigger::PostTurnUsage,
                observer,
            )
            .await
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        if event.is_some() {
            // Success → reset the noop counter so the breaker only trips on
            // SUSTAINED inability to shrink, not on transient threshold dance.
            self.consecutive_auto_compact_noops = 0;
        } else {
            self.consecutive_auto_compact_noops =
                self.consecutive_auto_compact_noops.saturating_add(1);
        }
        Ok(event)
    }

    /// Stage pruning and summarization on a clone. Only install a useful,
    /// durably saved replacement; errors never erase the original transcript.
    pub async fn compact_in_place(
        &mut self,
        config: CompactionConfig,
        trigger: CompactionTrigger,
        mut observer: Option<&mut dyn RuntimeObserver>,
    ) -> Result<Option<AutoCompactionEvent>, CompactionError> {
        let before = estimate_session_tokens(&self.session);
        let mut progress = CompactionProgress::started(trigger.as_str(), before);
        if let Some(observer) = observer.as_deref_mut() {
            observer.on_compaction(&progress);
        }
        let result = self
            .compact_in_place_inner(config, trigger, progress.id.clone())
            .await;
        // A durable commit wins over a cancellation arriving afterwards.
        progress.status = match self
            .last_compaction_report
            .as_ref()
            .map(|report| report.outcome)
        {
            Some(CompactionOutcome::TargetMet | CompactionOutcome::Skipped) => {
                CompactionStatus::Completed
            }
            Some(CompactionOutcome::Cancelled) => CompactionStatus::Cancelled,
            None if result.is_ok() => CompactionStatus::Completed,
            None if self.hook_abort_signal.is_aborted() => CompactionStatus::Cancelled,
            Some(CompactionOutcome::Failed) | None => CompactionStatus::Failed,
        };
        progress.after_tokens = Some(estimate_session_tokens(&self.session));
        progress.report = self.last_compaction_report.clone();
        if let Some(observer) = observer {
            observer.on_compaction(&progress);
        }
        result.map_err(CompactionError::into_terminal)
    }

    async fn compact_in_place_inner(
        &mut self,
        config: CompactionConfig,
        trigger: CompactionTrigger,
        run_id: String,
    ) -> Result<Option<AutoCompactionEvent>, CompactionError> {
        let source = self.session.clone();
        let before = estimate_session_tokens(&source);
        let result = self
            .run_compaction(
                &source,
                config,
                None,
                CompactionRunOptions {
                    allow_pruning: true,
                    allow_repeat_skip: trigger != CompactionTrigger::ProviderRejection,
                },
                run_id,
            )
            .await?;
        if result.compacted_session.messages == self.session.messages {
            if let Some(report) = &result.report {
                self.set_compaction_report(report.clone());
            }
            self.persist_compaction_maintenance();
            if self.last_compaction_report.as_ref().is_some_and(|report| {
                report.outcome == CompactionOutcome::Failed
                    && report
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.starts_with("source_changed"))
            }) {
                return Err(CompactionError::Persistence(
                    SessionError::SourceChanged.to_string(),
                ));
            }
            return Ok(None);
        }
        if self.hook_abort_signal.is_aborted() {
            if let Some(report) = &mut self.last_compaction_report {
                report.outcome = CompactionOutcome::Cancelled;
                report.after_history = before;
                report.reason = Some("cancelled_before_commit".into());
            }
            self.persist_compaction_maintenance();
            return Err(CompactionError::ApiError("cancelled".into()));
        }
        if let Some(path) = self.session.persistence_path() {
            if let Err(error) = result.compacted_session.save_compacted_to_path_checked(
                &self.session,
                path,
                result.source_revision.as_deref(),
            ) {
                let source_changed = matches!(error, SessionError::SourceChanged);
                let refresh_error = if source_changed {
                    self.compaction_source_revision = None;
                    self.refresh_compaction_source().err()
                } else {
                    None
                };
                let active_history = estimate_session_tokens(&self.session);
                if let Some(report) = &mut self.last_compaction_report {
                    report.outcome = CompactionOutcome::Failed;
                    report.after_history = active_history;
                    report.reason = Some(if source_changed {
                        refresh_error.map_or_else(
                            || "source_changed; maintenance metadata remains in memory".into(),
                            |refresh| format!("source_changed; source reload failed: {refresh}; maintenance metadata remains in memory"),
                        )
                    } else {
                        "persistence_failed".into()
                    });
                }
                if source_changed {
                    self.session
                        .last_compaction_report
                        .clone_from(&self.last_compaction_report);
                } else {
                    self.persist_compaction_maintenance();
                }
                return Err(CompactionError::Persistence(error.to_string()));
            }
        }
        let after = estimate_session_tokens(&result.compacted_session);
        let event = AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
            report: result.report.clone(),
        };
        let removed = result.removed_message_count;
        let summary_source = result.summary_source;
        self.install_compacted_session(result.compacted_session);
        self.record_compaction(trigger, removed, before, after, Some(&summary_source));
        Ok(Some(event))
    }

    pub fn persist_compaction_maintenance(&mut self) {
        self.session.last_compaction_report = self.last_compaction_report.clone();
        let Some(expected_revision) = self.compaction_source_revision.clone() else {
            return;
        };
        if let Err(error) = self
            .session
            .persist_maintenance_metadata_checked(expected_revision.as_deref())
        {
            let source_changed = matches!(error, SessionError::SourceChanged);
            let refresh_error = if source_changed {
                self.compaction_source_revision = None;
                self.refresh_compaction_source().err()
            } else {
                None
            };
            let active_history = estimate_session_tokens(&self.session);
            if let Some(report) = &mut self.last_compaction_report {
                if source_changed {
                    report.outcome = CompactionOutcome::Failed;
                    report.after_history = active_history;
                    report.reason = Some(refresh_error.map_or_else(
                        || "source_changed".into(),
                        |refresh| format!("source_changed; source reload failed: {refresh}"),
                    ));
                }
                report.reason = Some(format!(
                    "{}; maintenance metadata could not be saved: {error}",
                    report.reason.as_deref().unwrap_or("failed")
                ));
                self.session.last_compaction_report = Some(report.clone());
            }
            self.record_session_persist_error("compaction_maintenance_usage", &error.to_string());
        } else {
            // A second owner finalize must compare with our own metadata write.
            match self.session.capture_durable_revision_for_history() {
                Ok((revision, new_usage)) => {
                    self.record_compaction_maintenance_usage(new_usage);
                    self.compaction_source_revision = Some(revision.into());
                    self.session_source_stale = false;
                }
                Err(error) => {
                    self.compaction_source_revision = None;
                    self.session_source_stale = true;
                    let source_changed = matches!(error, SessionError::SourceChanged);
                    let refresh_error = if source_changed {
                        self.refresh_compaction_source().err()
                    } else {
                        None
                    };
                    let active_history = estimate_session_tokens(&self.session);
                    if let Some(report) = &mut self.last_compaction_report {
                        report.outcome = CompactionOutcome::Failed;
                        report.after_history = active_history;
                        report.reason = Some(if source_changed {
                            refresh_error.map_or_else(
                                || "source_changed".into(),
                                |refresh| {
                                    format!("source_changed; source reload failed: {refresh}")
                                },
                            )
                        } else {
                            format!("source_revision_read_failed: {error}")
                        });
                        self.session.last_compaction_report = Some(report.clone());
                    }
                    self.record_session_persist_error(
                        "compaction_maintenance_revision",
                        &error.to_string(),
                    );
                }
            }
        }
    }

    fn record_compaction_maintenance_usage(&mut self, new_usage: Vec<TokenUsage>) {
        for usage in new_usage {
            self.usage_tracker.record_maintenance(usage);
        }
        self.usage_tracker.set_unknown_maintenance_receipts(
            self.session
                .maintenance_usage
                .iter()
                .filter(|receipt| receipt.usage.is_none())
                .count(),
        );
    }

    fn record_session_persist_error(&self, operation: &str, error: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };
        let mut attributes = Map::new();
        attributes.insert(
            "operation".to_string(),
            Value::String(operation.to_string()),
        );
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("session_persist_error", attributes);
    }

    fn record_compaction(
        &self,
        trigger: CompactionTrigger,
        removed_message_count: usize,
        estimated_before: usize,
        estimated_after: usize,
        summary_source: Option<&CompactionSummarySource>,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };
        let mut attributes = Map::new();
        attributes.insert(
            "trigger".to_string(),
            Value::String(trigger.as_str().to_string()),
        );
        attributes.insert(
            "removed_messages".to_string(),
            Value::from(removed_message_count as u64),
        );
        attributes.insert(
            "estimated_tokens_before".to_string(),
            Value::from(estimated_before as u64),
        );
        attributes.insert(
            "estimated_tokens_after".to_string(),
            Value::from(estimated_after as u64),
        );
        // `local` means the structural fallback ran: it counts messages and
        // lists tool names, it does not summarise content. Telling the two
        // apart is the difference between "the summary is thin" and "the
        // summariser never ran".
        if let Some(source) = summary_source {
            attributes.insert(
                "summary_source".to_string(),
                Value::String(source.to_string()),
            );
        }
        session_tracer.record("session_compacted", attributes);
    }

    /// The turn's compaction allowance is spent while a guard still wants to
    /// compact. Recorded once per turn: it is the difference between "the
    /// context guard never fired" and "it fired until it ran out", which is
    /// the first thing to know when a turn dies of context overflow.
    fn record_compaction_budget_exhausted(&self, trigger: CompactionTrigger) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };
        let mut attributes = Map::new();
        attributes.insert(
            "trigger".to_string(),
            Value::String(trigger.as_str().to_string()),
        );
        attributes.insert(
            "max_turn_compactions".to_string(),
            Value::from(MAX_TURN_COMPACTIONS as u64),
        );
        attributes.insert(
            "estimated_tokens".to_string(),
            Value::from(estimate_session_tokens(&self.session) as u64),
        );
        session_tracer.record("session_compaction_budget_exhausted", attributes);
    }

    fn record_turn_started(&self, user_input: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "user_input".to_string(),
            Value::String(user_input.to_string()),
        );
        session_tracer.record("turn_started", attributes);
    }

    fn record_assistant_iteration(
        &self,
        iteration: usize,
        assistant_message: &ConversationMessage,
        pending_tool_use_count: usize,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "assistant_blocks".to_string(),
            Value::from(assistant_message.blocks.len() as u64),
        );
        attributes.insert(
            "pending_tool_use_count".to_string(),
            Value::from(pending_tool_use_count as u64),
        );
        session_tracer.record("assistant_iteration_completed", attributes);
    }

    fn record_tool_started(&self, iteration: usize, tool_name: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        session_tracer.record("tool_execution_started", attributes);
    }

    fn record_tool_finished(&self, iteration: usize, result_message: &ConversationMessage) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let Some(ContentBlock::ToolResult {
            tool_name,
            is_error,
            ..
        }) = result_message.blocks.first()
        else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("tool_name".to_string(), Value::String(tool_name.clone()));
        attributes.insert("is_error".to_string(), Value::Bool(*is_error));
        session_tracer.record("tool_execution_finished", attributes);
    }

    fn record_empty_post_tool_completion(&self, iteration: usize) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        session_tracer.record("empty_post_tool_completion", attributes);
    }

    fn record_turn_completed(&self, summary: &TurnSummary) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let model_request_count = summary
            .assistant_messages
            .iter()
            .filter(|m| m.usage.is_some())
            .count() as u64;
        let mut attributes = Map::new();
        attributes.insert(
            "iterations".to_string(),
            Value::from(summary.iterations as u64),
        );
        attributes.insert(
            "assistant_messages".to_string(),
            Value::from(summary.assistant_messages.len() as u64),
        );
        attributes.insert(
            "tool_results".to_string(),
            Value::from(summary.tool_results.len() as u64),
        );
        attributes.insert(
            "prompt_cache_events".to_string(),
            Value::from(summary.prompt_cache_events.len() as u64),
        );
        attributes.insert(
            "model_request_count".to_string(),
            Value::from(model_request_count),
        );
        attributes.insert(
            "turn_total_tokens".to_string(),
            Value::from(summary.turn_usage.total_tokens() as u64),
        );
        attributes.insert(
            "session_total_tokens".to_string(),
            Value::from(summary.session_usage.total_tokens() as u64),
        );
        session_tracer.record("turn_completed", attributes);
    }

    fn record_turn_failed(&self, iteration: usize, error: &RuntimeError) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }
}

/// Combine the compaction events of one turn (overflow recovery plus the
/// post-turn auto-compaction) into the single event reported to callers.
fn merge_auto_compaction(
    first: Option<AutoCompactionEvent>,
    second: Option<AutoCompactionEvent>,
) -> Option<AutoCompactionEvent> {
    match (first, second) {
        (None, None) => None,
        (Some(event), None) | (None, Some(event)) => Some(event),
        (Some(a), Some(b)) => Some(AutoCompactionEvent {
            removed_message_count: a.removed_message_count + b.removed_message_count,
            report: b.report,
        }),
    }
}

/// Per-model auto-compact threshold:
/// `context_window - request max_tokens - buffer`.
///
/// The provider rejects a request once `input + max_tokens` passes the
/// window, and the API client's local preflight mirrors that same sum. The
/// output reservation subtracted here is therefore the `max_tokens` chat
/// requests are actually sent with
/// ([`crate::model_capabilities::request_max_output_tokens`]), not the
/// compaction summary's much smaller cap: with a 200K window and 64K
/// `max_tokens` the provider rejects at 136K, so the old
/// `min(max_output, 20K)` form put the threshold at 167K — above the entire
/// band where rejections happen, where it could never fire in time.
///
/// Falls back to the env-var override when set.
#[must_use]
pub fn auto_compact_threshold_for_model(model: &str) -> u32 {
    // Env-var override takes precedence (explicit user intent).
    if let Ok(raw) = std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR) {
        if let Ok(threshold) = raw.trim().parse::<u32>() {
            if threshold > 0 {
                return threshold;
            }
        }
    }

    let context_window = crate::model_capabilities::context_window_or_default(model);
    let max_output = crate::model_capabilities::request_max_output_tokens(model);
    let buffer = autocompact_buffer_tokens(model);
    context_window.saturating_sub(max_output.saturating_add(buffer))
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
        Option<String>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut finished = false;
    let mut usage = None;
    let mut response_model = None;

    for event in events {
        match event {
            AssistantEvent::ThinkingStart => start_thinking_block(&mut text, &mut blocks),
            AssistantEvent::Model(model) => {
                response_model = Some(model);
            }
            AssistantEvent::Thinking {
                thinking,
                signature,
            } => {
                flush_text_block(&mut text, &mut blocks);
                push_thinking_block(&mut blocks, thinking, signature);
            }
            // Kept, not rendered: there is nothing legible in it, but the next
            // request has to replay it or the cached prefix is rebuilt.
            AssistantEvent::RedactedThinking { data } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::RedactedThinking { data });
            }
            AssistantEvent::TextDelta(delta) => {
                text.push_str(&delta);
            }
            AssistantEvent::ToolUse {
                id,
                name,
                input,
                thought_signature,
            } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                });
            }
            AssistantEvent::Usage(value) => usage = Some(value),
            AssistantEvent::PromptCache(event) => prompt_cache_events.push(event),
            AssistantEvent::MessageStop => {
                finished = true;
            }
        }
    }

    finish_assistant_blocks(&mut text, &mut blocks);

    if !finished {
        return Err(RuntimeError::new(
            "assistant stream ended without a message stop event",
        ));
    }
    if blocks.is_empty() {
        return Err(RuntimeError::new("assistant stream produced no content"));
    }

    Ok((
        ConversationMessage::assistant_with_usage(blocks, usage),
        usage,
        prompt_cache_events,
        response_model,
    ))
}

/// Sums the token usage from all assistant messages in a turn.
/// Each assistant message carries the usage from one model request.
fn sum_assistant_message_usage(messages: &[ConversationMessage]) -> TokenUsage {
    let mut total = UsageAggregation::default();
    for message in messages {
        if let Some(usage) = message.usage {
            total.push(usage);
        }
    }
    total.finish()
}

fn has_pending_tool_uses(message: &ConversationMessage) -> bool {
    message
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
}

fn message_has_generation_tool_use(message: &ConversationMessage) -> bool {
    message.blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolUse { name, .. } if is_generation_tool_name(name)
        )
    })
}

fn is_generation_tool_name(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "bash" | "PowerShell")
}

fn tool_results_include_requested_deliverable(
    tool_results: &[ConversationMessage],
    intent: &crate::file_intent::UserRequestIntent,
) -> bool {
    tool_results.iter().any(|message| {
        message.blocks.iter().any(|block| {
            let ContentBlock::ToolResult {
                output, is_error, ..
            } = block
            else {
                return false;
            };
            if *is_error {
                return false;
            }
            serde_json::from_str::<Value>(output)
                .ok()
                .is_some_and(|value| json_contains_requested_deliverable_path(&value, intent))
        })
    })
}

fn json_contains_requested_deliverable_path(
    value: &Value,
    intent: &crate::file_intent::UserRequestIntent,
) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            let is_path_key = key.contains("path") || key == "filename" || key == "file";
            if is_path_key
                && value
                    .as_str()
                    .is_some_and(|path| is_final_requested_deliverable_path(path, intent))
            {
                return true;
            }
            if key == "content" || key == "stdout" || key == "stderr" {
                return false;
            }
            json_contains_requested_deliverable_path(value, intent)
        }),
        Value::Array(values) => values
            .iter()
            .any(|value| json_contains_requested_deliverable_path(value, intent)),
        _ => false,
    }
}

fn is_final_requested_deliverable_path(
    path: &str,
    intent: &crate::file_intent::UserRequestIntent,
) -> bool {
    let normalized = path.replace('\\', "/");
    if normalized.starts_with(".drafts/") || normalized.contains("/.drafts/") {
        return false;
    }
    intent.is_requested_deliverable_path(&normalized)
}

fn notify_tool_result(
    observer: Option<&mut dyn RuntimeObserver>,
    result_message: &ConversationMessage,
) {
    let Some(observer) = observer else {
        return;
    };
    let Some(ContentBlock::ToolResult {
        tool_use_id,
        tool_name,
        output,
        is_error,
    }) = result_message.blocks.first()
    else {
        return;
    };

    observer.on_tool_result(tool_use_id, tool_name, output, *is_error);
}

fn runtime_observer_mut<'a>(
    observer: &'a mut Option<&mut dyn RuntimeObserver>,
) -> Option<&'a mut dyn RuntimeObserver> {
    observer
        .as_mut()
        .map(|observer| &mut **observer as &mut dyn RuntimeObserver)
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

fn push_thinking_block(
    blocks: &mut Vec<ContentBlock>,
    thinking: String,
    signature: Option<String>,
) {
    if let Some(ContentBlock::Thinking {
        thinking: existing,
        signature: existing_signature,
    }) = blocks.last_mut()
    {
        existing.push_str(&thinking);
        // Append, don't just fill: a signature can arrive in several deltas, and
        // keeping only the first chunk produces a block the server rejects as
        // unverifiable — worse than having none, because it looks signed.
        if let Some(chunk) = signature {
            existing_signature
                .get_or_insert_with(String::new)
                .push_str(&chunk);
        }
        return;
    }

    // Ignore orphan deltas, not valid empty blocks. ThinkingStart establishes
    // those blocks explicitly, including display: omitted and progress updates.
    if thinking.is_empty() {
        return;
    }

    blocks.push(ContentBlock::Thinking {
        thinking,
        signature,
    });
}

fn start_thinking_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    flush_text_block(text, blocks);
    blocks.push(ContentBlock::Thinking {
        thinking: String::new(),
        signature: None,
    });
}

fn finish_assistant_blocks(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    flush_text_block(text, blocks);
    // A start frame is a boundary, not content. Keep signed empty blocks and
    // readable summaries while dropping placeholders that never received data.
    blocks.retain(|block| {
        !matches!(block, ContentBlock::Thinking { thinking, signature }
            if thinking.is_empty() && signature.as_deref().is_none_or(str::is_empty))
    });
}

#[cfg(test)]
fn is_concurrency_safe_tool(tool_name: &str) -> bool {
    crate::tool_concurrency::builtin_is_concurrency_safe(tool_name, "{}")
}

/// Ceiling on how many tools of one concurrency-safe batch execute at once.
///
/// Claude Code's number (`toolOrchestration.ts`:
/// `CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY || 10`). Until sub-agents joined the
/// set above there was no ceiling at all — the batch went to `join_all` whole —
/// which was survivable for file reads and greps and is not now that one batch
/// entry can be a full model session: an unbounded fan-out arrives upstream as
/// that many simultaneous requests against a single account.
const MAX_TOOL_USE_CONCURRENCY: usize = 10;

/// `SUDOCODE_MAX_TOOL_USE_CONCURRENCY` overrides [`MAX_TOOL_USE_CONCURRENCY`].
///
/// Worth a knob for the reason Claude Code has one: the useful ceiling is set
/// by what the upstream account will run at once, not by this process. Past
/// that point the surplus does not merely wait — a pooled deployment can route
/// the overflow to a different account, where the system prompt and tools this
/// batch is sharing have never been cached, so every spilled sub-agent pays to
/// create the prefix again.
///
/// `0` and unparseable values fall back to the default rather than meaning
/// "no limit": a zero would stall the batch forever.
fn max_tool_use_concurrency() -> usize {
    std::env::var("SUDOCODE_MAX_TOOL_USE_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(MAX_TOOL_USE_CONCURRENCY)
}

fn format_hook_message(result: &HookRunResult, fallback: &str) -> String {
    if result.messages().is_empty() {
        fallback.to_string()
    } else {
        result.messages().join("\n")
    }
}

fn merge_hook_feedback(messages: &[String], output: String, is_error: bool) -> String {
    if messages.is_empty() {
        return output;
    }

    let mut sections = Vec::new();
    if !output.trim().is_empty() {
        sections.push(output);
    }
    let label = if is_error {
        "Hook feedback (error)"
    } else {
        "Hook feedback"
    };
    sections.push(format!("{label}:\n{}", messages.join("\n")));
    sections.join("\n\n")
}

type ToolHandler = Box<dyn Fn(&str) -> Result<String, ToolError> + Send + Sync>;

/// Simple in-memory tool executor for tests and lightweight integrations.
#[derive(Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl Fn(&str) -> Result<String, ToolError> + Send + Sync + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    async fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

/// Per-tool inline budget for a tool result, in bytes; above it the full
/// result is spilled to the session's tool-results dir and replaced with a
/// preview + marker. Tiered like CC's `maxResultSizeChars`: bash 30 000,
/// grep 20 000, everything else 50 000. `None` = never offload — `read_file`
/// paginates itself (a file read is always wanted in full, so spilling it
/// would only cost round-trips), matching CC's `Read` which opts out of the
/// persistence path entirely.
pub(crate) fn offload_threshold_for(tool_name: &str) -> Option<usize> {
    // Model-facing names may be CC-style PascalCase aliases (`Read`, `Bash`)
    // or scode's snake_case; compare case-insensitively on both spellings.
    let lower = tool_name.to_ascii_lowercase();
    match lower.as_str() {
        "read" | "read_file" | "read_tool_output" => None,
        "bash" | "powershell" => Some(OFFLOAD_THRESHOLD_BASH),
        "grep" | "grep_search" => Some(OFFLOAD_THRESHOLD_GREP),
        _ => Some(OFFLOAD_THRESHOLD_DEFAULT),
    }
}

/// Inline budget for `bash` output before offload (CC: 30 000 chars).
pub(crate) const OFFLOAD_THRESHOLD_BASH: usize = 30_000;
/// Inline budget for `grep_search` output before offload (CC: 20 000 chars).
pub(crate) const OFFLOAD_THRESHOLD_GREP: usize = 20_000;
/// Inline budget for every other tool (CC: min(maxResultSizeChars, 50 000)).
pub(crate) const OFFLOAD_THRESHOLD_DEFAULT: usize = 50_000;
/// Bytes of the head kept inline as a preview when offloading (CC: 2000
/// chars). Small on purpose — it exists to let the model recognise what the
/// result is; locating content is `read_tool_output`'s job.
pub(crate) const OFFLOAD_PREVIEW_BYTES: usize = 2_000;

/// End index of the inline preview: [`OFFLOAD_PREVIEW_BYTES`], pulled back
/// to the last newline when one falls in the second half of the window (so
/// the preview ends on a whole line), and always on a char boundary.
fn offload_preview_end(output: &str) -> usize {
    let mut end = OFFLOAD_PREVIEW_BYTES.min(output.len());
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    match output[..end].rfind('\n') {
        Some(nl) if nl * 2 > end => nl,
        _ => end,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        auto_compact_threshold_for_model, build_assistant_message, is_concurrency_safe_tool,
        max_tool_use_concurrency, push_thinking_block, ApiClient, ApiRequest, AssistantEvent,
        AssistantEventStream, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent,
        RuntimeError, RuntimeObserver, StaticToolExecutor, ToolExecutor, MAX_TOOL_USE_CONCURRENCY,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPrompt, SystemPromptBuilder};

    /// Serialises tests that mutate `CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
    use crate::session::{ContentBlock, MessageRole, Session};
    use crate::usage::{TokenUsage, UsageCostCurrency};
    use crate::ToolError;
    use async_trait::async_trait;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    /// Helper: convert a `Vec<AssistantEvent>` into an [`AssistantEventStream`].
    fn events_to_stream(events: Vec<AssistantEvent>) -> AssistantEventStream {
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    struct ScriptedApiClient {
        call_count: usize,
    }

    #[async_trait]
    impl ApiClient for ScriptedApiClient {
        async fn stream(
            &mut self,
            request: ApiRequest,
        ) -> Result<AssistantEventStream, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("Let me calculate that.".to_string()),
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                            ..TokenUsage::default()
                        }),
                        AssistantEvent::MessageStop,
                    ]))
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("The answer is 4.".to_string()),
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                            ..TokenUsage::default()
                        }),
                        AssistantEvent::PromptCache(PromptCacheEvent {
                            unexpected: true,
                            reason:
                                "cache read tokens dropped while prompt fingerprint remained stable"
                                    .to_string(),
                            previous_cache_read_input_tokens: 6_000,
                            current_cache_read_input_tokens: 1_000,
                            token_drop: 5_000,
                        }),
                        AssistantEvent::MessageStop,
                    ]))
                }
                _ => unreachable!("extra API call"),
            }
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ObservedRuntimeEvent {
        ThinkingDelta(String),
        TextDelta(String),
        ToolUse {
            id: String,
            name: String,
            input: String,
        },
        ToolResult {
            tool_use_id: String,
            tool_name: String,
            output: String,
            is_error: bool,
        },
    }

    #[derive(Default)]
    struct RecordingRuntimeObserver {
        events: Vec<ObservedRuntimeEvent>,
    }

    impl RuntimeObserver for RecordingRuntimeObserver {
        fn on_thinking_delta(&mut self, delta: &str) {
            self.events
                .push(ObservedRuntimeEvent::ThinkingDelta(delta.to_string()));
        }

        fn on_text_delta(&mut self, delta: &str) {
            self.events
                .push(ObservedRuntimeEvent::TextDelta(delta.to_string()));
        }

        fn on_tool_use(&mut self, id: &str, name: &str, input: &str) {
            self.events.push(ObservedRuntimeEvent::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: input.to_string(),
            });
        }

        fn on_tool_result(
            &mut self,
            tool_use_id: &str,
            tool_name: &str,
            output: &str,
            is_error: bool,
        ) {
            self.events.push(ObservedRuntimeEvent::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                tool_name: tool_name.to_string(),
                output: output.to_string(),
                is_error,
            });
        }
    }

    struct ThinkingApiClient;

    #[async_trait]
    impl ApiClient for ThinkingApiClient {
        async fn stream(
            &mut self,
            _request: ApiRequest,
        ) -> Result<AssistantEventStream, RuntimeError> {
            Ok(events_to_stream(vec![
                AssistantEvent::Thinking {
                    thinking: "considering the request".to_string(),
                    signature: None,
                },
                AssistantEvent::TextDelta("done".to_string()),
                AssistantEvent::MessageStop,
            ]))
        }
    }

    /// One assistant message emitting three read-only (`read_file`) tool
    /// calls, then end-turn — drives a single concurrency-safe batch.
    struct ConcurrencyProbeClient {
        call_count: usize,
    }

    #[async_trait]
    impl ApiClient for ConcurrencyProbeClient {
        async fn stream(
            &mut self,
            _request: ApiRequest,
        ) -> Result<AssistantEventStream, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "r1".to_string(),
                        name: "read_file".to_string(),
                        input: "{\"path\":\"a\"}".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::ToolUse {
                        id: "r2".to_string(),
                        name: "read_file".to_string(),
                        input: "{\"path\":\"b\"}".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::ToolUse {
                        id: "r3".to_string(),
                        name: "read_file".to_string(),
                        input: "{\"path\":\"c\"}".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            } else {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }
    }

    /// Records the peak number of simultaneously-in-flight executes. A serial
    /// loop peaks at 1; a concurrency-safe batch peaks at the batch size.
    struct PeakConcurrencyProbe {
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ToolExecutor for PeakConcurrencyProbe {
        async fn execute(&self, _tool_name: &str, _input: &str) -> Result<String, ToolError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            // Yield long enough for the sibling futures to also enter before any
            // completes — deterministic on the single-threaded test runtime.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.active.fetch_sub(1, SeqCst);
            Ok("ok".to_string())
        }
    }

    /// One assistant message emitting `calls` read-only tool calls, then
    /// end-turn. Parameterised so a test can ask for a batch deliberately
    /// wider than the concurrency ceiling.
    struct WideBatchClient {
        call_count: usize,
        calls: usize,
    }

    #[async_trait]
    impl ApiClient for WideBatchClient {
        async fn stream(
            &mut self,
            _request: ApiRequest,
        ) -> Result<AssistantEventStream, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                let uses = (0..self.calls)
                    .map(|i| AssistantEvent::ToolUse {
                        id: format!("r{i}"),
                        name: "read_file".to_string(),
                        input: format!("{{\"path\":\"f{i}\"}}"),
                        thought_signature: None,
                    })
                    .chain(std::iter::once(AssistantEvent::MessageStop))
                    .collect();
                Ok(events_to_stream(uses))
            } else {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }
    }

    /// Claude Code caps a concurrency-safe batch at 10
    /// (`toolOrchestration.ts`). Pinned in its own test so the parity is a
    /// stated fact rather than a coincidence, and so a change to the number
    /// has to be deliberate.
    #[test]
    fn tool_use_concurrency_ceiling_matches_claude_code() {
        assert_eq!(MAX_TOOL_USE_CONCURRENCY, 10);
    }

    /// A batch wider than the ceiling must not run wide. Before the ceiling
    /// existed this peaked at the batch size, which only became dangerous once
    /// `agent_spawn` joined the concurrency-safe set: each entry is then a full
    /// model session, and the surplus lands upstream as simultaneous requests
    /// on one account.
    ///
    /// Sized from `max_tool_use_concurrency()` rather than the constant so an
    /// environment that overrides the limit still exercises the cap instead of
    /// failing for the wrong reason.
    #[tokio::test]
    async fn a_batch_wider_than_the_ceiling_is_capped() {
        let limit = max_tool_use_concurrency();
        let calls = limit + 3;
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            WideBatchClient {
                call_count: 0,
                calls,
            },
            PeakConcurrencyProbe {
                active: std::sync::Arc::clone(&active),
                peak: std::sync::Arc::clone(&peak),
            },
            PermissionPolicy::new(PermissionMode::Allow),
            SystemPromptBuilder::new().with_os("linux", "6.8").build(),
        );

        let summary = runtime
            .run_turn("read many files", None, None)
            .await
            .expect("turn should succeed");

        assert_eq!(
            summary.tool_results.len(),
            calls,
            "every tool in the batch still produces a result"
        );
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            limit,
            "the batch must be throttled to the ceiling, not run {calls} wide"
        );
    }

    /// Sub-agent spawns run in parallel, as they do in Claude Code
    /// (`AgentTool.isConcurrencySafe() => true`). Named spellings included
    /// because the model chooses the spelling and canonicalization is what
    /// makes the set match.
    #[test]
    fn sub_agent_spawns_are_concurrency_safe() {
        for name in ["agent_spawn", "Agent", "pid_fork"] {
            assert!(
                is_concurrency_safe_tool(name),
                "{name} must batch concurrently: serialising delegations makes \
                 the prompt's \"launch several at once\" advice false"
            );
        }
        // The counter-case: a writer still partitions the run.
        assert!(!is_concurrency_safe_tool("write_file"));
    }

    #[tokio::test]
    async fn concurrency_safe_tools_in_a_batch_execute_concurrently() {
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ConcurrencyProbeClient { call_count: 0 },
            PeakConcurrencyProbe {
                active: std::sync::Arc::clone(&active),
                peak: std::sync::Arc::clone(&peak),
            },
            PermissionPolicy::new(PermissionMode::Allow),
            SystemPromptBuilder::new().with_os("linux", "6.8").build(),
        );

        let summary = runtime
            .run_turn("read three files", None, None)
            .await
            .expect("turn should succeed");

        assert_eq!(
            summary.tool_results.len(),
            3,
            "all three tools produced a result"
        );
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the three read-only tools must overlap (peak concurrency == batch size); \
             a serial loop would peak at 1"
        );
    }

    /// One message emitting read_file, read_file, write_file, read_file — a
    /// mixed batch. The two leading reads are concurrency-safe and must
    /// overlap; the write is not, so it partitions the run and executes serial.
    struct MixedBatchClient {
        call_count: usize,
    }

    #[async_trait]
    impl ApiClient for MixedBatchClient {
        async fn stream(
            &mut self,
            _request: ApiRequest,
        ) -> Result<AssistantEventStream, RuntimeError> {
            self.call_count += 1;
            if self.call_count == 1 {
                let tool = |id: &str, name: &str| AssistantEvent::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: "{\"path\":\"f\"}".to_string(),
                    thought_signature: None,
                };
                Ok(events_to_stream(vec![
                    tool("a", "read_file"),
                    tool("b", "read_file"),
                    tool("c", "write_file"),
                    tool("d", "read_file"),
                    AssistantEvent::MessageStop,
                ]))
            } else {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }
    }

    struct MixedBatchProbe {
        active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        write_peak_active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ToolExecutor for MixedBatchProbe {
        async fn execute(&self, tool_name: &str, _input: &str) -> Result<String, ToolError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            if tool_name == "write_file" {
                self.write_peak_active.fetch_max(now, SeqCst);
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.active.fetch_sub(1, SeqCst);
            Ok("ok".to_string())
        }
    }

    #[tokio::test]
    async fn mixed_batch_partitions_writer_and_serialises_it() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::sync::Arc;
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let write_peak_active = Arc::new(AtomicUsize::new(0));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            MixedBatchClient { call_count: 0 },
            MixedBatchProbe {
                active: Arc::clone(&active),
                peak: Arc::clone(&peak),
                write_peak_active: Arc::clone(&write_peak_active),
            },
            PermissionPolicy::new(PermissionMode::Allow),
            SystemPromptBuilder::new().with_os("linux", "6.8").build(),
        );

        let summary = runtime
            .run_turn("read, write, read", None, None)
            .await
            .expect("turn should succeed");

        assert_eq!(
            summary.tool_results.len(),
            4,
            "all four tools produced a result"
        );
        // The two leading reads form one concurrent batch (peak 2). The write
        // is not concurrency-safe, so the run never grows past 2 — a wrong
        // partition (treating write as safe) would batch all four and peak at 4.
        assert_eq!(
            peak.load(SeqCst),
            2,
            "the writer must partition the run: peak concurrency is the 2 leading reads, not 4"
        );
        // The writer executed alone — no sibling was in flight beside it.
        assert_eq!(
            write_peak_active.load(SeqCst),
            1,
            "the non-concurrency-safe write must run serially (no overlap)"
        );
    }

    #[tokio::test]
    async fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let summary = runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce), None)
            .await
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.prompt_cache_events.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        // turn_usage should aggregate both model request usages
        assert_eq!(summary.turn_usage.input_tokens, 44); // 20 + 24
        assert_eq!(summary.turn_usage.output_tokens, 10); // 6 + 4
        assert_eq!(summary.turn_usage.cache_creation_input_tokens, 2); // 1 + 1
        assert_eq!(summary.turn_usage.cache_read_input_tokens, 5); // 2 + 3
        assert_eq!(summary.turn_usage.total_tokens(), 61);
        // session_usage should equal turn_usage for first turn
        assert_eq!(summary.session_usage, summary.turn_usage);
        assert_eq!(summary.auto_compaction, None);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    /// The prompt-cache invariant: a request may only ever *append* to the
    /// previous one.
    ///
    /// Anthropic's prompt cache matches a byte-exact prefix. Mutating anything
    /// the previous request already sent — a system block, a tool definition,
    /// or an older message — invalidates the cache from that point on, and the
    /// whole remaining context is re-written at 1.25x input price instead of
    /// being read at 0.1x. A pre-request pass that content-cleared stale tool
    /// results used to break exactly this: on a live session it rebuilt ~314k
    /// tokens on 24% of turns, 99% of all cache-creation traffic, to elide a
    /// few KB of stale output.
    ///
    /// This test drives enough tool calls to trip that pass (it kept the two
    /// most recent results per tool name, so the fourth `bash` call cleared the
    /// first) and asserts the prefix is untouched — segment by segment, so a
    /// failure names the message that moved.
    #[tokio::test]
    async fn consecutive_requests_only_append_and_never_rewrite_the_cached_prefix() {
        /// Enough `bash` results that a keep-the-last-2 policy has to clear one.
        const TOOL_CALLS: usize = 5;

        #[derive(Clone)]
        struct Recorded {
            system_static: String,
            system_dynamic: String,
            messages: Vec<crate::session::ConversationMessage>,
        }

        struct RecordingClient {
            seen: Arc<std::sync::Mutex<Vec<Recorded>>>,
        }

        #[async_trait]
        impl ApiClient for RecordingClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                let calls = {
                    let mut seen = self.seen.lock().expect("record requests");
                    seen.push(Recorded {
                        system_static: request.system_prompt.static_text(),
                        system_dynamic: request.system_prompt.dynamic_text(),
                        messages: request.messages.clone(),
                    });
                    seen.len()
                };

                if calls > TOOL_CALLS {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: format!("bash-{calls}"),
                        name: "bash".to_string(),
                        input: String::new(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        struct AllowAll;
        impl PermissionPrompter for AllowAll {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Allow
            }
        }

        let seen = Arc::new(std::sync::Mutex::new(Vec::<Recorded>::new()));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            RecordingClient {
                seen: Arc::clone(&seen),
            },
            // Output big enough that clearing it would be a visible saving —
            // i.e. exactly the case the old pass considered worth rewriting.
            StaticToolExecutor::new().register("bash", |_input| Ok("out ".repeat(400))),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            SystemPrompt::default(),
        );

        runtime
            .run_turn("run bash a few times", Some(&mut AllowAll), None)
            .await
            .expect("conversation loop should succeed");

        let seen = seen.lock().expect("read records").clone();
        assert!(
            seen.len() > TOOL_CALLS,
            "expected the tool loop to issue more than {TOOL_CALLS} requests, got {}",
            seen.len()
        );

        for (index, pair) in seen.windows(2).enumerate() {
            let (before, after) = (&pair[0], &pair[1]);

            assert_eq!(
                before.system_static,
                after.system_static,
                "request {} rewrote the static system block; the cached prefix \
                 starts there, so every later token has to be re-cached",
                index + 1
            );
            assert_eq!(
                before.system_dynamic,
                after.system_dynamic,
                "request {} rewrote the dynamic system block, invalidating the \
                 whole message history behind it",
                index + 1
            );

            assert!(
                after.messages.len() >= before.messages.len(),
                "request {} dropped messages ({} -> {}); a request may only \
                 append to its predecessor",
                index + 1,
                before.messages.len(),
                after.messages.len()
            );

            for (position, old) in before.messages.iter().enumerate() {
                assert_eq!(
                    old,
                    &after.messages[position],
                    "request {} rewrote message[{position}] — the provider has \
                     already cached it, so the entire context from here on is \
                     re-written instead of read",
                    index + 1
                );
            }
        }
    }

    #[tokio::test]
    async fn oversized_tool_output_is_offloaded_and_resume_is_byte_identical() {
        // A tool whose output exceeds the offload threshold. A unique tail
        // sentinel sits past the preview window so we can prove it is spilled
        // to the offload file and never inlined into the transcript.
        const SENTINEL: &str = "UNIQUE_TAIL_SENTINEL_ZZZ";
        let big = format!("{}{SENTINEL}", "A".repeat(60_000));

        struct OneToolThenStop;
        #[async_trait]
        impl ApiClient for OneToolThenStop {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                if request.messages.iter().any(|m| m.role == MessageRole::Tool) {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "big-1".to_string(),
                        name: "big".to_string(),
                        input: String::new(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        struct AllowAll;
        impl PermissionPrompter for AllowAll {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Allow
            }
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let session_file = std::env::temp_dir().join(format!("runtime-offload-e2e-{nanos}.jsonl"));

        let tool_output = big.clone();
        let mut runtime = ConversationRuntime::new(
            Session::new().with_persistence_path(session_file.clone()),
            OneToolThenStop,
            StaticToolExecutor::new().register("big", move |_input| Ok(tool_output.clone())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            SystemPrompt::default(),
        );

        runtime
            .run_turn("please run big", Some(&mut AllowAll), None)
            .await
            .expect("conversation loop should succeed");

        // 1. Full blob spilled to the session's tool-results/<id>, byte-exact.
        let offloaded = runtime
            .session()
            .tool_results_dir()
            .expect("tool-results dir")
            .join("big-1");
        let spilled = fs::read_to_string(&offloaded).expect("offloaded blob should exist");
        assert_eq!(spilled, big);

        // 2. In-transcript tool_result is the replaced preview+marker, NOT the
        //    full blob: it carries the opaque-id marker, keeps only the head,
        //    and drops the tail sentinel.
        let tool_msg = runtime
            .session()
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("tool result message should exist");
        let ContentBlock::ToolResult { output, .. } = &tool_msg.blocks[0] else {
            panic!("expected a tool_result block");
        };
        assert!(output.contains("<persisted-output"));
        assert!(output.contains("id=\"big-1\""));
        assert!(!output.contains(SENTINEL), "tail must not be inlined");
        assert!(output.len() < big.len());
        let inline_output = output.clone();

        // 3. Resume replays byte-identically: the persisted transcript never
        //    held the tail, and reloads to the exact same tool_result content
        //    (so the prompt-cache prefix stays warm across --resume).
        let on_disk = fs::read_to_string(&session_file).expect("transcript should persist");
        assert!(
            !on_disk.contains(SENTINEL),
            "transcript must not hold the tail"
        );

        let restored = Session::load_from_path(&session_file).expect("session should reload");
        let restored_tool = restored
            .messages
            .iter()
            .find(|m| m.role == MessageRole::Tool)
            .expect("reloaded tool result should exist");
        let ContentBlock::ToolResult {
            output: restored_output,
            ..
        } = &restored_tool.blocks[0]
        else {
            panic!("expected a reloaded tool_result block");
        };
        assert_eq!(*restored_output, inline_output);

        let _ = fs::remove_file(&offloaded);
        let _ = fs::remove_dir(offloaded.parent().unwrap());
        let _ = fs::remove_file(&session_file);
    }

    #[tokio::test]
    async fn records_runtime_session_trace_events() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-runtime", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            SystemPrompt::default(),
        )
        .with_session_tracer(tracer);

        runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce), None)
            .await
            .expect("conversation loop should succeed");

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(trace_names.contains(&"turn_started"));
        assert!(trace_names.contains(&"assistant_iteration_completed"));
        assert!(trace_names.contains(&"tool_execution_started"));
        assert!(trace_names.contains(&"tool_execution_finished"));
        assert!(trace_names.contains(&"turn_completed"));
    }

    #[tokio::test]
    async fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        #[async_trait]
        impl ApiClient for SingleCallApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("I could not use the tool.".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            SystemPrompt::default(),
        );

        let summary = runtime
            .run_turn("use the tool", Some(&mut RejectPrompter), None)
            .await
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[tokio::test]
    async fn empty_post_tool_completion_ends_turn_without_visible_message() {
        struct EmptyAfterToolApiClient {
            call_count: usize,
        }

        #[async_trait]
        impl ApiClient for EmptyAfterToolApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "write".to_string(),
                            input: "file".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ])),
                    2 => {
                        let last_message = request
                            .messages
                            .last()
                            .expect("tool result should be present");
                        assert_eq!(last_message.role, MessageRole::Tool);
                        Ok(events_to_stream(vec![
                            AssistantEvent::Usage(TokenUsage {
                                input_tokens: 12,
                                output_tokens: 0,
                                cache_creation_input_tokens: 0,
                                cache_read_input_tokens: 0,
                                ..TokenUsage::default()
                            }),
                            AssistantEvent::MessageStop,
                        ]))
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            EmptyAfterToolApiClient { call_count: 0 },
            StaticToolExecutor::new().register("write", |_input| Ok("created".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let summary = runtime
            .run_turn("create the file", None, None)
            .await
            .expect("empty post-tool completion should be treated as success");

        assert_eq!(summary.assistant_messages.len(), 1);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.turn_usage.output_tokens, 0);
        assert_eq!(summary.session_usage.output_tokens, 0);
        assert_eq!(runtime.session().messages.len(), 3);
    }

    #[tokio::test]
    async fn empty_post_tool_completion_retries_when_requested_deliverable_is_missing() {
        struct EmptyThenContinueApiClient {
            call_count: usize,
        }

        #[async_trait]
        impl ApiClient for EmptyThenContinueApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "write_file".to_string(),
                            input: "{}".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ])),
                    2 => Ok(events_to_stream(vec![AssistantEvent::MessageStop])),
                    3 => {
                        assert!(request.messages.iter().any(|message| {
                            message.role == MessageRole::User
                                && message.blocks.iter().any(|block| {
                                    matches!(
                                        block,
                                        ContentBlock::Text { text }
                                            if text.contains("previous model response was empty")
                                    )
                                })
                        }));
                        Ok(events_to_stream(vec![
                            AssistantEvent::ToolUse {
                                id: "tool-2".to_string(),
                                name: "bash".to_string(),
                                input: r#"{"command":"python3 .drafts/generate_pdf.py"}"#
                                    .to_string(),
                                thought_signature: None,
                            },
                            AssistantEvent::MessageStop,
                        ]))
                    }
                    4 => Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("Generated report.pdf.".to_string()),
                        AssistantEvent::MessageStop,
                    ])),
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            EmptyThenContinueApiClient { call_count: 0 },
            StaticToolExecutor::new()
                .register("write_file", |_input| {
                    Ok(r#"{"filePath":".drafts/generate_pdf.py"}"#.to_string())
                })
                .register("bash", |_input| {
                    Ok(r#"{"stdout":"created report.pdf","filePath":"report.pdf"}"#.to_string())
                }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let summary = runtime
            .run_turn("生成一个 PDF 文件", None, None)
            .await
            .expect("missing deliverable should get one continuation retry");

        assert_eq!(summary.iterations, 4);
        assert_eq!(summary.tool_results.len(), 2);
        assert!(runtime.session().messages.iter().any(|message| {
            message.role == MessageRole::User
                && message.blocks.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text }
                            if text.contains("previous model response was empty")
                    )
                })
        }));
    }

    #[tokio::test]
    async fn repeated_empty_post_tool_completion_fails_when_deliverable_is_missing() {
        struct AlwaysEmptyAfterToolApiClient {
            call_count: usize,
        }

        #[async_trait]
        impl ApiClient for AlwaysEmptyAfterToolApiClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "write_file".to_string(),
                            input: "{}".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ])),
                    2 | 3 => Ok(events_to_stream(vec![AssistantEvent::MessageStop])),
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            AlwaysEmptyAfterToolApiClient { call_count: 0 },
            StaticToolExecutor::new().register("write_file", |_input| {
                Ok(r#"{"filePath":".drafts/generate_pdf.py"}"#.to_string())
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let error = runtime
            .run_turn("生成一个 PDF 文件", None, None)
            .await
            .expect_err("second empty response should fail while deliverable is missing");

        assert!(error
            .to_string()
            .contains("before producing the requested file deliverable"));
    }

    #[tokio::test]
    async fn denies_tool_use_when_pre_tool_hook_blocks() {
        struct SingleCallApiClient;
        #[async_trait]
        impl ApiClient for SingleCallApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook denies")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'blocked by hook'; exit 2")],
                Vec::new(),
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use the tool", None, None)
            .await
            .expect("conversation should continue after hook denial");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook denial should produce an error result: {output}"
        );
        assert!(
            output.contains("denied tool") || output.contains("blocked by hook"),
            "unexpected hook denial output: {output:?}"
        );
    }

    #[tokio::test]
    async fn denies_tool_use_when_pre_tool_hook_fails() {
        struct SingleCallApiClient;
        #[async_trait]
        impl ApiClient for SingleCallApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook fails")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'broken hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            )),
        );

        // when
        let summary = runtime
            .run_turn("use the tool", None, None)
            .await
            .expect("conversation should continue after hook failure");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook failure should produce an error result: {output}"
        );
        assert!(
            output.contains("exited with status 1") || output.contains("broken hook"),
            "unexpected hook failure output: {output:?}"
        );
    }

    #[tokio::test]
    async fn appends_post_tool_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        #[async_trait]
        impl ApiClient for TwoCallApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: r#"{"lhs":2,"rhs":2}"#.to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ])),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(events_to_stream(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ]))
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'pre hook ran'")],
                vec![shell_snippet("printf 'post hook ran'")],
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use add", None, None)
            .await
            .expect("tool loop succeeds");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "post hook should preserve non-error result: {output:?}"
        );
        assert!(
            output.contains('4'),
            "tool output missing value: {output:?}"
        );
        assert!(
            output.contains("pre hook ran"),
            "tool output missing pre hook feedback: {output:?}"
        );
        assert!(
            output.contains("post hook ran"),
            "tool output missing post hook feedback: {output:?}"
        );
    }

    #[tokio::test]
    async fn appends_post_tool_use_failure_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        #[async_trait]
        impl ApiClient for TwoCallApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "fail".to_string(),
                            input: r#"{"path":"README.md"}"#.to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ])),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(events_to_stream(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ]))
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("fail", |_input| Err(ToolError::new("tool exploded"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                Vec::new(),
                vec![shell_snippet("printf 'post hook should not run'")],
                vec![shell_snippet("printf 'failure hook ran'")],
            )),
        );

        // when
        let summary = runtime
            .run_turn("use fail", None, None)
            .await
            .expect("tool loop succeeds");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "failure hook path should preserve error result: {output:?}"
        );
        assert!(
            output.contains("tool exploded"),
            "tool output missing failure reason: {output:?}"
        );
        assert!(
            output.contains("failure hook ran"),
            "tool output missing failure hook feedback: {output:?}"
        );
        assert!(
            !output.contains("post hook should not run"),
            "normal post hook should not run on tool failure: {output:?}"
        );
    }

    #[tokio::test]
    async fn runtime_observer_receives_text_delta_tool_use_and_tool_result_in_order() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        let mut observer = RecordingRuntimeObserver::default();

        runtime
            .run_turn("what is 2 + 2?", None, Some(&mut observer))
            .await
            .expect("conversation loop should succeed");

        assert_eq!(
            observer.events,
            vec![
                ObservedRuntimeEvent::TextDelta("Let me calculate that.".to_string()),
                ObservedRuntimeEvent::ToolUse {
                    id: "tool-1".to_string(),
                    name: "add".to_string(),
                    input: "2,2".to_string(),
                },
                ObservedRuntimeEvent::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    tool_name: "add".to_string(),
                    output: "4".to_string(),
                    is_error: false,
                },
                ObservedRuntimeEvent::TextDelta("The answer is 4.".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn runtime_observer_receives_thinking_delta() {
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ThinkingApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        let mut observer = RecordingRuntimeObserver::default();

        runtime
            .run_turn("think briefly", None, Some(&mut observer))
            .await
            .expect("conversation loop should succeed");

        assert_eq!(
            observer.events,
            vec![
                ObservedRuntimeEvent::ThinkingDelta("considering the request".to_string()),
                ObservedRuntimeEvent::TextDelta("done".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn thinking_signature_is_assembled_from_its_deltas_and_never_rendered() {
        /// The real stream shape: thinking text in several deltas, then the
        /// signature in its own delta(s), which the provider layer carries as
        /// text-less `Thinking` events. Both halves have to survive — a block
        /// whose signature is truncated looks signed and is rejected, and a
        /// block with no signature at all is dropped before the wire, which is
        /// what made every tool round-trip rebuild the whole cached prefix.
        struct SignedThinkingApiClient;

        #[async_trait]
        impl ApiClient for SignedThinkingApiClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::Thinking {
                        thinking: "first half ".to_string(),
                        signature: None,
                    },
                    AssistantEvent::Thinking {
                        thinking: "second half".to_string(),
                        signature: None,
                    },
                    AssistantEvent::Thinking {
                        thinking: String::new(),
                        signature: Some("sig-part-1".to_string()),
                    },
                    AssistantEvent::Thinking {
                        thinking: String::new(),
                        signature: Some("-sig-part-2".to_string()),
                    },
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SignedThinkingApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        let mut observer = RecordingRuntimeObserver::default();

        runtime
            .run_turn("think briefly", None, Some(&mut observer))
            .await
            .expect("conversation loop should succeed");

        match &runtime.session().messages[1].blocks[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "first half second half");
                assert_eq!(signature.as_deref(), Some("sig-part-1-sig-part-2"));
            }
            other => panic!("expected a signed thinking block, got {other:?}"),
        }

        // A signature-only event carries no text, so it must not reach a
        // renderer as an empty thinking delta.
        assert_eq!(
            observer.events,
            vec![
                ObservedRuntimeEvent::ThinkingDelta("first half ".to_string()),
                ObservedRuntimeEvent::ThinkingDelta("second half".to_string()),
                ObservedRuntimeEvent::TextDelta("done".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn redacted_thinking_is_announced_to_the_renderer_and_still_replayed() {
        /// The provider encrypts a thinking block when the turn trips a safety
        /// classifier. There is no plaintext for anyone to print, but printing
        /// nothing is indistinguishable from a turn that never thought — and the
        /// export already labels it, so the live view has to as well.
        struct RedactedThinkingApiClient;

        #[async_trait]
        impl ApiClient for RedactedThinkingApiClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::RedactedThinking {
                        data: "\"ciphertext\"".to_string(),
                    },
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            RedactedThinkingApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        let mut observer = RecordingRuntimeObserver::default();

        runtime
            .run_turn("think briefly", None, Some(&mut observer))
            .await
            .expect("conversation loop should succeed");

        assert_eq!(
            observer.events,
            vec![
                ObservedRuntimeEvent::ThinkingDelta(
                    "[thinking: redacted by the provider]\n".to_string()
                ),
                ObservedRuntimeEvent::TextDelta("done".to_string()),
            ]
        );
        // The note exists for the renderer only. What goes back on the wire is
        // the ciphertext the server sent — that is what keeps the prefix cached.
        assert_eq!(
            runtime.session().messages[1].blocks[0],
            ContentBlock::RedactedThinking {
                data: "\"ciphertext\"".to_string()
            },
        );
    }

    #[tokio::test]
    async fn cache_safe_compaction_asks_for_thinking_exactly_when_the_stream_does() {
        use super::{TextCompletion, TextCompletionOptions};
        use crate::session::ConversationMessage;
        use std::collections::BTreeSet;

        /// `thinking` is part of Anthropic's cache key, so the compaction built
        /// to reuse the turn stream's prefix has to ask for thinking exactly
        /// when the stream does — omitting it re-writes the whole prefix and
        /// still returns 200, so nothing would fail loudly. The standard
        /// compaction builds its own prefix and should not pay for thinking it
        /// cannot reuse.
        #[derive(Default)]
        struct OptionRecorder {
            seen: Vec<TextCompletionOptions>,
        }

        #[async_trait]
        impl ApiClient for OptionRecorder {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("not used"))
            }

            fn thinking_enabled(&self) -> bool {
                true
            }

            async fn complete_text(
                &mut self,
                _request: ApiRequest,
                options: TextCompletionOptions,
            ) -> Result<TextCompletion, RuntimeError> {
                self.seen.push(options);
                Ok(TextCompletion {
                    text: "<summary>a checkpoint</summary>".to_string(),
                    usage: None,
                    stop_reason: Some("end_turn".to_string()),
                    has_tool_calls: false,
                })
            }
        }

        let mut client = OptionRecorder::default();
        client
            .send_cache_safe_compaction(
                ApiRequest {
                    system_prompt: SystemPrompt::default(),
                    messages: vec![ConversationMessage::user_text("history")],
                    trace_id: None,
                    pre_compact_discovered_tools: BTreeSet::default(),
                },
                "summarize",
                12_000,
            )
            .await
            .expect("cache-safe compaction should succeed");
        client
            .send_compaction(
                "claude-sonnet-4-6",
                "system",
                vec![ConversationMessage::user_text("history")],
                12_000,
            )
            .await
            .expect("standard compaction should succeed");

        assert_eq!(
            client
                .seen
                .iter()
                .map(|options| options.thinking_enabled)
                .collect::<Vec<_>>(),
            vec![true, false],
        );
        // The other two fields are the rest of what makes the prefix match;
        // losing either costs the same read as losing `thinking`.
        assert!(client.seen[0].include_tools);
        assert!(client.seen[0].cache_prefix);
    }

    #[test]
    fn a_signature_with_no_thinking_block_creates_nothing() {
        // Defensive: a stream that somehow signs nothing must not produce an
        // empty-but-signed block, which the API will not take back.
        let mut blocks = Vec::new();
        push_thinking_block(&mut blocks, String::new(), Some("orphan".to_string()));
        assert!(blocks.is_empty(), "unexpected blocks: {blocks:?}");
    }

    #[tokio::test]
    async fn abort_during_tool_execution_cancels_turn_and_synthesizes_remaining_results() {
        use futures::StreamExt;

        struct TwoToolUseApiClient {
            calls: usize,
            emitted: Arc<tokio::sync::Notify>,
        }

        #[async_trait]
        impl ApiClient for TwoToolUseApiClient {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.calls += 1;
                assert_eq!(
                    self.calls, 1,
                    "cancelled turn must not make a follow-up API call"
                );
                let emitted = Arc::clone(&self.emitted);
                Ok(Box::pin(
                    events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "slow".to_string(),
                            input: "{}".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::ToolUse {
                            id: "tool-2".to_string(),
                            name: "later".to_string(),
                            input: "{}".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ])
                    .map(move |event| {
                        if matches!(event, Ok(AssistantEvent::MessageStop)) {
                            emitted.notify_one();
                        }
                        event
                    }),
                ))
            }
        }

        struct CancelExecutor {
            abort: crate::HookAbortSignal,
            emitted: Arc<tokio::sync::Notify>,
        }
        impl ToolExecutor for CancelExecutor {
            async fn execute(&self, name: &str, _input: &str) -> Result<String, ToolError> {
                assert_eq!(name, "slow", "remaining tool should be synthesized");
                // Both ids have arrived before cancellation. With streaming,
                // tool execution can otherwise predate the second block.
                self.emitted.notified().await;
                self.abort.abort();
                Ok("partial output".into())
            }
        }
        let abort_signal = crate::HookAbortSignal::new();
        let emitted = Arc::new(tokio::sync::Notify::new());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TwoToolUseApiClient {
                calls: 0,
                emitted: Arc::clone(&emitted),
            },
            CancelExecutor {
                abort: abort_signal.clone(),
                emitted,
            },
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_hook_abort_signal(abort_signal);

        let summary = runtime
            .run_turn("use tools", None, None)
            .await
            .expect("cancelled tool turn should resolve cleanly");

        assert!(summary.cancelled);
        assert_eq!(summary.tool_results.len(), 2);
        let ContentBlock::ToolResult {
            output, is_error, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected first tool result");
        };
        assert!(*is_error);
        assert_eq!(output, "partial output");
        let ContentBlock::ToolResult {
            output, is_error, ..
        } = &summary.tool_results[1].blocks[0]
        else {
            panic!("expected synthesized tool result");
        };
        assert!(*is_error);
        assert!(output.contains("Interrupted"));
    }

    #[tokio::test]
    async fn runtime_observer_receives_denied_tool_result() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct ToolUseApiClient;
        #[async_trait]
        impl ApiClient for ToolUseApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ToolUseApiClient,
            StaticToolExecutor::new()
                .register("blocked", |_input| panic!("denied tool should not execute")),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            SystemPrompt::default(),
        );
        let mut observer = RecordingRuntimeObserver::default();

        runtime
            .run_turn(
                "use the tool",
                Some(&mut RejectPrompter),
                Some(&mut observer),
            )
            .await
            .expect("conversation should continue after denied tool");

        assert!(observer.events.contains(&ObservedRuntimeEvent::ToolResult {
            tool_use_id: "tool-1".to_string(),
            tool_name: "blocked".to_string(),
            output: "not now".to_string(),
            is_error: true,
        }));
    }

    #[tokio::test]
    async fn runtime_observer_receives_error_tool_result() {
        struct ToolUseApiClient;
        #[async_trait]
        impl ApiClient for ToolUseApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]));
                }
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "fail".to_string(),
                        input: "{}".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ToolUseApiClient,
            StaticToolExecutor::new().register("fail", |_input| Err(ToolError::new("boom"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        let mut observer = RecordingRuntimeObserver::default();

        runtime
            .run_turn("use the tool", None, Some(&mut observer))
            .await
            .expect("conversation should continue after tool error");

        assert!(observer.events.contains(&ObservedRuntimeEvent::ToolResult {
            tool_use_id: "tool-1".to_string(),
            tool_name: "fail".to_string(),
            output: "boom".to_string(),
            is_error: true,
        }));
    }

    #[tokio::test]
    async fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                    ..TokenUsage::default()
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    #[tokio::test]
    async fn compacts_session_after_turns() {
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        runtime.run_turn("a", None, None).await.expect("turn a");
        runtime.run_turn("b", None, None).await.expect("turn b");
        runtime.run_turn("c", None, None).await.expect("turn c");

        let result = runtime.compact_sync(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(
            result.compacted_session.session_id,
            runtime.session().session_id
        );
        assert!(result.compacted_session.compaction.is_some());
    }

    #[tokio::test]
    async fn persists_conversation_turn_messages_to_jsonl_session() {
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let path = temp_session_path("persisted-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        runtime
            .run_turn("persist this turn", None, None)
            .await
            .expect("turn should succeed");

        let restored = Session::load_from_path(&path).expect("persisted session should reload");
        fs::remove_file(&path).expect("temp session file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].role, MessageRole::User);
        assert_eq!(restored.messages[1].role, MessageRole::Assistant);
        assert_eq!(restored.session_id, runtime.session().session_id);
    }

    #[tokio::test]
    async fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");

        let runtime = ConversationRuntime::new(
            session.clone(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let forked = runtime.fork_session(Some("alt-path".to_string()));

        assert_eq!(forked.messages, session.messages);
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(
            forked
                .fork
                .as_ref()
                .map(|fork| (fork.parent_session_id.as_str(), fork.branch_name.as_deref())),
            Some((session.session_id.as_str(), Some("alt-path")))
        );
        assert!(runtime.session().fork.is_none());
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-conversation-{label}-{nanos}.json"))
    }

    /// Same cross-platform translator as `hooks::tests::shell_snippet`.
    /// See that copy for the full rationale. Duplicated here (rather than
    /// extracted into a shared `test_util`) because `#[cfg(test)] mod tests`
    /// in each file is the canonical place a test-only helper lives in
    /// this codebase, and the function is ~50 lines — small enough that a
    /// second copy is cheaper than a new module surface.
    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        use regex::Regex;
        let mut s = script.to_string();

        let printf_format = Regex::new(r"printf\s+'%s'\s+'([^']*)'(\s*>&2)?").unwrap();
        s = printf_format
            .replace_all(&s, |caps: &regex::Captures<'_>| {
                let msg = &caps[1];
                if caps.get(2).is_some() {
                    format!("echo {msg}1>&2")
                } else {
                    format!("echo {msg}")
                }
            })
            .into_owned();

        let printf_simple = Regex::new(r"printf\s+'([^']*)'(\s*>&2)?").unwrap();
        s = printf_simple
            .replace_all(&s, |caps: &regex::Captures<'_>| {
                let msg = &caps[1];
                if caps.get(2).is_some() {
                    format!("echo {msg}1>&2")
                } else {
                    format!("echo {msg}")
                }
            })
            .into_owned();

        let exit_chain = Regex::new(r";\s*exit\s+(\d+)").unwrap();
        s = exit_chain.replace_all(&s, " && exit /b $1").into_owned();

        let other_chain = Regex::new(r";\s*").unwrap();
        s = other_chain.replace_all(&s, " & ").into_owned();

        let sleep_re = Regex::new(r"\bsleep\s+(\d+)").unwrap();
        s = sleep_re
            .replace_all(&s, "timeout /t $1 /nobreak >NUL")
            .into_owned();

        s
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[tokio::test]
    async fn failed_post_turn_compaction_fails_turn_and_keeps_history() {
        let _g = env_guard();
        // Env-var override sets threshold to 100 — test-only, independent of SSOT.
        // Mock reports a 200-token context on the latest response → crosses
        // 100 → triggers auto-compact.
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "100");
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 200,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        ..TokenUsage::default()
                    }),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let error = runtime
            .run_turn("trigger", None, None)
            .await
            .expect_err("compaction failure must fail the turn");

        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");

        let message = error.to_string();
        assert!(
            message.contains(crate::compact::COMPACTION_FAILED),
            "{message}"
        );
        // The turn-level error is what the TUI prints verbatim: one marker and
        // one "history preserved", not a stack of nested wrappers.
        assert_eq!(
            message.matches(crate::compact::COMPACTION_FAILED).count(),
            1
        );
        assert_eq!(message.matches("history preserved").count(), 1, "{message}");
        assert_eq!(runtime.session().messages.len(), 6);
        assert_eq!(runtime.session().messages[0].role, MessageRole::User);
    }

    #[tokio::test]
    async fn skips_auto_compaction_below_threshold() {
        let _g = env_guard();
        // Env-var override sets threshold to 1000. Mock reports 50 → below → no compact.
        std::env::set_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS", "1000");
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 50,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        ..TokenUsage::default()
                    }),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let summary = runtime
            .run_turn("trigger", None, None)
            .await
            .expect("turn should succeed");

        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");

        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[tokio::test]
    async fn auto_compact_threshold_uses_per_model_ssot() {
        let _g = env_guard();
        // Ensure no env-var override is active.
        std::env::remove_var("CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS");
        // The threshold is context_window - request max_tokens - buffer.
        // For claude-sonnet-4-6 (200K context, 64K request max_tokens):
        //   buffer = 13K (200K < 400K), threshold = 200K - 64K - 13K = 123K.
        // The provider rejects this model at 200K - 64K = 136K, so the
        // threshold has to sit below that to ever fire.
        let threshold = auto_compact_threshold_for_model("claude-sonnet-4-6");
        assert_eq!(threshold, 123_000);

        // Unknown model: SSOT default window (1M) and the 64K request
        // heuristic — buffer = 50K (1M >= 800K), threshold = 886K.
        let unknown = auto_compact_threshold_for_model("some-unknown-model");
        assert_eq!(unknown, 886_000);
    }

    // Circuit-breaker for consecutive auto-compact no-ops (PR #249) is
    // genuinely untestable from the public interface — the no-op
    // condition requires compact_session to return removed=0 on three
    // CONSECUTIVE turns, but the public run_turn() always appends 2
    // messages per turn, and with the hardcoded
    // CompactionConfig::default().preserve_recent_messages=4, the
    // session naturally grows past the preserve floor on turn 3 (6
    // messages → keep_from=2 → removed=2 → counter resets).
    //
    // Possible test paths considered + rejected:
    //   1. Expose a builder for preserve_recent_messages on
    //      ConversationRuntime → public API surface bloat just for
    //      testing.
    //   2. Make consecutive_auto_compact_noops pub or add a getter →
    //      same problem (test-only surface).
    //   3. Add a #[cfg(test)] backdoor → ok but couples the test to
    //      implementation detail rather than behaviour.
    //   4. PTY-test with a mock ApiClient that controls everything →
    //      same as (1)/(2) at a different layer.
    //
    // Honest decision: the breaker's 5-line implementation is locally
    // obvious (loop-counter + early-return); the value is structural
    // (signal-floor for telemetry / ACP session events) more than
    // user-visible; the cost of testability surfaces would exceed the
    // value. Leaving uncovered with this comment block as the
    // contract.

    #[tokio::test]
    async fn compaction_health_probe_blocks_turn_when_tool_executor_is_broken() {
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                panic!("API should not run when health probe fails");
            }
        }

        let mut session = Session::new();
        session.record_compaction("summarized earlier work", 4);
        session
            .push_user_text("previous message")
            .expect("message should append");

        let tool_executor = StaticToolExecutor::new().register("glob_search", |_input| {
            Err(ToolError::new("transport unavailable"))
        });
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let error = runtime
            .run_turn("trigger", None, None)
            .await
            .expect_err("health probe failure should abort the turn");
        assert!(
            error
                .to_string()
                .contains("Session health probe failed after compaction"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("transport unavailable"),
            "expected underlying probe error: {error}"
        );
    }

    #[tokio::test]
    async fn compaction_health_probe_rejects_empty_compacted_session() {
        struct SimpleApi;
        #[async_trait]
        impl ApiClient for SimpleApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        let mut session = Session::new();
        session.record_compaction("fresh summary", 2);

        let tool_executor = StaticToolExecutor::new().register("glob_search", |_input| {
            Err(ToolError::new(
                "glob_search should not run for an empty compacted session",
            ))
        });
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let error = runtime
            .run_turn("trigger", None, None)
            .await
            .expect_err("empty compacted session must not continue");
        assert!(error.to_string().contains("no active history"));
        assert!(runtime.session().messages.is_empty());
    }

    #[tokio::test]
    async fn build_assistant_message_requires_message_stop_event() {
        // given
        let events = vec![AssistantEvent::TextDelta("hello".to_string())];

        // when
        let error = build_assistant_message(events)
            .expect_err("assistant messages should require a stop event");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream ended without a message stop event"));
    }

    #[tokio::test]
    async fn build_assistant_message_requires_content() {
        // given
        let events = vec![AssistantEvent::MessageStop];

        // when
        let error =
            build_assistant_message(events).expect_err("assistant messages should require content");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
    }

    #[tokio::test]
    async fn static_tool_executor_rejects_unknown_tools() {
        // given
        let mut executor = StaticToolExecutor::new();

        // when
        let error = executor
            .execute("missing", "{}")
            .await
            .expect_err("unregistered tools should fail");

        // then
        assert_eq!(error.to_string(), "unknown tool: missing");
    }

    #[tokio::test]
    async fn run_turn_errors_when_max_iterations_is_exceeded() {
        struct LoopingApi;

        #[async_trait]
        impl ApiClient for LoopingApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Ok(events_to_stream(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo".to_string(),
                        input: "payload".to_string(),
                        thought_signature: None,
                    },
                    AssistantEvent::MessageStop,
                ]))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_max_iterations(1);

        // when
        let error = runtime
            .run_turn("loop", None, None)
            .await
            .expect_err("conversation loop should stop after the configured limit");

        // then
        assert!(error
            .to_string()
            .contains("conversation loop exceeded the maximum number of iterations"));
    }

    /// A turn that grows past the window mid-flight must be compacted
    /// *before* the request is dispatched. Reproduces the production failure:
    /// the agent takes many tool-call steps in one turn, the history outgrows
    /// the window, and the only salvage is a single reactive compaction after
    /// the provider has already rejected — so the second overflow kills the
    /// turn.
    #[tokio::test]
    async fn turn_compacts_proactively_instead_of_waiting_for_a_rejection() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Leave room for a complete checkpoint plus the protected tool pairs
        // while still forcing this long turn across the request budget.
        const LIMIT: usize = 100_000;
        const STEPS: usize = 110;

        struct BudgetedApi {
            rejections: Arc<AtomicUsize>,
            steps: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ApiClient for BudgetedApi {
            fn context_budget(
                &self,
                _model: &str,
                _system_prompt: &SystemPrompt,
            ) -> crate::compact::ContextBudget {
                crate::compact::ContextBudget {
                    context_limit: LIMIT,
                    max_output_tokens: 200,
                    overhead_tokens: 100,
                    buffer_tokens: 300,
                }
            }

            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                let session_tokens: usize = request
                    .messages
                    .iter()
                    .map(crate::compact::estimate_message_tokens)
                    .sum();
                if session_tokens + 100 + 200 > LIMIT {
                    self.rejections.fetch_add(1, Ordering::Relaxed);
                    return Err(RuntimeError::context_window_blocked(
                        "prompt is too long for this model".to_string(),
                    ));
                }
                let step = self.steps.fetch_add(1, Ordering::Relaxed);
                if step < STEPS {
                    Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: format!("tool-{step}"),
                            name: "bulk".to_string(),
                            input: "go".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ]))
                } else {
                    Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]))
                }
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<crate::session::ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                Ok("<summary>Steps so far were bulk tool calls.</summary>".to_string())
            }
        }

        let rejections = Arc::new(AtomicUsize::new(0));
        let steps = Arc::new(AtomicUsize::new(0));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            BudgetedApi {
                rejections: Arc::clone(&rejections),
                steps: Arc::clone(&steps),
            },
            // Keep individual outputs below the pruning threshold. The full
            // turn crosses the budget while leaving room for the summary send.
            StaticToolExecutor::new().register("bulk", |_| Ok("x".repeat(4_000))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_max_iterations(128);

        let summary = runtime
            .run_turn("do many steps", None, None)
            .await
            .expect("a turn that outgrows the window mid-flight should compact and finish");

        assert!(
            summary.iterations > STEPS,
            "expected the turn to run all its steps, got {}",
            summary.iterations
        );
        assert_eq!(
            rejections.load(Ordering::Relaxed),
            0,
            "the runtime should compact before dispatching, never letting the provider reject"
        );
    }

    /// Recorded `session_compacted` traces that actually removed something,
    /// in the order they were emitted.
    fn effective_compaction_traces(
        sink: &MemoryTelemetrySink,
    ) -> Vec<serde_json::Map<String, serde_json::Value>> {
        sink.events()
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) if trace.name == "session_compacted" => {
                    Some(trace.attributes.clone())
                }
                _ => None,
            })
            .filter(|attrs| {
                attrs
                    .get("removed_messages")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    > 0
            })
            .collect()
    }

    /// Every compaction that runs must leave a trace. The two paths that
    /// fire during a real turn used to record nothing at all — only the
    /// engine host's preflight did — so a session log showed no compaction
    /// even when one had run and worked, which is exactly what makes a
    /// context-overflow report undiagnosable.
    #[tokio::test]
    async fn in_turn_compaction_is_recorded_with_its_trigger_and_summary_source() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const LIMIT: usize = 100_000;

        struct TinyBudgetApi {
            steps: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ApiClient for TinyBudgetApi {
            fn context_budget(
                &self,
                _model: &str,
                _system_prompt: &SystemPrompt,
            ) -> crate::compact::ContextBudget {
                crate::compact::ContextBudget {
                    context_limit: LIMIT,
                    max_output_tokens: 200,
                    overhead_tokens: 100,
                    buffer_tokens: 300,
                }
            }

            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                let step = self.steps.fetch_add(1, Ordering::Relaxed);
                if step < 110 {
                    Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: format!("tool-{step}"),
                            name: "bulk".to_string(),
                            input: "go".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ]))
                } else {
                    Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]))
                }
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<crate::session::ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                Ok("<summary>Bulk steps.</summary>".to_string())
            }
        }

        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-compaction-trace", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TinyBudgetApi {
                steps: Arc::new(AtomicUsize::new(0)),
            },
            StaticToolExecutor::new().register("bulk", |_| Ok("x".repeat(4_000))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_max_iterations(128)
        .with_session_tracer(tracer);

        runtime
            .run_turn("do many steps", None, None)
            .await
            .expect("turn should finish");

        let compactions = effective_compaction_traces(&sink);
        let attrs = compactions
            .first()
            .expect("an in-turn compaction that removed messages must be recorded");
        assert_eq!(
            attrs.get("trigger").and_then(serde_json::Value::as_str),
            Some("in_turn_budget")
        );
        assert_eq!(
            attrs
                .get("summary_source")
                .and_then(serde_json::Value::as_str),
            Some("llm"),
            "the log must distinguish an LLM summary from the local structural fallback"
        );
        let before = attrs
            .get("estimated_tokens_before")
            .and_then(serde_json::Value::as_u64)
            .expect("before estimate");
        let after = attrs
            .get("estimated_tokens_after")
            .and_then(serde_json::Value::as_u64)
            .expect("after estimate");
        assert!(
            after <= before / 2,
            "compaction should meet the complete-history target"
        );
    }

    /// Reproduces the reported production failure, using only surface that
    /// predates this change so it can be run against `main` unmodified.
    ///
    /// The salvage compaction was capped at once per turn. A turn that takes
    /// many tool-call steps grows its own history while it runs, so the first
    /// rejection was recovered, the history grew again, and the second
    /// rejection ended the turn with `retried_context_window_overflow`
    /// already spent. A long turn must be able to compact as often as it
    /// needs and still terminate.
    ///
    /// The client here does not override `context_budget`, so the proactive
    /// guard cannot see the artificially small limit and the turn is carried
    /// entirely by the salvage path — which is what isolates the allowance.
    #[tokio::test]
    async fn a_long_turn_survives_repeated_context_window_rejections() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Small enough that a handful of tool results overflow it, so one
        /// compaction cannot carry the whole turn.
        const LIMIT: usize = 4_000;
        const STEPS: usize = 20;

        struct GrindingApi {
            steps: Arc<AtomicUsize>,
            rejections: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl ApiClient for GrindingApi {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                let session_tokens: usize = request
                    .messages
                    .iter()
                    .map(crate::compact::estimate_message_tokens)
                    .sum();
                if session_tokens + 300 > LIMIT {
                    self.rejections.fetch_add(1, Ordering::Relaxed);
                    return Err(RuntimeError::context_window_blocked(
                        "input length and `max_tokens` exceed context limit".to_string(),
                    ));
                }
                let step = self.steps.fetch_add(1, Ordering::Relaxed);
                if step < STEPS {
                    Ok(events_to_stream(vec![
                        AssistantEvent::ToolUse {
                            id: format!("tool-{step}"),
                            name: "bulk".to_string(),
                            input: "go".to_string(),
                            thought_signature: None,
                        },
                        AssistantEvent::MessageStop,
                    ]))
                } else {
                    Ok(events_to_stream(vec![
                        AssistantEvent::TextDelta("done".to_string()),
                        AssistantEvent::MessageStop,
                    ]))
                }
            }

            async fn send_compaction(
                &mut self,
                _model: &str,
                _system_prompt: &str,
                _messages: Vec<crate::session::ConversationMessage>,
                _max_tokens: u32,
            ) -> Result<String, RuntimeError> {
                Ok("<summary>Bulk steps so far.</summary>".to_string())
            }
        }

        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-many-compactions", sink.clone());
        let rejections = Arc::new(AtomicUsize::new(0));
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            GrindingApi {
                steps: Arc::new(AtomicUsize::new(0)),
                rejections: Arc::clone(&rejections),
            },
            StaticToolExecutor::new().register("bulk", |_| Ok("x".repeat(2_000))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_max_iterations(128)
        .with_session_tracer(tracer);

        let summary = runtime
            .run_turn("grind", None, None)
            .await
            .expect("a long turn should compact as often as it needs and finish");
        assert!(summary.iterations > STEPS);
        assert!(
            rejections.load(Ordering::Relaxed) > 1,
            "the scenario is only a regression test if more than one rejection happened, saw {}",
            rejections.load(Ordering::Relaxed)
        );

        let compactions = effective_compaction_traces(&sink).len();
        assert!(
            compactions > 1,
            "a turn this long needs more than one compaction, saw {compactions}"
        );
        assert!(
            compactions <= super::MAX_TURN_COMPACTIONS,
            "compaction must stay bounded, saw {compactions}"
        );
    }

    #[tokio::test]
    async fn conversation_runtime_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<ConversationRuntime<ScriptedApiClient, StaticToolExecutor>>();
    }

    #[tokio::test]
    async fn run_turn_propagates_api_errors() {
        struct FailingApi;

        #[async_trait]
        impl ApiClient for FailingApi {
            async fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        // when
        let error = runtime
            .run_turn("hello", None, None)
            .await
            .expect_err("API failures should propagate");

        // then
        assert_eq!(error.to_string(), "upstream failed");
    }

    /// Captures the [`ApiRequest`] sent on each turn so tests can assert on
    /// the messages and system prompt that the model would actually see.
    #[derive(Default)]
    struct CapturingApi {
        requests: Arc<std::sync::Mutex<Vec<ApiRequest>>>,
    }

    #[async_trait]
    impl ApiClient for CapturingApi {
        async fn stream(
            &mut self,
            request: ApiRequest,
        ) -> Result<AssistantEventStream, RuntimeError> {
            self.requests
                .lock()
                .expect("requests mutex should not be poisoned")
                .push(request);
            Ok(events_to_stream(vec![
                AssistantEvent::TextDelta("done".to_string()),
                AssistantEvent::MessageStop,
            ]))
        }
    }

    #[tokio::test]
    async fn first_turn_announces_current_date_before_user_content() {
        // The system prompt carries no date, so the first turn must announce
        // it via a user-side system-reminder block — PREPENDED, so harness
        // text never trails what the user typed. Appended, it extended the
        // referent of "send exactly this text" and the model copied the
        // reminder into the tool argument (sudocode#623).
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date("2026-05-15");
        runtime.today_override = Some("2026-05-15".to_string());

        runtime
            .run_turn("hello", None, None)
            .await
            .expect("turn should succeed");

        let requests = captured.lock().expect("captured mutex");
        let user_blocks = &requests[0].messages[0].blocks;
        assert_eq!(
            user_blocks.len(),
            2,
            "first turn should carry date announcement + user text"
        );
        let ContentBlock::Text { text: announcement } = &user_blocks[0] else {
            panic!("first block should be the date announcement");
        };
        assert!(
            matches!(&user_blocks[1], ContentBlock::Text { text } if text == "hello"),
            "the user's own text must come LAST, so nothing the harness wrote \
             trails it: {user_blocks:?}"
        );
        assert!(
            announcement.contains("<system-reminder>")
                && announcement.contains("Today's date is 2026-05-15"),
            "date announcement malformed: {announcement}"
        );
    }

    #[tokio::test]
    async fn date_announcement_is_not_repeated_on_later_turns() {
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date("2026-05-15");
        runtime.today_override = Some("2026-05-15".to_string());

        runtime.run_turn("first", None, None).await.expect("turn 1");
        runtime
            .run_turn("second", None, None)
            .await
            .expect("turn 2");

        let requests = captured.lock().expect("captured mutex");
        let second_turn_user_blocks = requests[1]
            .messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .expect("user message")
            .blocks
            .clone();
        assert_eq!(
            second_turn_user_blocks.len(),
            1,
            "date announcement must not repeat while the session carries it"
        );
        assert!(matches!(
            &second_turn_user_blocks[0],
            ContentBlock::Text { text } if text == "second"
        ));
    }

    #[tokio::test]
    async fn first_turn_announces_active_model_before_user_content() {
        // The system prompt carries no model, so the first turn must announce
        // it via a user-side system-reminder block PREPENDED to the user's
        // content (mirrors the date announcement, same reason: sudocode#623).
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_model("claude-opus-5");

        runtime
            .run_turn("hello", None, None)
            .await
            .expect("turn should succeed");

        let requests = captured.lock().expect("captured mutex");
        let user_blocks = &requests[0].messages[0].blocks;
        assert_eq!(
            user_blocks.len(),
            2,
            "first turn should carry model announcement + user text"
        );
        let ContentBlock::Text { text: announcement } = &user_blocks[0] else {
            panic!("first block should be the model announcement");
        };
        assert!(
            matches!(&user_blocks[1], ContentBlock::Text { text } if text == "hello"),
            "the user's own text must come LAST, so nothing the harness wrote \
             trails it: {user_blocks:?}"
        );
        assert!(
            announcement.contains("<system-reminder>")
                && announcement.contains("You are running as claude-opus-5"),
            "model announcement malformed: {announcement}"
        );
    }

    #[tokio::test]
    async fn model_announcement_is_not_repeated_on_later_turns() {
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_model("claude-opus-5");

        runtime.run_turn("first", None, None).await.expect("turn 1");
        runtime
            .run_turn("second", None, None)
            .await
            .expect("turn 2");

        let requests = captured.lock().expect("captured mutex");
        let second_turn_user_blocks = requests[1]
            .messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .expect("user message")
            .blocks
            .clone();
        assert_eq!(
            second_turn_user_blocks.len(),
            1,
            "model announcement must not repeat while the session carries it"
        );
    }

    #[tokio::test]
    async fn model_change_is_announced_mid_session() {
        // Switching the requested model mid-session (as `/model` does by
        // rebuilding the runtime with a new session.model) must inject a
        // change reminder naming the new model, prepended BEFORE the user
        // content.
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_model("claude-opus-5");

        runtime.run_turn("first", None, None).await.expect("turn 1");
        runtime.session_mut().model = Some("claude-sonnet-4-6".to_string());
        runtime
            .run_turn("second", None, None)
            .await
            .expect("turn 2");

        let requests = captured.lock().expect("captured mutex");
        let second_turn_user_blocks = requests[1]
            .messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .expect("user message")
            .blocks
            .clone();
        let ContentBlock::Text { text: reminder } = &second_turn_user_blocks[0] else {
            panic!("first block of the switched turn should be the change reminder");
        };
        assert!(
            reminder.contains("The active model has changed")
                && reminder.contains("claude-sonnet-4-6"),
            "model change reminder malformed: {reminder}"
        );
    }

    #[tokio::test]
    async fn date_announcement_reinjected_when_session_lost_it() {
        // Compaction can drop the message that carried the announcement; the
        // runtime re-injects by scanning the session rather than tracking a
        // one-shot flag. Simulated here with a pre-seeded session that has
        // messages but no date announcement.
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut session = Session::new();
        session
            .push_user_blocks(vec![ContentBlock::Text {
                text: "summary of earlier work".to_string(),
            }])
            .expect("seed message");
        let mut runtime = ConversationRuntime::new(
            session,
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date("2026-05-15");
        runtime.today_override = Some("2026-05-15".to_string());

        runtime
            .run_turn("hello", None, None)
            .await
            .expect("turn should succeed");

        let requests = captured.lock().expect("captured mutex");
        let user_blocks = &requests[0]
            .messages
            .iter()
            .rfind(|m| m.role == MessageRole::User)
            .expect("user message")
            .blocks
            .clone();
        assert_eq!(user_blocks.len(), 2, "announcement should be re-injected");
        assert!(
            matches!(
                &user_blocks[0],
                ContentBlock::Text { text } if text.contains("Today's date is 2026-05-15")
            ),
            "re-injection lands in FRONT, like the first-turn announcement — \
             harness text never trails the user's own: {user_blocks:?}"
        );
    }

    #[tokio::test]
    async fn injects_date_change_reminder_when_local_date_rolls_over() {
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date("2026-05-15");
        runtime.today_override = Some("2026-05-16".to_string());

        runtime
            .run_turn("hello", None, None)
            .await
            .expect("turn should succeed");

        let requests = captured.lock().expect("captured mutex");
        let user_blocks = &requests[0].messages[0].blocks;
        assert_eq!(
            user_blocks.len(),
            2,
            "rollover should prepend exactly one reminder block"
        );
        let ContentBlock::Text { text: reminder } = &user_blocks[0] else {
            panic!("first block should be the reminder text block");
        };
        assert!(
            reminder.contains("<system-reminder>"),
            "reminder should be wrapped in a system-reminder tag, got {reminder}"
        );
        assert!(
            reminder.contains("2026-05-15") && reminder.contains("2026-05-16"),
            "reminder should mention old and new date, got {reminder}"
        );
        assert!(matches!(
            &user_blocks[1],
            ContentBlock::Text { text } if text == "hello"
        ));
    }

    #[tokio::test]
    async fn date_change_reminder_fires_only_once_per_rollover() {
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date("2026-05-15");
        runtime.today_override = Some("2026-05-16".to_string());

        runtime
            .run_turn("first", None, None)
            .await
            .expect("first turn");
        runtime
            .run_turn("second", None, None)
            .await
            .expect("second turn");

        let requests = captured.lock().expect("captured mutex");
        // first turn carries the reminder + user text
        assert_eq!(requests[0].messages[0].blocks.len(), 2);
        // second turn (still on 2026-05-16) carries only the user text;
        // the runtime's known date was advanced after firing the reminder.
        let second_turn_user_blocks = requests[1]
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .last()
            .expect("user message")
            .blocks
            .clone();
        assert_eq!(
            second_turn_user_blocks.len(),
            1,
            "reminder must not repeat after rollover acknowledged"
        );
        assert!(matches!(
            &second_turn_user_blocks[0],
            ContentBlock::Text { text } if text == "second"
        ));
    }

    #[tokio::test]
    async fn prompt_known_date_advances_after_rollover_and_can_be_carried_over() {
        // Models the CLI rebuild path: a fresh runtime inherits the previous
        // runtime's `prompt_known_date()` so the rollover reminder fires
        // exactly once per actual date change, even when the runtime is
        // reconstructed every turn (see issue #135).
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut first = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date("2026-05-15");
        first.today_override = Some("2026-05-19".to_string());

        first
            .run_turn("first", None, None)
            .await
            .expect("first turn");
        // Reminder fires and the runtime advances its known date to today.
        assert_eq!(first.prompt_known_date(), Some("2026-05-19"));

        let carried = first
            .prompt_known_date()
            .expect("known date should be set")
            .to_string();

        // Simulate `prepare_turn_runtime` rebuilding the runtime for the next
        // turn while inheriting the advanced known date from the previous
        // runtime.
        let mut second = ConversationRuntime::new(
            first.session().clone(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        )
        .with_session_known_date(carried);
        second.today_override = Some("2026-05-19".to_string());

        second
            .run_turn("second", None, None)
            .await
            .expect("second turn");

        let requests = captured.lock().expect("captured mutex");
        let second_turn_user_blocks = requests[1]
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .last()
            .expect("user message")
            .blocks
            .clone();
        assert_eq!(
            second_turn_user_blocks.len(),
            1,
            "carrying over the advanced known date must suppress a duplicate reminder"
        );
        assert!(matches!(
            &second_turn_user_blocks[0],
            ContentBlock::Text { text } if text == "second"
        ));
    }

    #[tokio::test]
    async fn no_reminder_when_session_known_date_unset() {
        let captured: Arc<std::sync::Mutex<Vec<ApiRequest>>> = Arc::default();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            CapturingApi {
                requests: captured.clone(),
            },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );
        runtime.today_override = Some("2099-12-31".to_string());

        runtime
            .run_turn("hello", None, None)
            .await
            .expect("turn should succeed");

        let requests = captured.lock().expect("captured mutex");
        assert_eq!(
            requests[0].messages[0].blocks.len(),
            1,
            "no reminder should fire without a known date"
        );
    }

    /// Test that turn_usage correctly aggregates usage across multiple model requests
    /// within a single turn (e.g., tool_use followed by final response).
    #[tokio::test]
    async fn aggregates_turn_usage_across_multiple_model_requests() {
        // This client simulates: tool_use -> tool_result -> final text
        // with different usages for each model request
        struct MultiRequestApiClient {
            call_count: usize,
        }

        #[async_trait]
        impl ApiClient for MultiRequestApiClient {
            async fn stream(
                &mut self,
                request: ApiRequest,
            ) -> Result<AssistantEventStream, RuntimeError> {
                self.call_count += 1;
                match self.call_count {
                    1 => {
                        // First request: tool_use with usage_a
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::User));
                        Ok(events_to_stream(vec![
                            AssistantEvent::ToolUse {
                                id: "tool-1".to_string(),
                                name: "test_tool".to_string(),
                                input: "{}".to_string(),
                                thought_signature: None,
                            },
                            AssistantEvent::Usage(TokenUsage {
                                input_tokens: 100,
                                output_tokens: 50,
                                cache_creation_input_tokens: 10,
                                cache_read_input_tokens: 20,
                                cost_units: Some(1_000),
                                cost_currency: Some(UsageCostCurrency::SudoPoint),
                            }),
                            AssistantEvent::MessageStop,
                        ]))
                    }
                    2 => {
                        // Second request: final text with usage_b
                        let last_message = request
                            .messages
                            .last()
                            .expect("tool result should be present");
                        assert_eq!(last_message.role, MessageRole::Tool);
                        Ok(events_to_stream(vec![
                            AssistantEvent::TextDelta("Done!".to_string()),
                            AssistantEvent::Usage(TokenUsage {
                                input_tokens: 200,
                                output_tokens: 30,
                                cache_creation_input_tokens: 5,
                                cache_read_input_tokens: 15,
                                cost_units: Some(2_000),
                                cost_currency: Some(UsageCostCurrency::SudoPoint),
                            }),
                            AssistantEvent::MessageStop,
                        ]))
                    }
                    _ => unreachable!("unexpected extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            MultiRequestApiClient { call_count: 0 },
            StaticToolExecutor::new().register("test_tool", |_input| Ok("result".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            SystemPrompt::default(),
        );

        let summary = runtime
            .run_turn("test", None, None)
            .await
            .expect("turn should succeed");

        // Verify turn_usage aggregates both requests
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(
            summary.turn_usage.input_tokens, 300,
            "input_tokens should be 100 + 200"
        );
        assert_eq!(
            summary.turn_usage.output_tokens, 80,
            "output_tokens should be 50 + 30"
        );
        assert_eq!(
            summary.turn_usage.cache_creation_input_tokens, 15,
            "cache_creation should be 10 + 5"
        );
        assert_eq!(
            summary.turn_usage.cache_read_input_tokens, 35,
            "cache_read should be 20 + 15"
        );
        assert_eq!(summary.turn_usage.total_tokens(), 430);
        assert_eq!(summary.turn_usage.cost_units, Some(3_000));
        assert_eq!(
            summary.turn_usage.cost_currency,
            Some(UsageCostCurrency::SudoPoint)
        );

        // For first turn, session_usage should equal turn_usage
        assert_eq!(
            summary.session_usage, summary.turn_usage,
            "first turn: session_usage should equal turn_usage"
        );

        // Verify runtime.usage().cumulative_usage() matches session_usage
        assert_eq!(
            runtime.usage().cumulative_usage(),
            summary.session_usage,
            "runtime cumulative usage should match session_usage"
        );
    }

    #[test]
    fn turn_usage_omits_cost_when_any_usage_lacks_cost() {
        let messages = vec![
            crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "first".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: 10,
                    cache_read_input_tokens: 20,
                    cost_units: Some(1_000),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ),
            crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "second".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 200,
                    output_tokens: 30,
                    cache_creation_input_tokens: 5,
                    cache_read_input_tokens: 15,
                    ..TokenUsage::default()
                }),
            ),
        ];

        let usage = super::sum_assistant_message_usage(&messages);

        assert_eq!(usage.total_tokens(), 430);
        assert_eq!(usage.cost_units, None);
        assert_eq!(usage.cost_currency, None);
    }

    #[test]
    fn turn_usage_preserves_zero_cost() {
        let messages = vec![crate::session::ConversationMessage::assistant_with_usage(
            vec![ContentBlock::Text {
                text: "free".to_string(),
            }],
            Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 10,
                cache_read_input_tokens: 20,
                cost_units: Some(0),
                cost_currency: Some(UsageCostCurrency::SudoPoint),
            }),
        )];

        let usage = super::sum_assistant_message_usage(&messages);

        assert_eq!(usage.total_tokens(), 180);
        assert_eq!(usage.cost_units, Some(0));
        assert_eq!(usage.cost_currency, Some(UsageCostCurrency::SudoPoint));
    }
}
