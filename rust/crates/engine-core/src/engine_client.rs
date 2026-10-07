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
//! renderer). It keeps the post-tool stall timeout + empty-response retry +
//! prompt-cache extraction, since those change *which events* are produced (core
//! behavior), not how they look.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use api::{
    AuthMode, ContentBlockDelta, InputMessage, MessageRequest, MessageResponse, MessageStream,
    OutputContentBlock, PromptCache, PromptCacheRecord, ProviderClient, ResolvedProvider,
    StreamEvent, SudoCodeConfig, ToolChoice, ToolDefinition,
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
    require_model_mount: bool,
    client: ProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    allowed_tools: Option<BTreeSet<String>>,
    tool_registry: GlobalToolRegistry,
    cold_tools: OnceLock<Vec<api::ToolDefinition>>,
    revealed_tools: OnceLock<Vec<api::ToolDefinition>>,
    reasoning_effort: Option<String>,
    thinking_enabled: bool,
    /// The account this client bills to, for error messages. The gateway rejects
    /// an unroutable model in terms of its own routing groups, which the user
    /// never configured; naming the account turns that into an actionable edit.
    account: Option<String>,
    catalog: Option<runtime::model_discovery::ModelCatalog>,
}

impl EngineApiClient {
    /// Identity every request from this client carries, built in one place so
    /// the main loop and the subagent client cannot disagree about the routing
    /// key — a conversation split across two keys is split across two upstream
    /// accounts, and the prompt cache is per-account.
    #[inline]
    fn request_metadata(&self) -> api::RequestMetadata {
        api::RequestMetadata::for_session(&self.session_id)
    }

    /// Build each schema set once, with deterministic order across rebuilds.
    #[inline]
    fn tool_definitions(&self, revealed: bool) -> Option<&[api::ToolDefinition]> {
        if !self.enable_tools {
            return None;
        }
        let cold = self.cold_tools.get_or_init(|| {
            let mut definitions = self
                .tool_registry
                .core_definitions(self.allowed_tools.as_ref(), None);
            definitions.sort_by(|a, b| a.name.cmp(&b.name));
            definitions
        });
        Some(if revealed {
            self.revealed_tools.get_or_init(|| {
                let mut definitions = cold.clone();
                for definition in &mut definitions {
                    definition.defer_loading = false;
                }
                definitions
            })
        } else {
            cold
        })
    }

    #[inline]
    fn request_tools(&self, request: &ApiRequest) -> Option<Vec<api::ToolDefinition>> {
        if !self.enable_tools {
            return None;
        }
        Some(
            self.request_tool_definitions(&request.messages, &request.pre_compact_discovered_tools),
        )
    }

    #[inline]
    fn session_request_fields(&self) -> api::SessionRequestFields {
        api::SessionRequestFields {
            metadata: Some(self.request_metadata()),
            reasoning_effort: self.reasoning_effort.clone(),
        }
    }

    /// Build a client for `model`, resolving the provider from config + auth
    /// mode (identical resolution to the old CLI client, minus the render
    /// plumbing).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: &str,
        sudocode_config: &SudoCodeConfig,
        model: &str,
        auth_mode: AuthMode,
        tool_registry: GlobalToolRegistry,
        enable_tools: bool,
        allowed_tools: Option<BTreeSet<String>>,
        access: &api::ModelAccess,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let resolved: ResolvedProvider =
            api::resolve_provider_from_config(model, Some(auth_mode), sudocode_config)?;
        let catalog = if resolved.base_url.starts_with("nexus://") || access.require_mount {
            None
        } else {
            api::model_discovery::model_catalog_for_resolved(&resolved)
        };
        if let Some(catalog) = &catalog {
            catalog.refresh_in_background();
        }
        let mut client =
            ProviderClient::from_resolved_with_access(&resolved, Some(auth_mode), access)?
                .with_prompt_cache(PromptCache::new(session_id));
        let sink = Arc::new(SudoclawLogSink::new()?);
        client = client.with_session_tracer(SessionTracer::new(session_id, sink));

        // Same selector `doctor` reports through, so an error names the account
        // the report shows. Best-effort: a client that cannot name its account
        // still works, it just explains less.
        let account = api::proxy_account_for_model(sudocode_config, model)
            .ok()
            .map(|selected| selected.name.to_string());

        // Publish the mode for subagents, here in the one place every session
        // (REPL, ACP, co-host) and every `/auth` / `/model` rebuild passes
        // through holding a concrete `AuthMode` — and only on the success path,
        // so "published" means a session is actually running on it. A subagent
        // resolves its provider on its own thread and cannot see this session's
        // `--auth`; left to auto-detect it picks `subscription` and dies on "no
        // token available for subscription provider". The note on
        // `request_metadata` above says why the two must agree: two credential
        // paths are two upstream accounts, and the prompt cache is per-account.
        tools::set_global_auth_mode(auth_mode);

        Ok(Self {
            require_model_mount: access.require_mount,
            client,
            session_id: session_id.to_string(),
            model: resolved.model_id.clone(),
            enable_tools,
            allowed_tools,
            tool_registry,
            cold_tools: OnceLock::new(),
            revealed_tools: OnceLock::new(),
            reasoning_effort: None,
            thinking_enabled: true,
            account,
            catalog,
        })
    }

    /// Render a provider failure for a human, adding the account when the
    /// gateway's complaint is really "this account cannot route that model".
    ///
    /// One method rather than four call sites deciding for themselves — the
    /// duplication this whole area is being repaired for.
    fn runtime_error(&self, error: &api::ApiError) -> RuntimeError {
        if let api::ApiError::ToolCallingUnsupported { model } = error {
            if let Some(tracer) = self.session_tracer() {
                tracer.record(
                    "model_capability_rejected",
                    serde_json::Map::from_iter([
                        ("model".into(), serde_json::json!(model)),
                        ("capability".into(), serde_json::json!("tool_calling")),
                        ("source".into(), serde_json::json!("endpoint_catalog")),
                    ]),
                );
            }
        }
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
        api::estimate_request_overhead_tokens(system.as_deref(), self.tool_definitions(false))
            as usize
    }

    /// The `tools` array a request over `messages` carries — the same
    /// `core_definitions` call [`ApiClient::stream`] makes, with the same
    /// discovered-tool reveal — so `/context` counts what is actually on the
    /// wire rather than a second guess at it. Empty when tools are disabled.
    #[must_use]
    #[inline]
    pub fn request_tool_definitions(
        &self,
        messages: &[ConversationMessage],
        pre_compact_discovered_tools: &BTreeSet<String>,
    ) -> Vec<ToolDefinition> {
        if !self.enable_tools {
            return Vec::new();
        }
        let revealed = self.revealed_tools.get().is_some()
            || !pre_compact_discovered_tools.is_empty()
            || !tools::extract_discovered_tool_names(messages).is_empty();
        self.tool_definitions(revealed)
            .map_or_else(Vec::new, <[_]>::to_vec)
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
            retry_request: Some(build_empty_response_retry_request(
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
                        // stop if needed, then retry once if the stream produced
                        // nothing usable.
                        if let Some(record) = state.client.take_last_prompt_cache_record() {
                            if let Some(evt) = prompt_cache_record_to_event(record) {
                                state.buffer.push_back(AssistantEvent::PromptCache(evt));
                            }
                        }
                        if !state.saw_stop && state.has_content {
                            // Tools may already have run while streaming. EOF
                            // cannot certify that the response finished, and a
                            // retry could repeat their side effects.
                            return Err(RuntimeError::new("provider stream ended without message_stop; partial progress preserved"));
                        }
                        // A terminal frame and cache/usage metadata do not make
                        // an empty response useful. Retry it once regardless of
                        // how the gateway framed the end of the stream.
                        if !state.has_content {
                            if let Some(retry_request) = state.retry_request.take() {
                                let response = state
                                    .client
                                    .send_message_streamed(&retry_request, None)
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
    fn requires_model_mount(&self) -> bool {
        self.require_model_mount
    }

    fn model_catalog(&self) -> Option<runtime::model_discovery::ModelCatalog> {
        self.catalog.clone()
    }
    fn wire_model_id(&self) -> Option<&str> {
        Some(&self.model)
    }

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
        let _catalog_scope = self
            .catalog
            .as_ref()
            .map(runtime::model_discovery::ModelCatalog::enter);
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

    async fn complete_text(
        &mut self,
        request: ApiRequest,
        options: runtime::TextCompletionOptions,
    ) -> Result<runtime::TextCompletion, RuntimeError> {
        let catalog = self.catalog.clone();
        let request = async {
            let tools = if options.include_tools {
                self.request_tools(&request)
            } else {
                None
            };
            self.client
                .complete_text(
                    &self.model,
                    request,
                    options,
                    tools,
                    self.session_request_fields(),
                )
                .await
        };
        if let Some(catalog) = catalog {
            catalog.refresh_if_missing().await;
            catalog.scope(request).await
        } else {
            request.await
        }
    }

    fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }

    fn routing_session_id(&self) -> Option<&str> {
        Some(self.session_id.as_str())
    }

    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        let catalog = self.catalog.clone();
        let request = async {
            let is_post_tool = request_ends_with_tool_result(&request);
            let tools = self.request_tools(&request);
            let mut message_request = api::session_message_request(
                &self.model,
                &request,
                runtime::TextCompletionOptions {
                    max_tokens: api::max_tokens_for_model(&self.model),
                    include_tools: self.enable_tools,
                    cache_prefix: true,
                    thinking_enabled: self.thinking_enabled,
                },
                tools,
                self.session_request_fields(),
            );
            message_request.tool_choice = self.enable_tools.then_some(ToolChoice::Auto);

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
                        if error.to_string().contains("post-tool stall")
                            && attempt < max_attempts => {}
                    Err(error) => return Err(error),
                }
            }
            Err(RuntimeError::new("post-tool continuation nudge exhausted"))
        };
        if let Some(catalog) = catalog {
            catalog.refresh_if_missing().await;
            catalog.scope(request).await
        } else {
            request.await
        }
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
        RuntimeError::new(message).retryable(error.is_retryable())
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
    /// Sent once if the stream ends having produced nothing usable. `None` once
    /// spent, so an empty answer costs at most one extra turn.
    retry_request: Option<MessageRequest>,
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
            // The signature arrives in its own delta, after the thinking text.
            // Dropping it used to be free-looking — nothing renders it — but it
            // is what makes the thinking block replayable: `convert_messages`
            // only sends a thinking block back when it is signed, and a turn
            // replayed without its thinking block invalidates the whole cached
            // prefix on every tool round-trip. Carried as a Thinking event with
            // no text so the block it belongs to picks it up in order; the
            // observer skips empty deltas so nothing renders.
            ContentBlockDelta::SignatureDelta { signature } => {
                buffer.push_back(AssistantEvent::Thinking {
                    thinking: String::new(),
                    signature: Some(signature),
                });
            }
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
            // The provider codec completes on stop_reason without waiting for
            // message_stop in another packet. Carry that explicit completion
            // across the engine boundary; EOF alone must still fail below.
            if delta.delta.stop_reason.is_some() && !*saw_stop {
                *saw_stop = true;
                buffer.push_back(AssistantEvent::MessageStop);
            }
        }
        StreamEvent::MessageStop(_) => {
            if !*saw_stop {
                *saw_stop = true;
                buffer.push_back(AssistantEvent::MessageStop);
            }
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
            buffer.push_back(AssistantEvent::ThinkingStart);
            buffer.push_back(AssistantEvent::Thinking {
                thinking,
                signature,
            });
        }
        OutputContentBlock::RedactedThinking { data } => {
            buffer.push_back(AssistantEvent::RedactedThinking {
                data: data.to_string(),
            });
        }
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

/// The one retry for a stream that ended having produced nothing usable.
///
/// This used to re-send the request non-streaming, on the theory that the
/// streaming transport was what had failed. It is the wrong remedy twice over.
/// A non-streaming request writes nothing to the socket until generation has
/// finished, and on this path a connection that stays byte-quiet for ~50s is
/// closed with no HTTP response at all (measured: `stream: false` died at 50.3s
/// where `stream: true` had its first byte at 1.7s and ran 201.8s to
/// completion) — so the retry was in the one shape least likely to survive, and
/// most likely to be slow, since it carries the whole conversation. And the
/// transport was rarely the problem in the first place: an upstream that
/// answers `stream: true` with a whole JSON body is now read directly
/// (`api::sse`), leaving this retry for the case it actually addresses — the
/// model returned no usable content.
///
/// So the shape is unchanged and the remedy is about content: after a tool
/// result, drop the tools and ask for a plain final message, which is what
/// turns a stalled tool loop into an answer.
fn build_empty_response_retry_request(
    request: &MessageRequest,
    is_post_tool: bool,
) -> MessageRequest {
    let mut retry = request.clone();
    if is_post_tool {
        retry.tools = None;
        retry.tool_choice = None;
        retry
            .messages
            .push(InputMessage::user_text(POST_TOOL_FINAL_SYNTHESIS_PROMPT));
    }
    retry
}
