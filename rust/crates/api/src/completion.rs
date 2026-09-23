//! Shared non-streaming text transport for main-agent and subagent clients.
//! Summarization prompts, retries, and checkpoint validation stay in runtime.

use crate::{
    CacheHints, InputContentBlock, InputMessage, MessageRequest, OutputContentBlock,
    ProviderClient, RequestMetadata, ToolDefinition, ToolResultContentBlock,
};
use runtime::{
    ApiRequest, ContentBlock, ConversationMessage, MessageRole, RuntimeError, TextCompletion,
    TextCompletionOptions,
};

impl ProviderClient {
    /// Build and send a text-only request with caller-selected model and schemas.
    /// Return completion metadata unchanged so the consumer can validate it.
    ///
    /// `metadata` is the caller's routing key, and it is a parameter rather
    /// than something this function could derive because this transport has
    /// no session of its own — it serves whichever client calls it. Omitting
    /// it is the expensive case: compaction goes through here carrying the
    /// entire conversation, so a compaction request that routes to a
    /// different upstream account than the turns around it pays a full cold
    /// write for the whole history, twice — once here, once when the next
    /// turn lands back on the original account.
    pub async fn complete_text(
        &self,
        model: &str,
        request: ApiRequest,
        options: TextCompletionOptions,
        tools: Option<Vec<ToolDefinition>>,
        metadata: Option<RequestMetadata>,
    ) -> Result<TextCompletion, RuntimeError> {
        let cache_hints =
            (options.cache_prefix && !request.system_prompt.is_empty()).then(|| CacheHints {
                system_static: Some(request.system_prompt.static_text()),
                system_dynamic: Some(request.system_prompt.dynamic_text()),
                breakpoint_last_message: true,
            });
        let message_request = MessageRequest {
            model: model.into(),
            max_tokens: options.max_tokens,
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.render()),
            tools: if options.include_tools {
                tools.filter(|tools| !tools.is_empty())
            } else {
                None
            },
            stream: false,
            thinking_enabled: false,
            cache_hints,
            metadata,
            ..Default::default()
        };
        let response = self
            .send_message(&message_request, None)
            .await
            .map_err(|error| {
                if error.is_context_window_failure() {
                    RuntimeError::context_window_blocked(error.to_string())
                } else {
                    RuntimeError::new(error.to_string())
                        .with_failure_class(error.safe_failure_class())
                }
            })?;
        Ok(TextCompletion {
            text: response
                .content
                .iter()
                .filter_map(|block| match block {
                    OutputContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
            has_tool_calls: response
                .content
                .iter()
                .any(|block| matches!(block, OutputContentBlock::ToolUse { .. })),
            stop_reason: response.stop_reason,
        })
    }
}

/// Convert active conversation messages to provider input, preserving tool pairs.
#[must_use]
pub fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
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
                    tool_name: _,
                    output,
                    is_error,
                } => {
                    // A tool result is its text, ToolSearch's included.
                    //
                    // This used to append one `tool_reference` block per match
                    // beside that text, which made every request that followed a
                    // ToolSearch fail: `400 invalid_request_error — Tool
                    // definitions/code execution functions cannot be mixed with
                    // other content`. A content array carrying tool definitions
                    // may carry nothing else, so the text and the references
                    // could not both be there, and deferred tools were
                    // unreachable in practice — the model searched, and the turn
                    // after the search died.
                    //
                    // Dropping the references costs nothing, because they were
                    // the second of two mechanisms doing one job.
                    // `extract_discovered_tool_names` reads the same `matches`
                    // out of this text and `core_definitions` clears
                    // `defer_loading` for those names, so the next request
                    // carries their full schemas and the model can call them.
                    // That is the path the deferred-tools prompt section
                    // describes, and keeping the text is what lets the model see
                    // what a keyword search actually matched.
                    let content: Vec<ToolResultContentBlock> = vec![ToolResultContentBlock::Text {
                        text: output.clone(),
                    }];
                    Some(InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content,
                        is_error: *is_error,
                    })
                }
                ContentBlock::Image { data, mime_type } => Some(InputContentBlock::Image {
                    source: crate::ImageSource {
                        source_type: "base64".to_string(),
                        media_type: mime_type.clone(),
                        data: data.clone(),
                    },
                }),
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            continue;
        }

        // Merge consecutive Tool-role messages into the previous user-role
        // InputMessage. Anthropic requires every `tool_use` in an assistant
        // turn to have its matching `tool_result` in the SAME next user
        // message; emitting one user message per tool_result breaks this.
        if matches!(message.role, MessageRole::Tool) {
            if let Some(last) = result.last_mut() {
                if last.role == "user"
                    && last
                        .content
                        .iter()
                        .any(|block| matches!(block, InputContentBlock::ToolResult { .. }))
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
    for message in &mut result {
        // Keep every tool result before its attachments. OpenAI tool messages
        // must answer the whole assistant batch before a user image message.
        message
            .content
            .sort_by_key(|block| !matches!(block, InputContentBlock::ToolResult { .. }));
    }
    result
}
