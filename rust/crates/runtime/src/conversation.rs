use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::pin::Pin;

use async_trait::async_trait;
use futures::stream::Stream;
use futures::StreamExt;
use serde_json::{Map, Value};
use telemetry::SessionTracer;

use crate::compact::{
    compact_session, estimate_session_tokens, CompactionConfig, CompactionResult,
};
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::prompt::SystemPrompt;
use crate::session::{ContentBlock, ConversationMessage, Session};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";

/// Message used in synthetic tool results when a turn is interrupted.
const INTERRUPT_MESSAGE: &str = "Interrupted · What should Sudo Code do instead?";
const EMPTY_POST_TOOL_DELIVERABLE_REMINDER: &str = "\
<system-reminder>
The previous model response was empty after a tool completed. The user requested a file deliverable, but the current turn has not produced a matching final file yet. Continue the same task now: create or execute whatever is needed to produce the requested file, then verify it exists before ending the turn.
</system-reminder>";

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: SystemPrompt,
    pub messages: Vec<ConversationMessage>,
    /// Optional trace ID for end-to-end request tracking.
    /// Passed through to the HTTP layer as X-Request-ID header.
    pub trace_id: Option<String>,
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantEvent {
    Thinking {
        thinking: String,
        signature: Option<String>,
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
    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError>;
}

/// Optional observer for runtime events emitted while processing a turn.
pub trait RuntimeObserver {
    fn on_text_delta(&mut self, _delta: &str) {}

    fn on_tool_use(&mut self, _id: &str, _name: &str, _input: &str) {}

    fn on_tool_result(
        &mut self,
        _tool_use_id: &str,
        _tool_name: &str,
        _output: &str,
        _is_error: bool,
    ) {
    }
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
pub trait ToolExecutor: Send {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;

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
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: SystemPrompt,
    /// Date (`YYYY-MM-DD`) baked into the cacheable system prompt at session
    /// start. When `Some`, [`ConversationRuntime::run_turn_with_blocks`]
    /// compares it against today's local date at turn time and prepends a
    /// `<system-reminder>` content block when the date has rolled over,
    /// instead of mutating the system prompt itself (which would invalidate
    /// the prompt-cache prefix).
    prompt_known_date: Option<String>,
    /// Override for "today" used in tests. Always `None` outside tests.
    #[cfg(test)]
    today_override: Option<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter + Send>>,
    session_tracer: Option<SessionTracer>,
    /// File operation tracker for the current turn.
    file_tracker: crate::file_tracker::TurnFileTracker,
    /// Current turn ID for file tracking.
    current_turn_id: Option<String>,
    /// User request intent for the current turn.
    user_request_intent: Option<crate::file_intent::UserRequestIntent>,
    /// Trace ID for the current request (passed from ACP _meta.traceId).
    trace_id: Option<String>,
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
            tool_executor,
            permission_policy,
            system_prompt,
            prompt_known_date: None,
            #[cfg(test)]
            today_override: None,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
            file_tracker: crate::file_tracker::TurnFileTracker::new(workspace_root),
            current_turn_id: None,
            user_request_intent: None,
            trace_id: None,
        }
    }

    /// Records the date (`YYYY-MM-DD`) that was frozen into the cacheable
    /// system prompt at session start. Each turn compares this against the
    /// local date and emits a `<system-reminder>` content block when the
    /// date has rolled over, leaving the system prompt itself untouched so
    /// the prompt cache prefix stays warm.
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

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.tool_executor
            .set_abort_signal(hook_abort_signal.clone());
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_hook_progress_reporter(
        mut self,
        hook_progress_reporter: Box<dyn HookProgressReporter + Send>,
    ) -> Self {
        self.hook_progress_reporter = Some(hook_progress_reporter);
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

    fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
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

    /// If the session-start date frozen into the cacheable system prompt no
    /// longer matches today's local date, prepend a `<system-reminder>`
    /// content block so the assistant learns about the rollover without
    /// invalidating the prompt-cache prefix. The known date is then advanced
    /// so the reminder fires only once per rollover.
    fn inject_date_change_reminder(&mut self, blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
        let Some(known) = self.prompt_known_date.clone() else {
            return blocks;
        };
        let today = self.current_local_date();
        if today == known {
            return blocks;
        }
        let reminder = ContentBlock::Text {
            text: format!(
                "<system-reminder>The local calendar date has changed since this session started. \
                 The system prompt was cached on {known}; today is now {today}. \
                 Treat {today} as the current date for any reasoning that depends on it.</system-reminder>"
            ),
        };
        self.prompt_known_date = Some(today);
        let mut combined = Vec::with_capacity(blocks.len() + 1);
        combined.push(reminder);
        combined.extend(blocks);
        combined
    }

    /// Run a session health probe to verify the runtime is functional after compaction.
    /// Returns Ok(()) if healthy, Err if the session appears broken.
    fn run_session_health_probe(&mut self) -> Result<(), String> {
        // Check if we have basic session integrity
        if self.session.messages.is_empty() && self.session.compaction.is_some() {
            // Freshly compacted with no messages - this is normal
            return Ok(());
        }

        // Verify tool executor is responsive with a non-destructive probe
        // Using glob_search with a pattern that won't match anything
        let probe_input = r#"{"pattern": "*.health-check-probe-"}"#;
        match self.tool_executor.execute("glob_search", probe_input) {
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
                AssistantEvent::Thinking {
                    thinking,
                    signature,
                } => {
                    flush_text_block(&mut text, &mut blocks);
                    blocks.push(ContentBlock::Thinking {
                        thinking,
                        signature,
                    });
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
                AssistantEvent::Usage(_)
                | AssistantEvent::PromptCache(_)
                | AssistantEvent::MessageStop => {}
            }
        }
        flush_text_block(&mut text, &mut blocks);

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
                tool_name,
                INTERRUPT_MESSAGE,
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

    fn push_tool_result_message(
        &mut self,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        iterations: usize,
        tool_results: &mut Vec<ConversationMessage>,
        result_message: ConversationMessage,
    ) -> Result<(), RuntimeError> {
        notify_tool_result(runtime_observer_mut(observer), &result_message);
        self.session
            .push_message(result_message.clone())
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        self.record_tool_finished(iterations, &result_message);
        tool_results.push(result_message);
        Ok(())
    }

    fn push_interrupted_tool_results(
        &mut self,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        iterations: usize,
        tool_results: &mut Vec<ConversationMessage>,
        pending_tool_uses: &[(String, String, String)],
        start_index: usize,
    ) -> Result<(), RuntimeError> {
        for (tool_use_id, tool_name, _) in &pending_tool_uses[start_index..] {
            let result_message = ConversationMessage::tool_result(
                tool_use_id.clone(),
                tool_name.clone(),
                INTERRUPT_MESSAGE,
                true,
            );
            self.push_tool_result_message(observer, iterations, tool_results, result_message)?;
        }
        Ok(())
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
    pub async fn run_turn_with_blocks(
        &mut self,
        blocks: Vec<ContentBlock>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
        mut observer: Option<&mut dyn RuntimeObserver>,
    ) -> Result<TurnSummary, RuntimeError> {
        let blocks = self.inject_date_change_reminder(blocks);
        let label = blocks
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();

        if self.session.compaction.is_some() {
            if let Err(error) = self.run_session_health_probe() {
                return Err(RuntimeError::new(format!(
                    "Session health probe failed after compaction: {error}. \
                     The session may be in an inconsistent state. \
                     Consider starting a fresh session with /session new."
                )));
            }
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
        self.session
            .push_user_blocks(blocks)
            .map_err(|error| RuntimeError::new(error.to_string()))?;

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut retried_empty_post_tool_deliverable = false;

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

            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
                trace_id: self.trace_id.clone(),
            };
            let mut stream = match self.api_client.stream(request).await {
                Ok(stream) => stream,
                Err(error) => {
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            };

            // Consume the stream event-by-event, racing against the abort
            // signal so cancellation drops the HTTP connection immediately.
            let events = {
                let abort = &self.hook_abort_signal;
                let mut collected = Vec::new();
                loop {
                    tokio::select! {
                        biased;
                        () = abort.cancelled() => {
                            // Drop the stream to close the HTTP connection
                            // and stop token consumption.
                            drop(stream);
                            self.finalize_cancelled_turn(collected);
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
                            });
                        }
                        next = stream.next() => {
                            match next {
                                Some(Ok(event)) => {
                                    // Notify the observer in real time as events
                                    // arrive from the API stream so ACP clients
                                    // receive incremental updates.
                                    if let Some(obs) = observer.as_mut() {
                                        match &event {
                                            AssistantEvent::TextDelta(delta) => {
                                                obs.on_text_delta(delta);
                                            }
                                            AssistantEvent::ToolUse { id, name, input, .. } => {
                                                obs.on_tool_use(id, name, input);
                                            }
                                            _ => {}
                                        }
                                    }
                                    collected.push(event);
                                }
                                Some(Err(error)) => {
                                    self.record_turn_failed(iterations, &error);
                                    return Err(error);
                                }
                                None => break,
                            }
                        }
                    }
                }
                collected
            };

            let (mut assistant_message, usage, turn_prompt_cache_events) =
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
                        let auto_compaction = self.maybe_auto_compact();
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
                        };
                        self.record_turn_completed(&summary);
                        return Ok(summary);
                    }
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            assistant_message.model.clone_from(&self.session.model);
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

            self.session
                .push_message(assistant_message.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            assistant_messages.push(assistant_message);

            if pending_tool_uses.is_empty() {
                break;
            }

            for tool_index in 0..pending_tool_uses.len() {
                if self.hook_abort_signal.is_aborted() {
                    self.push_interrupted_tool_results(
                        &mut observer,
                        iterations,
                        &mut tool_results,
                        &pending_tool_uses,
                        tool_index,
                    )?;
                    return Ok(self.cancelled_summary(
                        assistant_messages,
                        tool_results,
                        prompt_cache_events,
                        iterations,
                    ));
                }

                let (tool_use_id, tool_name, input) = pending_tool_uses[tool_index].clone();
                let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
                if self.hook_abort_signal.is_aborted() {
                    self.push_interrupted_tool_results(
                        &mut observer,
                        iterations,
                        &mut tool_results,
                        &pending_tool_uses,
                        tool_index,
                    )?;
                    return Ok(self.cancelled_summary(
                        assistant_messages,
                        tool_results,
                        prompt_cache_events,
                        iterations,
                    ));
                }

                let effective_input = pre_hook_result
                    .updated_input()
                    .map_or_else(|| input.clone(), ToOwned::to_owned);
                let permission_context = PermissionContext::new(
                    pre_hook_result.permission_override(),
                    pre_hook_result.permission_reason().map(ToOwned::to_owned),
                );

                let permission_outcome = if pre_hook_result.is_cancelled() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook cancelled tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_failed() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook failed for tool `{tool_name}`"),
                        ),
                    }
                } else if pre_hook_result.is_denied() {
                    PermissionOutcome::Deny {
                        reason: format_hook_message(
                            &pre_hook_result,
                            &format!("PreToolUse hook denied tool `{tool_name}`"),
                        ),
                    }
                } else if let Some(prompt) = prompter.as_mut() {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        Some(*prompt),
                    )
                } else {
                    self.permission_policy.authorize_with_context(
                        &tool_name,
                        &effective_input,
                        &permission_context,
                        None,
                    )
                };

                let result_message = match permission_outcome {
                    PermissionOutcome::Allow => {
                        self.record_tool_started(iterations, &tool_name);
                        let (mut output, mut is_error) =
                            match self.tool_executor.execute(&tool_name, &effective_input) {
                                Ok(output) => (output, false),
                                Err(error) => (error.to_string(), true),
                            };
                        if self.hook_abort_signal.is_aborted() {
                            output = merge_hook_feedback(pre_hook_result.messages(), output, true);
                            let result_message = ConversationMessage::tool_result(
                                tool_use_id,
                                tool_name,
                                output,
                                true,
                            );
                            self.push_tool_result_message(
                                &mut observer,
                                iterations,
                                &mut tool_results,
                                result_message,
                            )?;
                            self.push_interrupted_tool_results(
                                &mut observer,
                                iterations,
                                &mut tool_results,
                                &pending_tool_uses,
                                tool_index + 1,
                            )?;
                            return Ok(self.cancelled_summary(
                                assistant_messages,
                                tool_results,
                                prompt_cache_events,
                                iterations,
                            ));
                        }
                        output = merge_hook_feedback(pre_hook_result.messages(), output, false);

                        let post_hook_result = if is_error {
                            self.run_post_tool_use_failure_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                            )
                        } else {
                            self.run_post_tool_use_hook(
                                &tool_name,
                                &effective_input,
                                &output,
                                false,
                            )
                        };
                        if post_hook_result.is_denied()
                            || post_hook_result.is_failed()
                            || post_hook_result.is_cancelled()
                        {
                            is_error = true;
                        }
                        output = merge_hook_feedback(
                            post_hook_result.messages(),
                            output,
                            post_hook_result.is_denied()
                                || post_hook_result.is_failed()
                                || post_hook_result.is_cancelled(),
                        );

                        ConversationMessage::tool_result(tool_use_id, tool_name, output, is_error)
                    }
                    PermissionOutcome::Deny { reason } => ConversationMessage::tool_result(
                        tool_use_id,
                        tool_name,
                        merge_hook_feedback(pre_hook_result.messages(), reason, true),
                        true,
                    ),
                };
                self.push_tool_result_message(
                    &mut observer,
                    iterations,
                    &mut tool_results,
                    result_message,
                )?;
            }
        }

        let auto_compaction = self.maybe_auto_compact();

        self.finish_current_turn_tracking();

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
        };
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    #[must_use]
    pub fn compact(&self, config: CompactionConfig) -> CompactionResult {
        compact_session(&self.session, config)
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
        &mut self.tool_executor
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
            self.file_tracker.cleanup_turn_drafts(&turn_id)
        } else {
            Vec::new()
        }
    }

    /// Rollback all file operations for the current turn (call on abort).
    /// Returns error messages for failed operations.
    pub fn rollback_current_turn(&mut self) -> Vec<String> {
        if let Some(turn_id) = self.current_turn_id.clone() {
            self.file_tracker.rollback_turn(&turn_id)
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

    fn maybe_auto_compact(&mut self) -> Option<AutoCompactionEvent> {
        if self.usage_tracker.cumulative_usage().input_tokens
            < self.auto_compaction_input_tokens_threshold
        {
            return None;
        }

        let result = compact_session(
            &self.session,
            CompactionConfig {
                max_estimated_tokens: 0,
                ..CompactionConfig::default()
            },
        );

        if result.removed_message_count == 0 {
            return None;
        }

        self.session = result.compacted_session;
        Some(AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
        })
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

/// Reads the automatic compaction threshold from the environment.
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    parse_auto_compaction_threshold(
        std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

#[must_use]
fn parse_auto_compaction_threshold(value: Option<&str>) -> u32 {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|threshold| *threshold > 0)
        .unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut finished = false;
    let mut usage = None;

    for event in events {
        match event {
            AssistantEvent::Thinking {
                thinking,
                signature,
            } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
                });
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

    flush_text_block(&mut text, &mut blocks);

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
    ))
}

/// Sums the token usage from all assistant messages in a turn.
/// Each assistant message carries the usage from one model request.
fn sum_assistant_message_usage(messages: &[ConversationMessage]) -> TokenUsage {
    let mut total = TokenUsage::default();
    for message in messages {
        if let Some(usage) = message.usage {
            total.add_assign_usage(usage);
        }
    }
    total
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
    matches!(
        name,
        "write_file" | "edit_file" | "bash" | "REPL" | "PowerShell" | "NotebookEdit"
    )
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

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError> + Send>;

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
        handler: impl FnMut(&str) -> Result<String, ToolError> + Send + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get_mut(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_assistant_message, parse_auto_compaction_threshold, ApiClient, ApiRequest,
        AssistantEvent, AssistantEventStream, AutoCompactionEvent, ConversationRuntime,
        PromptCacheEvent, RuntimeError, RuntimeObserver, StaticToolExecutor, ToolExecutor,
        DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPrompt, SystemPromptBuilder};
    use crate::session::{ContentBlock, MessageRole, Session};
    use crate::usage::TokenUsage;
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
    async fn abort_during_tool_execution_cancels_turn_and_synthesizes_remaining_results() {
        struct TwoToolUseApiClient {
            calls: usize,
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
                Ok(events_to_stream(vec![
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
                ]))
            }
        }

        let abort_signal = crate::HookAbortSignal::new();
        let abort_from_tool = abort_signal.clone();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            TwoToolUseApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("slow", move |_input| {
                    abort_from_tool.abort();
                    Ok("partial output".to_string())
                })
                .register("later", |_input| {
                    panic!("remaining tool should be synthesized")
                }),
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

        let result = runtime.compact(CompactionConfig {
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

    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        script.replace('\'', "\"")
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[tokio::test]
    async fn auto_compacts_when_cumulative_input_threshold_is_crossed() {
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
                        input_tokens: 120_000,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
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
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None, None)
            .await
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 2,
            })
        );
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[tokio::test]
    async fn skips_auto_compaction_below_threshold() {
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
                        input_tokens: 99_999,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
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
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None, None)
            .await
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[tokio::test]
    async fn auto_compaction_threshold_defaults_and_parses_values() {
        assert_eq!(
            parse_auto_compaction_threshold(None),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(parse_auto_compaction_threshold(Some("4321")), 4321);
        assert_eq!(
            parse_auto_compaction_threshold(Some("0")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(
            parse_auto_compaction_threshold(Some("not-a-number")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

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
    async fn compaction_health_probe_skips_empty_compacted_session() {
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

        let summary = runtime
            .run_turn("trigger", None, None)
            .await
            .expect("empty compacted session should not fail health probe");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
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
    async fn skips_date_change_reminder_when_known_date_unchanged() {
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
        assert_eq!(user_blocks.len(), 1, "no reminder should be prepended");
        assert!(matches!(
            &user_blocks[0],
            ContentBlock::Text { text } if text == "hello"
        ));
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
            summary.turn_usage.input_tokens,
            300,
            "input_tokens should be 100 + 200"
        );
        assert_eq!(
            summary.turn_usage.output_tokens,
            80,
            "output_tokens should be 50 + 30"
        );
        assert_eq!(
            summary.turn_usage.cache_creation_input_tokens,
            15,
            "cache_creation should be 10 + 5"
        );
        assert_eq!(
            summary.turn_usage.cache_read_input_tokens,
            35,
            "cache_read should be 20 + 15"
        );
        assert_eq!(summary.turn_usage.total_tokens(), 430);

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
}
