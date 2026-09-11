//! The pure (non-rendering) provider client the engine uses.
//!
//! This is the wire→[`AssistantEvent`] half of the old CLI `AnthropicRuntimeClient`
//! with **all rendering removed** (no markdown/ANSI, no spinner, no terminal
//! writes, no progress reporter, no friendly-error formatting). Every renderer
//! shares this identical core; the display half lives above the seam.
//!
//! It stays **incremental** (a `try_unfold` stream that yields each event as it
//! arrives) — unlike `tools::stream_with_provider`, which collects the whole
//! response before returning and drops thinking deltas (fine for subagents, but
//! it would lose live token streaming and the "Reasoning…" cue for a human
//! renderer). It keeps the post-tool stall timeout + non-streaming fallback +
//! prompt-cache extraction, since those change *which events* are produced (core
//! behavior), not how they look.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use api::{
    AuthMode, CacheHints, ContentBlockDelta, InputMessage, MessageRequest, MessageResponse,
    MessageStream, OutputContentBlock, PromptCache, PromptCacheRecord, ProviderClient,
    ResolvedProvider, StreamEvent, SudoCodeConfig, ToolChoice,
};
use async_trait::async_trait;
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, ConversationMessage, MessageRole,
    PromptCacheEvent, RuntimeError,
};
use telemetry::{SessionTracer, SudoclawLogSink};
use tools::GlobalToolRegistry;

/// Post-tool-completion stall deadline: if the model does not respond within
/// this window after a tool result, the stalled connection is dropped and the
/// request re-sent once as a continuation nudge. Matches the CLI value.
const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);

const POST_TOOL_FINAL_SYNTHESIS_PROMPT: &str = "The previous tool execution is complete. Send a final ordinary assistant message to the user in the user's language. Do not call tools. Only describe actions explicitly confirmed by the tool results. Only list files whose paths are explicitly present in the tool results. If the tool result only created a draft/helper script, say that the final deliverables have not been generated yet and identify the draft script path.";

/// The engine's provider client. Produces an incremental
/// [`AssistantEventStream`]; renders nothing.
pub struct EngineApiClient {
    client: ProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    allowed_tools: Option<BTreeSet<String>>,
    tool_registry: GlobalToolRegistry,
    reasoning_effort: Option<String>,
    thinking_enabled: bool,
    /// The account this client bills to, for error messages. The gateway rejects
    /// an unroutable model in terms of its own routing groups, which the user
    /// never configured; naming the account turns that into an actionable edit.
    account: Option<String>,
}

impl EngineApiClient {
    /// Build a client for `model`, resolving the provider from config + auth
    /// mode (identical resolution to the old CLI client, minus the render
    /// plumbing).
    pub fn new(
        session_id: &str,
        sudocode_config: &SudoCodeConfig,
        model: &str,
        auth_mode: AuthMode,
        tool_registry: GlobalToolRegistry,
        enable_tools: bool,
        allowed_tools: Option<BTreeSet<String>>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let resolved: ResolvedProvider =
            api::resolve_provider_from_config(model, Some(auth_mode), sudocode_config)?;
        let mut client = ProviderClient::from_resolved(&resolved, Some(auth_mode))?
            .with_prompt_cache(PromptCache::new(session_id));
        let sink = Arc::new(SudoclawLogSink::new()?);
        client = client.with_session_tracer(SessionTracer::new(session_id, sink));

        // Same selector `doctor` reports through, so an error names the account
        // the report shows. Best-effort: a client that cannot name its account
        // still works, it just explains less.
        let account = api::proxy_account_for_model(sudocode_config, model)
            .ok()
            .map(|selected| selected.name.to_string());

        Ok(Self {
            client,
            session_id: session_id.to_string(),
            model: resolved.model_id.clone(),
            enable_tools,
            allowed_tools,
            tool_registry,
            reasoning_effort: None,
            thinking_enabled: true,
            account,
        })
    }

    /// Render a provider failure for a human, adding the account when the
    /// gateway's complaint is really "this account cannot route that model".
    ///
    /// One method rather than four call sites deciding for themselves — the
    /// duplication this whole area is being repaired for.
    fn runtime_error(&self, error: &api::ApiError) -> RuntimeError {
        runtime_error_from_api(
            &self.session_id,
            self.account.as_deref(),
            &self.model,
            error,
        )
    }

    pub fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    pub fn set_thinking_enabled(&mut self, enabled: bool) {
        self.thinking_enabled = enabled;
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The session tracer installed on the underlying provider client — the
    /// runtime + CLI read it for structured turn logging. (`BuiltRuntime`
    /// delegates its `session_tracer()` here once this client is installed.)
    #[must_use]
    pub fn session_tracer(&self) -> Option<&SessionTracer> {
        self.client.session_tracer()
    }

    /// Estimated tokens of the request parts history compaction cannot shrink —
    /// the rendered system prompt and the tool definitions this client attaches
    /// — using the same heuristic as the API preflight. Drives the budget-aware
    /// pre-send auto-compaction on the engine turn path.
    #[must_use]
    pub fn fixed_request_overhead_tokens(&self, system_prompt: &runtime::SystemPrompt) -> usize {
        let system = (!system_prompt.is_empty()).then(|| system_prompt.render());
        let tools = self.enable_tools.then(|| {
            self.tool_registry
                .core_definitions(self.allowed_tools.as_ref())
        });
        api::estimate_request_overhead_tokens(system.as_deref(), tools.as_deref()) as usize
    }

    /// Start a streaming response, optionally applying a stall timeout on the
    /// first event for post-tool continuations. Returns an incremental stream of
    /// [`AssistantEvent`]s; dropping it cancels the underlying HTTP request.
    async fn try_start_stream(
        &self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
        is_post_tool: bool,
    ) -> Result<AssistantEventStream, RuntimeError> {
        let mut provider_stream = self
            .client
            .stream_message(message_request, None)
            .await
            .map_err(|error| self.runtime_error(&error))?;

        let prefetched_next = if apply_stall_timeout {
            match tokio::time::timeout(POST_TOOL_STALL_TIMEOUT, provider_stream.next_event()).await
            {
                Ok(inner) => match inner.map_err(|error| self.runtime_error(&error))? {
                    Some(event) => Some(Some(event)),
                    None => {
                        return Err(RuntimeError::new(
                            "post-tool stall: model stream ended before first event",
                        ));
                    }
                },
                Err(_elapsed) => {
                    return Err(RuntimeError::new(
                        "post-tool stall: model did not respond within timeout",
                    ));
                }
            }
        } else {
            None
        };

        let state = StreamState {
            provider_stream,
            pending_tool: None,
            buffer: VecDeque::new(),
            prefetched_next,
            saw_stop: false,
            has_content: false,
            done: false,
            client: self.client.clone(),
            session_id: self.session_id.clone(),
            account: self.account.clone(),
            model: self.model.clone(),
            fallback_request: Some(build_non_streaming_fallback_request(
                message_request,
                is_post_tool,
            )),
        };

        Ok(Box::pin(futures::stream::try_unfold(
            state,
            |mut state| async move {
                if let Some(event) = state.buffer.pop_front() {
                    return Ok(Some((event, state)));
                }
                if state.done {
                    return Ok(None);
                }

                loop {
                    let next = if let Some(prefetched_next) = state.prefetched_next.take() {
                        prefetched_next
                    } else {
                        state.provider_stream.next_event().await.map_err(|error| {
                            runtime_error_from_api(
                                &state.session_id,
                                state.account.as_deref(),
                                &state.model,
                                &error,
                            )
                        })?
                    };

                    let Some(event) = next else {
                        // Provider stream ended — emit prompt cache + a synthetic
                        // stop if needed, then fall back to a non-streaming
                        // request if the stream produced nothing usable.
                        if let Some(record) = state.client.take_last_prompt_cache_record() {
                            if let Some(evt) = prompt_cache_record_to_event(record) {
                                state.buffer.push_back(AssistantEvent::PromptCache(evt));
                            }
                        }
                        if !state.saw_stop && state.has_content {
                            state.buffer.push_back(AssistantEvent::MessageStop);
                        }
                        if state.buffer.is_empty() && !state.saw_stop {
                            if let Some(fallback_request) = state.fallback_request.take() {
                                let response = state
                                    .client
                                    .send_message(&fallback_request, None)
                                    .await
                                    .map_err(|error| {
                                        runtime_error_from_api(
                                            &state.session_id,
                                            state.account.as_deref(),
                                            &state.model,
                                            &error,
                                        )
                                    })?;
                                state.buffer.extend(response_to_events(response));
                                if let Some(record) = state.client.take_last_prompt_cache_record() {
                                    if let Some(evt) = prompt_cache_record_to_event(record) {
                                        state.buffer.push_back(AssistantEvent::PromptCache(evt));
                                    }
                                }
                            }
                        }
                        state.done = true;
                        return Ok(state.buffer.pop_front().map(|evt| (evt, state)));
                    };

                    process_provider_event(
                        event,
                        &mut state.buffer,
                        &mut state.pending_tool,
                        &mut state.saw_stop,
                        &mut state.has_content,
                    );

                    if let Some(event) = state.buffer.pop_front() {
                        return Ok(Some((event, state)));
                    }
                }
            },
        )))
    }
}

/// Carries a [`runtime::RetrySink`] across the one boundary where the
/// transport's own callback shape and the seam's meet.
///
/// `api` cannot name a runtime sink and the renderer cannot name an `api`
/// notifier — the compiler gate exists precisely to keep it that way — so the
/// translation happens here, in the crate that legitimately sees both.
struct RetrySinkNotifier(runtime::RetrySink);

impl api::RetryNotifier for RetrySinkNotifier {
    fn on_retry(&self, attempt: u32, max_retries: u32, reason: &str) {
        self.0.emit(runtime::RetryEvent::Waiting {
            attempt,
            max_retries,
            reason: reason.to_string(),
        });
    }

    fn on_retry_end(&self) {
        self.0.emit(runtime::RetryEvent::Resumed);
    }
}

#[async_trait]
impl ApiClient for EngineApiClient {
    fn set_retry_sink(&mut self, sink: Option<runtime::RetrySink>) {
        self.client.set_retry_notifier(sink.map(|sink| {
            std::sync::Arc::new(RetrySinkNotifier(sink)) as std::sync::Arc<dyn api::RetryNotifier>
        }));
    }

    /// The runtime's default derives these from the capabilities table and
    /// the system prompt alone. This client knows better on both counts: the
    /// output reservation it will actually request and the tool definitions
    /// it attaches to every request. Overriding keeps the in-turn guard and
    /// the engine host's preflight working off the same numbers.
    fn context_budget(
        &self,
        _model: &str,
        system_prompt: &runtime::SystemPrompt,
    ) -> runtime::ContextBudget {
        // Keyed on `self.model`, not the caller's model name, because that is
        // provably what the request will carry (`stream` below builds its
        // `MessageRequest` with `self.model` and
        // `api::max_tokens_for_model(&self.model)`). Budgeting against
        // anything else would let the guard and the provider's rejection
        // disagree.
        runtime::ContextBudget {
            context_limit: runtime::model_capabilities::context_window_or_default(&self.model)
                as usize,
            max_output_tokens: api::max_tokens_for_model(&self.model) as usize,
            overhead_tokens: self.fixed_request_overhead_tokens(system_prompt),
            buffer_tokens: runtime::autocompact_buffer_tokens(&self.model) as usize,
        }
    }

    async fn send_compaction(
        &mut self,
        model: &str,
        system_prompt: &str,
        messages: Vec<ConversationMessage>,
        max_tokens: u32,
    ) -> Result<String, RuntimeError> {
        let request = MessageRequest {
            model: model.to_string(),
            max_tokens,
            messages: tools::convert_messages(&messages),
            system: Some(system_prompt.to_string()),
            tools: None,
            tool_choice: None,
            stream: false,
            reasoning_effort: None,
            cache_hints: None,
            ..Default::default()
        };

        let response = self
            .client
            .send_message(&request, None)
            .await
            .map_err(|error| RuntimeError::new(format!("compaction API error: {error}")))?;

        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                OutputContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            return Err(RuntimeError::new(
                "compaction response contained no text content",
            ));
        }
        Ok(text)
    }

    async fn send_cache_safe_compaction(
        &mut self,
        request: ApiRequest,
        compaction_prompt: &str,
        max_tokens: u32,
    ) -> Result<String, RuntimeError> {
        let cache_hints = (!request.system_prompt.is_empty()).then(|| CacheHints {
            system_static: Some(request.system_prompt.static_text()),
            system_dynamic: Some(request.system_prompt.dynamic_text()),
            breakpoint_last_message: true,
        });

        let mut messages = tools::convert_messages(&request.messages);
        messages.push(InputMessage {
            role: "user".to_string(),
            content: vec![api::InputContentBlock::Text {
                text: compaction_prompt.to_string(),
            }],
        });

        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens,
            messages,
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.render()),
            tools: None,
            tool_choice: None,
            stream: false,
            reasoning_effort: None,
            cache_hints,
            thinking_enabled: false,
            ..Default::default()
        };

        let response = self
            .client
            .send_message(&message_request, None)
            .await
            .map_err(|error| RuntimeError::new(format!("cache-safe compaction error: {error}")))?;

        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                OutputContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if text.is_empty() {
            return Err(RuntimeError::new(
                "cache-safe compaction response contained no text content",
            ));
        }
        Ok(text)
    }

    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        let is_post_tool = request_ends_with_tool_result(&request);
        let cache_hints = (!request.system_prompt.is_empty()).then(|| CacheHints {
            system_static: Some(request.system_prompt.static_text()),
            system_dynamic: Some(request.system_prompt.dynamic_text()),
            breakpoint_last_message: true,
        });
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: api::max_tokens_for_model(&self.model),
            messages: tools::convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.render()),
            tools: self.enable_tools.then(|| {
                self.tool_registry
                    .core_definitions(self.allowed_tools.as_ref())
            }),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: self.reasoning_effort.clone(),
            cache_hints,
            thinking_enabled: self.thinking_enabled,
            ..Default::default()
        };

        // Post-tool continuations get one stall-timeout retry (a nudge); other
        // turns run a single attempt.
        let max_attempts = if is_post_tool { 2 } else { 1 };
        for attempt in 1..=max_attempts {
            match self
                .try_start_stream(&message_request, is_post_tool && attempt == 1, is_post_tool)
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(error)
                    if error.to_string().contains("post-tool stall") && attempt < max_attempts => {}
                Err(error) => return Err(error),
            }
        }
        Err(RuntimeError::new("post-tool continuation nudge exhausted"))
    }
}

/// Incremental stream state for the `try_unfold` above. Holds no render state.
/// Render a provider failure for a human, adding the account when the gateway's
/// complaint is really "this account cannot route that model".
///
/// One implementation shared by every error path in this file: the gateway
/// phrases the rejection in terms of its own routing groups, which nobody
/// configured, and four call sites each deciding how to say that is the
/// duplication this area is being repaired for.
fn user_visible_error(
    session_id: &str,
    account: Option<&str>,
    model: &str,
    error: &api::ApiError,
) -> String {
    let rendered = api::format_user_visible_api_error(session_id, error);
    api::explain_model_not_served(account, model, &rendered).unwrap_or(rendered)
}

/// Turn a provider error into a [`RuntimeError`] that keeps its
/// classification.
///
/// A context-window rejection is the one request failure the runtime can fix
/// on its own — it compacts the history and resends. That recovery is gated
/// on `RuntimeError::is_context_window_blocked`, which is set by
/// construction, not sniffed from the message. Building every provider
/// failure with `RuntimeError::new` therefore left the flag false on the one
/// error it exists for, and the salvage path could never run against a real
/// provider. Route every conversion here.
fn runtime_error_from_api(
    session_id: &str,
    account: Option<&str>,
    model: &str,
    error: &api::ApiError,
) -> RuntimeError {
    let message = user_visible_error(session_id, account, model, error);
    if error.is_context_window_failure() {
        RuntimeError::context_window_blocked(message)
    } else {
        RuntimeError::new(message)
    }
}

struct StreamState {
    provider_stream: MessageStream,
    pending_tool: Option<(String, String, String, Option<String>)>,
    buffer: VecDeque<AssistantEvent>,
    // Three states: `None` = no prefetch happened; `Some(None)` = prefetched and
    // the stream had already ended; `Some(Some(ev))` = prefetched first event
    // (used for the post-tool stall-timeout path).
    #[allow(clippy::option_option)]
    prefetched_next: Option<Option<StreamEvent>>,
    saw_stop: bool,
    has_content: bool,
    done: bool,
    client: ProviderClient,
    session_id: String,
    /// Carried so failures raised inside the stream explain themselves the same
    /// way as failures raised before it — one wording, not two.
    account: Option<String>,
    model: String,
    fallback_request: Option<MessageRequest>,
}

/// Translate one provider event into zero or more [`AssistantEvent`]s. Pure — no
/// I/O, no rendering.
fn process_provider_event(
    event: StreamEvent,
    buffer: &mut VecDeque<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String, Option<String>)>,
    saw_stop: &mut bool,
    has_content: &mut bool,
) {
    match event {
        StreamEvent::MessageStart(start) => {
            if !start.message.model.is_empty() {
                buffer.push_back(AssistantEvent::Model(start.message.model.clone()));
            }
            for block in start.message.content {
                push_output_block(block, buffer, pending_tool, true, has_content);
            }
        }
        StreamEvent::ContentBlockStart(start) => {
            push_output_block(start.content_block, buffer, pending_tool, true, has_content);
        }
        StreamEvent::ContentBlockDelta(delta) => match delta.delta {
            ContentBlockDelta::TextDelta { text } => {
                if !text.is_empty() {
                    *has_content = true;
                    buffer.push_back(AssistantEvent::TextDelta(text));
                }
            }
            ContentBlockDelta::InputJsonDelta { partial_json } => {
                if let Some((_, _, input, _)) = pending_tool {
                    input.push_str(&partial_json);
                }
            }
            ContentBlockDelta::ThinkingDelta { thinking } => {
                buffer.push_back(AssistantEvent::Thinking {
                    thinking,
                    signature: None,
                });
            }
            ContentBlockDelta::SignatureDelta { .. } => {}
        },
        StreamEvent::ContentBlockStop(_) => {
            if let Some((id, name, input, thought_signature)) = pending_tool.take() {
                let input = if input.is_empty() {
                    "{}".to_string()
                } else {
                    input
                };
                *has_content = true;
                buffer.push_back(AssistantEvent::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                });
            }
        }
        StreamEvent::MessageDelta(delta) => {
            buffer.push_back(AssistantEvent::Usage(delta.usage.token_usage()));
        }
        StreamEvent::MessageStop(_) => {
            *saw_stop = true;
            buffer.push_back(AssistantEvent::MessageStop);
        }
    }
}

/// Translate one output content block. Pure — no rendering.
fn push_output_block(
    block: OutputContentBlock,
    buffer: &mut VecDeque<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String, Option<String>)>,
    streaming_tool_input: bool,
    has_content: &mut bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                *has_content = true;
                buffer.push_back(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature,
        } => {
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            *pending_tool = Some((id, name, initial_input, thought_signature));
        }
        OutputContentBlock::Thinking {
            thinking,
            signature,
        } => {
            buffer.push_back(AssistantEvent::Thinking {
                thinking,
                signature,
            });
        }
        OutputContentBlock::RedactedThinking { .. } => {}
    }
}

/// Convert a non-streaming response into events. Pure — no rendering.
fn response_to_events(response: MessageResponse) -> VecDeque<AssistantEvent> {
    let mut events = VecDeque::new();
    let mut pending_tool = None;
    let mut has_content = false;

    for block in response.content {
        push_output_block(
            block,
            &mut events,
            &mut pending_tool,
            false,
            &mut has_content,
        );
        if let Some((id, name, input, thought_signature)) = pending_tool.take() {
            events.push_back(AssistantEvent::ToolUse {
                id,
                name,
                input,
                thought_signature,
            });
        }
    }
    events.push_back(AssistantEvent::Usage(response.usage.token_usage()));
    events.push_back(AssistantEvent::MessageStop);
    events
}

fn prompt_cache_record_to_event(record: PromptCacheRecord) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

/// `true` when the conversation ends with a tool-result message, so the model is
/// expected to continue after tool execution.
fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

fn build_non_streaming_fallback_request(
    request: &MessageRequest,
    is_post_tool: bool,
) -> MessageRequest {
    let mut fallback = MessageRequest {
        stream: false,
        ..request.clone()
    };
    if is_post_tool {
        fallback.tools = None;
        fallback.tool_choice = None;
        fallback
            .messages
            .push(InputMessage::user_text(POST_TOOL_FINAL_SYNTHESIS_PROMPT));
    }
    fallback
}
