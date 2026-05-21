use std::collections::VecDeque;
use std::io::{self, Write};

use api::{
    resolve_startup_auth_source, AnthropicClient, AuthMode, AuthSource, CacheHints,
    ContentBlockDelta, ImageSource, InputContentBlock, InputMessage, MessageRequest,
    MessageResponse, OutputContentBlock, PromptCache, ProviderClient as ApiProviderClient,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use async_trait::async_trait;
use futures::StreamExt;
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, ContentBlock, ConversationMessage,
    MessageRole, PromptCacheEvent, RuntimeError, TokenUsage,
};
use telemetry::{SessionTracer, SudoclawLogSink};
use tools::GlobalToolRegistry;

use super::format::{format_tool_call_start, format_user_visible_api_error};
use crate::render::{MarkdownStreamState, TerminalRenderer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{
    AllowedToolSet, InternalPromptProgressReporter, RuntimeConfig, POST_TOOL_STALL_TIMEOUT,
};

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
pub(crate) struct AnthropicRuntimeClient {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) client: ApiProviderClient,
    pub(crate) session_id: String,
    pub(crate) model: String,
    pub(crate) enable_tools: bool,
    pub(crate) emit_output: bool,
    pub(crate) allowed_tools: Option<AllowedToolSet>,
    pub(crate) tool_registry: GlobalToolRegistry,
    pub(crate) progress_reporter: Option<InternalPromptProgressReporter>,
    pub(crate) reasoning_effort: Option<String>,
    /// Shared flag from the Spinner. Set to `true` before writing output to
    /// pause the spinner animation, `false` after to let it resume.
    pub(crate) spinner_pause: Option<Arc<AtomicBool>>,
}

impl AnthropicRuntimeClient {
    pub(crate) fn new(
        session_id: &str,
        config: &RuntimeConfig,
        tool_registry: GlobalToolRegistry,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let sudocode_config = &config.sudocode_config;
        let effective_mode = config.auth_mode;

        let resolved = api::resolve_provider_from_config(
            &config.model,
            Some(effective_mode),
            sudocode_config,
        )?;
        let mut client = ApiProviderClient::from_resolved(&resolved, Some(effective_mode))?
            .with_prompt_cache(PromptCache::new(session_id));

        // 默认启用日志追踪
        let sink = Arc::new(SudoclawLogSink::new()?);
        let tracer = SessionTracer::new(session_id, sink);
        client = client.with_session_tracer(tracer);

        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model: config.model.clone(),
            enable_tools: config.enable_tools,
            emit_output: config.emit_output,
            allowed_tools: config.allowed_tools.clone(),
            tool_registry,
            progress_reporter: config.progress_reporter.clone(),
            reasoning_effort: None,
            spinner_pause: None,
        })
    }

    pub(crate) fn set_spinner_pause(&mut self, flag: Arc<AtomicBool>) {
        self.spinner_pause = Some(flag);
    }

    /// Pause the spinner and clear its line before writing content.
    fn pause_spinner(&self) {
        if let Some(flag) = &self.spinner_pause {
            flag.store(true, Ordering::SeqCst);
            // Brief sleep to let the spinner thread finish its current tick.
            std::thread::sleep(std::time::Duration::from_millis(10));
            // Clear the spinner text from the current line.
            let _ = write!(io::stdout(), "\r\x1b[2K");
            let _ = io::stdout().flush();
        }
    }

    /// Resume the spinner after content has been written.
    fn resume_spinner(&self) {
        if let Some(flag) = &self.spinner_pause {
            flag.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }

    /// Returns a reference to the session tracer, if available.
    pub(crate) fn session_tracer(&self) -> Option<&telemetry::SessionTracer> {
        self.client.session_tracer()
    }
}

pub(crate) fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    Ok(resolve_cli_auth_source_for_cwd()?)
}

pub(crate) fn resolve_cli_auth_source_for_cwd() -> Result<AuthSource, api::ApiError> {
    resolve_startup_auth_source(|| Ok(None))
}

#[async_trait]
impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let is_post_tool = request_ends_with_tool_result(&request);
        let cache_hints = (!request.system_prompt.is_empty()).then(|| CacheHints {
            system_static: Some(request.system_prompt.static_text()),
            system_dynamic: Some(request.system_prompt.dynamic_text()),
            breakpoint_last_message: true,
        });
        let trace_id = request.trace_id.as_deref();
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.render()),
            tools: self
                .enable_tools
                .then(|| filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref())),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: self.reasoning_effort.clone(),
            cache_hints,
            ..Default::default()
        };

        // When resuming after tool execution, apply a stall timeout on the
        // first stream event.  If the model does not respond within the
        // deadline we drop the stalled connection and re-send the request as
        // a continuation nudge (one retry only).
        let max_attempts: usize = if is_post_tool { 2 } else { 1 };

        for attempt in 1..=max_attempts {
            let result = self
                .try_start_stream(&message_request, is_post_tool && attempt == 1, trace_id)
                .await;
            match result {
                Ok(stream) => return Ok(stream),
                Err(error)
                    if error.to_string().contains("post-tool stall") && attempt < max_attempts =>
                {
                    // Stalled after tool completion — nudge the model by
                    // re-sending the same request.
                }
                Err(error) => return Err(error),
            }
        }

        Err(RuntimeError::new("post-tool continuation nudge exhausted"))
    }
}

/// Internal state for the unfold-based `AssistantEventStream` produced by
/// [`AnthropicRuntimeClient::try_start_stream`].
#[allow(clippy::struct_excessive_bools)]
struct CliStreamState {
    provider_stream: api::MessageStream,
    session_id: String,
    emit_output: bool,
    progress_reporter: Option<InternalPromptProgressReporter>,
    spinner_pause: Option<Arc<AtomicBool>>,
    pending_tool: Option<(String, String, String, Option<String>)>,
    block_has_thinking_summary: bool,
    markdown_stream: MarkdownStreamState,
    renderer: TerminalRenderer,
    glyph_state: ResponseGlyphState,
    buffer: VecDeque<AssistantEvent>,
    saw_stop: bool,
    has_content: bool,
    received_any_event: bool,
    apply_stall_timeout: bool,
    done: bool,
    /// Clone of the provider client used to extract prompt cache at end of stream.
    client: ApiProviderClient,
    /// Non-streaming fallback request (used when streaming yields no events).
    fallback_request: Option<MessageRequest>,
}

impl CliStreamState {
    fn pause_spinner(&self) {
        if let Some(flag) = &self.spinner_pause {
            flag.store(true, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = write!(io::stdout(), "\r\x1b[2K");
            let _ = io::stdout().flush();
        }
    }

    fn resume_spinner(&self) {
        if let Some(flag) = &self.spinner_pause {
            flag.store(false, Ordering::SeqCst);
        }
    }

    /// Process a single provider event, converting it into zero or more
    /// [`AssistantEvent`]s pushed onto `self.buffer`.  All I/O (terminal
    /// rendering) happens synchronously here so that no `dyn Write` reference
    /// is held across an `.await` point.
    #[allow(clippy::too_many_lines)]
    fn process_provider_event(&mut self, event: ApiStreamEvent) -> Result<(), RuntimeError> {
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        match event {
            ApiStreamEvent::MessageStart(start) => {
                for block in start.message.content {
                    push_output_block(
                        block,
                        out,
                        &mut self.buffer,
                        &mut self.pending_tool,
                        true,
                        &mut self.block_has_thinking_summary,
                        &mut self.glyph_state,
                    )?;
                }
            }
            ApiStreamEvent::ContentBlockStart(start) => {
                push_output_block(
                    start.content_block,
                    out,
                    &mut self.buffer,
                    &mut self.pending_tool,
                    true,
                    &mut self.block_has_thinking_summary,
                    &mut self.glyph_state,
                )?;
            }
            ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                ContentBlockDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_text_phase(&text);
                        }
                        if let Some(rendered) = self.markdown_stream.push(&self.renderer, &text) {
                            self.pause_spinner();
                            let prefixed = self.glyph_state.apply(&rendered);
                            write!(out, "{prefixed}")
                                .and_then(|()| out.flush())
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                        }
                        self.has_content = true;
                        self.buffer.push_back(AssistantEvent::TextDelta(text));
                    }
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    if let Some((_, _, input, _)) = &mut self.pending_tool {
                        input.push_str(&partial_json);
                    }
                }
                ContentBlockDelta::ThinkingDelta { .. } => {
                    if !self.block_has_thinking_summary {
                        self.pause_spinner();
                        render_thinking_block_summary(out, None, false)?;
                        self.block_has_thinking_summary = true;
                        self.glyph_state.visible_col = 0;
                    }
                }
                ContentBlockDelta::SignatureDelta { .. } => {}
            },
            ApiStreamEvent::ContentBlockStop(_) => {
                self.block_has_thinking_summary = false;
                if let Some(rendered) = self.markdown_stream.flush(&self.renderer) {
                    let prefixed = self.glyph_state.apply(&rendered);
                    write!(out, "{prefixed}")
                        .and_then(|()| out.flush())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                }
                if let Some((id, name, input, thought_signature)) = self.pending_tool.take() {
                    if let Some(progress_reporter) = &self.progress_reporter {
                        progress_reporter.mark_tool_phase(&name, &input);
                    }
                    self.pause_spinner();
                    writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                        .and_then(|()| out.flush())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    self.glyph_state.visible_col = 0;
                    self.resume_spinner();
                    self.has_content = true;
                    self.buffer.push_back(AssistantEvent::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                    });
                }
            }
            ApiStreamEvent::MessageDelta(delta) => {
                self.buffer
                    .push_back(AssistantEvent::Usage(delta.usage.token_usage()));
            }
            ApiStreamEvent::MessageStop(_) => {
                self.saw_stop = true;
                if let Some(rendered) = self.markdown_stream.flush(&self.renderer) {
                    let prefixed = self.glyph_state.apply(&rendered);
                    write!(out, "{prefixed}")
                        .and_then(|()| out.flush())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                }
                self.buffer.push_back(AssistantEvent::MessageStop);
            }
        }
        Ok(())
    }
}

impl AnthropicRuntimeClient {
    /// Start a streaming response, optionally applying a stall timeout on the
    /// first event for post-tool continuations.  Returns an incremental stream
    /// of [`AssistantEvent`]s.  Dropping the stream cancels the underlying
    /// HTTP request.
    async fn try_start_stream(
        &mut self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
        trace_id: Option<&str>,
    ) -> Result<AssistantEventStream, RuntimeError> {
        let provider_stream = self
            .client
            .stream_message(message_request, trace_id)
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;

        let state = CliStreamState {
            provider_stream,
            session_id: self.session_id.clone(),
            emit_output: self.emit_output,
            progress_reporter: self.progress_reporter.clone(),
            spinner_pause: self.spinner_pause.clone(),
            pending_tool: None,
            block_has_thinking_summary: false,
            markdown_stream: MarkdownStreamState::default(),
            renderer: TerminalRenderer::new(),
            glyph_state: ResponseGlyphState::new(query_terminal_width()),
            buffer: VecDeque::new(),
            saw_stop: false,
            has_content: false,
            received_any_event: false,
            apply_stall_timeout,
            done: false,
            client: self.client.clone(),
            fallback_request: Some(MessageRequest {
                stream: false,
                ..message_request.clone()
            }),
        };

        Ok(Box::pin(futures::stream::try_unfold(
            state,
            |mut state| async move {
                // Yield buffered events first.
                if let Some(event) = state.buffer.pop_front() {
                    return Ok(Some((event, state)));
                }
                if state.done {
                    return Ok(None);
                }

                loop {
                    let next = if state.apply_stall_timeout && !state.received_any_event {
                        match tokio::time::timeout(
                            POST_TOOL_STALL_TIMEOUT,
                            state.provider_stream.next_event(),
                        )
                        .await
                        {
                            Ok(inner) => inner.map_err(|error| {
                                RuntimeError::new(format_user_visible_api_error(
                                    &state.session_id,
                                    &error,
                                ))
                            })?,
                            Err(_elapsed) => {
                                return Err(RuntimeError::new(
                                    "post-tool stall: model did not respond within timeout",
                                ));
                            }
                        }
                    } else {
                        state.provider_stream.next_event().await.map_err(|error| {
                            RuntimeError::new(format_user_visible_api_error(
                                &state.session_id,
                                &error,
                            ))
                        })?
                    };

                    let Some(event) = next else {
                        // Provider stream ended — emit prompt cache and
                        // synthetic stop if needed, then signal done.
                        if let Some(record) = state.client.take_last_prompt_cache_record() {
                            if let Some(evt) = prompt_cache_record_to_runtime_event(record) {
                                state.buffer.push_back(AssistantEvent::PromptCache(evt));
                            }
                        }
                        if !state.saw_stop && state.has_content {
                            state.buffer.push_back(AssistantEvent::MessageStop);
                        }

                        // If stream produced nothing useful, fall back to
                        // non-streaming request.
                        if state.buffer.is_empty() && !state.saw_stop {
                            if let Some(fallback_request) = state.fallback_request.take() {
                                let response = state
                                    .client
                                    .send_message(&fallback_request, None)
                                    .await
                                    .map_err(|error| {
                                        RuntimeError::new(format_user_visible_api_error(
                                            &state.session_id,
                                            &error,
                                        ))
                                    })?;
                                // response_to_events does sync I/O (no await),
                                // so the dyn Write borrow is safe here.
                                let mut stdout = io::stdout();
                                let mut sink = io::sink();
                                let out: &mut dyn Write = if state.emit_output {
                                    &mut stdout
                                } else {
                                    &mut sink
                                };
                                let events = response_to_events(response, out)?;
                                state.buffer.extend(events);
                                if let Some(record) = state.client.take_last_prompt_cache_record() {
                                    if let Some(evt) = prompt_cache_record_to_runtime_event(record)
                                    {
                                        state.buffer.push_back(AssistantEvent::PromptCache(evt));
                                    }
                                }
                            }
                        }

                        state.done = true;
                        return Ok(state.buffer.pop_front().map(|evt| (evt, state)));
                    };
                    state.received_any_event = true;

                    // Process the provider event synchronously (all I/O
                    // happens inside this call, no dyn Write held across
                    // await points).
                    state.process_provider_event(event)?;

                    // If we produced any buffered events, yield the first one.
                    if let Some(event) = state.buffer.pop_front() {
                        return Ok(Some((event, state)));
                    }
                    // Otherwise loop to read more from the provider.
                }
            },
        )))
    }
}

/// Returns `true` when the conversation ends with a tool-result message,
/// meaning the model is expected to continue after tool execution.
pub(crate) fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

pub(crate) fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(crate) fn collect_tool_uses(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some(serde_json::json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_tool_results(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(serde_json::json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

pub(crate) fn collect_prompt_cache_events(
    summary: &runtime::TurnSummary,
) -> Vec<serde_json::Value> {
    summary
        .prompt_cache_events
        .iter()
        .map(|event| {
            serde_json::json!({
                "unexpected": event.unexpected,
                "reason": event.reason,
                "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                "token_drop": event.token_drop,
            })
        })
        .collect()
}

pub(crate) fn max_tokens_for_model(model: &str) -> u32 {
    api::max_tokens_for_model(model)
}

pub(crate) fn render_thinking_block_summary(
    out: &mut (impl Write + ?Sized),
    char_count: Option<usize>,
    redacted: bool,
) -> Result<(), RuntimeError> {
    let summary = if redacted {
        "\n  ▶ Thinking block hidden by provider\n".to_string()
    } else if let Some(char_count) = char_count {
        format!("\n  ▶ Thinking ({char_count} chars hidden)\n")
    } else {
        "\n  ▶ Thinking hidden\n".to_string()
    };
    write!(out, "{summary}")
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))
}

/// Stateful processor that prefixes the first line with ⏺ (bold) and indents
/// all continuation lines by two spaces so that column 0 is reserved
/// exclusively for status glyphs. Hard-wraps text at the terminal width so
/// the terminal never soft-wraps into column 0.
pub(crate) struct ResponseGlyphState {
    started: bool,
    visible_col: usize,
    max_col: usize,
    in_escape: bool,
}

impl ResponseGlyphState {
    fn new(terminal_width: usize) -> Self {
        Self {
            started: false,
            visible_col: 0,
            // Ensure at least 4 columns to avoid degenerate wrapping.
            max_col: terminal_width.max(4),
            in_escape: false,
        }
    }

    /// Process a rendered ANSI text chunk. Returns the wrapped+margined output.
    fn apply(&mut self, rendered: &str) -> String {
        if rendered.is_empty() {
            return String::new();
        }

        let mut out = String::with_capacity(rendered.len() + 64);

        for ch in rendered.chars() {
            if ch == '\r' {
                out.push(ch);
                self.visible_col = 0;
                continue;
            }
            if ch == '\n' {
                out.push(ch);
                self.visible_col = 0;
                continue;
            }

            // At line start, emit glyph or margin.
            if self.visible_col == 0 {
                if self.started {
                    out.push_str("  ");
                } else {
                    self.started = true;
                    out.push_str("\r\x1b[2K\x1b[1m⏺\x1b[0m ");
                }
                self.visible_col = 2;
            }

            // ANSI escape start.
            if ch == '\x1b' {
                self.in_escape = true;
                out.push(ch);
                continue;
            }

            // Inside an ANSI CSI sequence — push until ASCII letter terminates.
            if self.in_escape {
                out.push(ch);
                if ch.is_ascii_alphabetic() {
                    self.in_escape = false;
                }
                continue;
            }

            // Hard wrap: line has reached the terminal edge.
            if self.visible_col >= self.max_col {
                out.push('\n');
                out.push_str("  ");
                self.visible_col = 2;
            }

            out.push(ch);
            self.visible_col += 1;
        }

        out
    }
}

fn query_terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(80)
}

pub(crate) fn push_output_block(
    block: OutputContentBlock,
    out: &mut (impl Write + ?Sized),
    events: &mut VecDeque<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String, Option<String>)>,
    streaming_tool_input: bool,
    block_has_thinking_summary: &mut bool,
    glyph_state: &mut ResponseGlyphState,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                let prefixed = glyph_state.apply(&rendered);
                write!(out, "{prefixed}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push_back(AssistantEvent::TextDelta(text));
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
        OutputContentBlock::Thinking { thinking, .. } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            *block_has_thinking_summary = true;
            glyph_state.visible_col = 0;
        }
        OutputContentBlock::RedactedThinking { .. } => {
            render_thinking_block_summary(out, None, true)?;
            *block_has_thinking_summary = true;
            glyph_state.visible_col = 0;
        }
    }
    Ok(())
}

pub(crate) fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
) -> Result<VecDeque<AssistantEvent>, RuntimeError> {
    let mut events = VecDeque::new();
    let mut pending_tool = None;
    let mut glyph_state = ResponseGlyphState::new(query_terminal_width());

    for block in response.content {
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
            &mut glyph_state,
        )?;
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
    Ok(events)
}

pub(crate) fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

pub(crate) fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    let mut result: Vec<InputMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        let role = match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        let content = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(InputContentBlock::Text { text: text.clone() }),
                ContentBlock::Image { data, mime_type } => Some(InputContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: mime_type.clone(),
                        data: data.clone(),
                    },
                }),
                ContentBlock::Thinking { .. } => None,
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    thought_signature,
                } => Some(InputContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::from_str(input)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    thought_signature: thought_signature.clone(),
                }),
                ContentBlock::ToolResult {
                    tool_use_id,
                    output,
                    is_error,
                    ..
                } => Some(InputContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: vec![ToolResultContentBlock::Text {
                        text: output.clone(),
                    }],
                    is_error: *is_error,
                }),
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            continue;
        }

        // Merge consecutive Tool-role messages into the previous user-role
        // InputMessage. The runtime pushes each tool_result as its own
        // Tool-role ConversationMessage, but Anthropic requires every
        // `tool_use` in an assistant turn to have its matching `tool_result`
        // in the SAME next user message — splitting them across consecutive
        // user messages triggers `messages.N: tool_use ids` 400 errors.
        // OpenAI and Gemini conversions iterate per content block and emit
        // their own per-result messages, so merging here is a no-op for them.
        if matches!(message.role, MessageRole::Tool) {
            if let Some(last) = result.last_mut() {
                if last.role == "user"
                    && last
                        .content
                        .iter()
                        .all(|block| matches!(block, InputContentBlock::ToolResult { .. }))
                {
                    last.content.extend(content);
                    continue;
                }
            }
        }

        result.push(InputMessage {
            role: role.to_string(),
            content,
        });
    }
    result
}

pub(crate) fn filter_tool_specs(
    tool_registry: &GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<ToolDefinition> {
    tool_registry.definitions(allowed_tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str, name: &str) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: "{}".to_string(),
                thought_signature: None,
            }],
            usage: None,
            model: None,
        }
    }

    fn tool_use_multi(ids_and_names: &[(&str, &str)]) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Assistant,
            blocks: ids_and_names
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: "{}".to_string(),
                    thought_signature: None,
                })
                .collect(),
            usage: None,
            model: None,
        }
    }

    fn tool_result(id: &str, name: &str, output: &str) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                tool_name: name.to_string(),
                output: output.to_string(),
                is_error: false,
            }],
            usage: None,
            model: None,
        }
    }

    #[test]
    fn convert_messages_merges_consecutive_tool_results_into_single_user_message() {
        // Assistant emits two parallel tool_use blocks; runtime appends each
        // tool_result as its own Tool-role ConversationMessage. Anthropic
        // requires both tool_results in the same next user message — without
        // the merge, the second tool_use's id has no matching tool_result in
        // the immediately-following user turn, triggering a 400 error.
        let messages = vec![
            tool_use_multi(&[("call_a", "fn_a"), ("call_b", "fn_b")]),
            tool_result("call_a", "fn_a", "result_a"),
            tool_result("call_b", "fn_b", "result_b"),
        ];

        let converted = convert_messages(&messages);

        assert_eq!(converted.len(), 2, "tool_results must be merged");
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[1].content.len(), 2);
        for block in &converted[1].content {
            assert!(matches!(block, InputContentBlock::ToolResult { .. }));
        }
    }

    #[test]
    fn convert_messages_does_not_merge_tool_result_into_plain_user_text() {
        // If the previous user message is plain text (not a tool_result
        // bundle), we must not merge a fresh tool_result into it.
        let messages = vec![
            ConversationMessage {
                role: MessageRole::User,
                blocks: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
                usage: None,
                model: None,
            },
            tool_use("call_a", "fn_a"),
            tool_result("call_a", "fn_a", "ok"),
        ];

        let converted = convert_messages(&messages);

        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "user");
        assert!(matches!(
            converted[0].content[0],
            InputContentBlock::Text { .. }
        ));
        assert_eq!(converted[2].role, "user");
        assert!(matches!(
            converted[2].content[0],
            InputContentBlock::ToolResult { .. }
        ));
    }
}
