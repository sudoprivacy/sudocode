//! Shared text transport for main-agent and subagent clients.
//! Summarization prompts, retries, and checkpoint validation stay in runtime.
//!
//! Streamed, despite returning one whole value. This is the path compaction
//! takes, so its requests carry the entire conversation and are the slowest
//! and largest we ever send — and a non-streaming request writes nothing to
//! the socket until generation finishes, which on our path gets the connection
//! closed at ~50s with no HTTP response to explain it (`stream: false` died at
//! 50.3s where the identical `stream: true` request had its first byte at 1.7s
//! and finished at 201.8s). The symptom was
//! `Context compaction failed: compaction API error: api failed after 9
//! attempts`, where all nine attempts were the same doomed shape.

use crate::{
    CacheHints, InputContentBlock, InputMessage, MessageRequest, OutputContentBlock,
    ProviderClient, SessionRequestFields, ToolDefinition, ToolResultContentBlock,
};
use runtime::{
    ApiRequest, ContentBlock, ConversationMessage, MessageRole, RuntimeError, TextCompletion,
    TextCompletionOptions,
};

/// Build the shared request prefix for turns, subagents, and compaction.
///
/// Operation-specific changes (tool choice or fallback model) are applied by
/// the caller after this builder has copied the session fields.
#[inline]
#[must_use]
pub fn session_message_request(
    model: &str,
    request: &ApiRequest,
    options: TextCompletionOptions,
    tools: Option<Vec<ToolDefinition>>,
    session: SessionRequestFields,
) -> MessageRequest {
    let cache_hints =
        (options.cache_prefix && !request.system_prompt.is_empty()).then(|| CacheHints {
            system_static: Some(request.system_prompt.static_text()),
            system_dynamic: Some(request.system_prompt.dynamic_text()),
            breakpoint_last_message: true,
        });
    MessageRequest {
        model: model.into(),
        max_tokens: options.max_tokens,
        messages: convert_messages(&request.messages),
        system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.render()),
        tools: if options.include_tools {
            tools.filter(|tools| !tools.is_empty())
        } else {
            None
        },
        stream: true,
        thinking_enabled: options.thinking_enabled,
        reasoning_effort: session.reasoning_effort,
        cache_hints,
        metadata: session.metadata,
        ..Default::default()
    }
}

impl ProviderClient {
    /// Build and send a text-only request with caller-selected model and schemas.
    /// Return completion metadata unchanged so the consumer can validate it.
    ///
    /// `session` is the caller's session-level state, and it is a parameter
    /// rather than something this function could derive because this transport
    /// has no session of its own — it serves whichever client calls it. Both of
    /// its fields are load-bearing for the cache. Omitting the routing key is
    /// the expensive case: compaction goes through here carrying the entire
    /// conversation, so a compaction request that routes to a different
    /// upstream account than the turns around it pays a full cold write for the
    /// whole history, twice — once here, once when the next turn lands back on
    /// the original account.
    pub async fn complete_text(
        &self,
        model: &str,
        request: ApiRequest,
        options: TextCompletionOptions,
        tools: Option<Vec<ToolDefinition>>,
        session: SessionRequestFields,
    ) -> Result<TextCompletion, RuntimeError> {
        let message_request = session_message_request(model, &request, options, tools, session);
        let response = self
            .send_message_streamed(&message_request, None)
            .await
            .map_err(|error| {
                if error.is_context_window_failure() {
                    RuntimeError::context_window_blocked(error.to_string())
                } else {
                    RuntimeError::new(error.to_string())
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
    let history = runtime::model_tool_history(messages);
    let messages = history.as_ref();
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
                // Send a signed thinking block back. Dropping it is what a
                // client is tempted to do — the block is not addressed to the
                // user and costs bytes — but the assistant turn we replay then
                // differs from the one the server produced, and on Anthropic
                // that invalidates the cached prefix *from the start*, not from
                // the assistant turn. Measured on a live route, same shape, one
                // factor changed at a time (`ladder/tools/cache_prefix_probe.py`):
                //
                //   thinking off .............. turn 2 read 6275 / write 74
                //   thinking on, block dropped  turn 2 read    0 / write 6351
                //   thinking on, block returned turn 2 read 6315 / write 109
                //
                // So with thinking enabled, dropping the block re-wrote the
                // whole prefix on *every* tool round-trip — a cold write (1.25x)
                // in place of a read (0.1x), which is where an agentic session's
                // cache_creation share comes from.
                //
                // Unsigned blocks are dropped instead of sent: they are accepted
                // (no 400) but buy nothing — the same probe measured read 0 with
                // the signature stripped, because the server cannot validate the
                // block and does not count it as the turn it issued. A route that
                // strips signatures therefore degrades to today's behaviour
                // rather than sending junk the provider has to reason about.
                //
                // Providers that have no thinking channel already ignore this
                // variant (gemini, codex); the OpenAI-compatible provider maps it
                // to a reasoning item or `reasoning_content` — code that was
                // written for these blocks and was unreachable until now.
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => signature
                    .as_ref()
                    .map(|signature| InputContentBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: Some(signature.clone()),
                    }),
                // Encrypted thinking needs no signature check — the ciphertext
                // is its own proof — so it goes back unconditionally. Same
                // probe, trigger string in the prompt: dropped it gave turn 2
                // `read 0 / write 3121`, replayed it gave `read 3083 / write
                // 109`. `data` is stored as JSON text; a payload that is not
                // valid JSON can only have been a bare string.
                ContentBlock::RedactedThinking { data } => {
                    Some(InputContentBlock::RedactedThinking {
                        data: serde_json::from_str(data)
                            .unwrap_or_else(|_| serde_json::Value::String(data.clone())),
                    })
                }
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

#[cfg(test)]
mod tests {
    use super::{convert_messages, session_message_request};
    use crate::{
        InputContentBlock, MessageRequest, RequestMetadata, SessionRequestFields, ToolDefinition,
    };
    use runtime::{
        ApiRequest, ContentBlock, ConversationMessage, MessageRole, SystemPrompt,
        TextCompletionOptions,
    };

    fn assistant(blocks: Vec<ContentBlock>) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::Assistant,
            blocks,
            usage: None,
            model: None,
            duration_ms: None,
        }
    }

    fn tool_use() -> ContentBlock {
        ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "Read".to_string(),
            input: "{\"path\":\"a.txt\"}".to_string(),
            thought_signature: None,
        }
    }

    fn tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "Read".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            defer_loading: false,
        }
    }

    /// Every field the turn stream fills from session state has to be restated
    /// by [`session_message_request`] or it silently falls back to the type's
    /// default — and on Anthropic a request parameter that differs from the one
    /// the prefix was cached under rewrites the whole prefix and still returns
    /// 200. Three measured regressions came from that gap, so this test exists
    /// to make the next one a compile error rather than a quiet bill.
    #[test]
    fn cache_safe_request_mirrors_the_stream() {
        let mut system_prompt = SystemPrompt::default();
        system_prompt.append_static_section("be terse".to_string());
        let request = ApiRequest {
            system_prompt,
            messages: vec![assistant(vec![ContentBlock::Text {
                text: "hi".to_string(),
            }])],
            trace_id: None,
            pre_compact_discovered_tools: std::collections::BTreeSet::new(),
        };
        let options = TextCompletionOptions {
            max_tokens: 12_000,
            include_tools: true,
            cache_prefix: true,
            thinking_enabled: true,
        };
        let session = SessionRequestFields {
            metadata: Some(RequestMetadata::for_session("session-1")),
            reasoning_effort: Some("high".to_string()),
        };

        let built = session_message_request(
            "claude-sonnet-4-6",
            &request,
            options,
            Some(vec![tool_definition()]),
            session.clone(),
        );

        // Destructured exhaustively on purpose: a new field on `MessageRequest`
        // breaks this line, and whoever adds it has to classify it here —
        // mirrored from the session, derived from the request, or deliberately
        // left at the provider default.
        let MessageRequest {
            model,
            max_tokens,
            messages,
            system,
            tools,
            tool_choice,
            stream,
            temperature,
            top_p,
            frequency_penalty,
            presence_penalty,
            stop,
            reasoning_effort,
            cache_hints,
            thinking_enabled,
            metadata,
        } = built;

        // Mirrored from the session: these are the cache-key fields.
        assert_eq!(reasoning_effort, session.reasoning_effort);
        assert_eq!(metadata, session.metadata);
        assert!(thinking_enabled);

        // Derived from this call's arguments.
        assert_eq!(model, "claude-sonnet-4-6");
        assert_eq!(max_tokens, 12_000);
        assert_eq!(messages.len(), 1);
        assert_eq!(system.as_deref(), Some("be terse"));
        assert_eq!(tools.map(|tools| tools.len()), Some(1));
        assert!(stream, "a non-streaming request of this size dies at ~50s");
        assert!(cache_hints.is_some(), "this path exists to reuse a prefix");

        // Deliberately left at the provider default. `tool_choice` is the one
        // worth a note: the stream sends `auto` and this path sends nothing,
        // which looks like the same bug as the three above. It is not —
        // measured on a live route, dropping `tool_choice` on the second of two
        // otherwise identical requests read the full prefix (2985 / 2982 / 2979
        // against a control of 2998 / 2985, with a negative control in the same
        // shape reading 0), and Anthropic's default with tools present is `auto`
        // anyway. Matching it would be churn, not a fix.
        assert!(tool_choice.is_none());
        assert!(temperature.is_none());
        assert!(top_p.is_none());
        assert!(frequency_penalty.is_none());
        assert!(presence_penalty.is_none());
        assert!(stop.is_none());
    }

    #[test]
    fn signed_thinking_block_is_replayed_ahead_of_its_tool_use() {
        let converted = convert_messages(&[assistant(vec![
            ContentBlock::Thinking {
                thinking: "weighing the options".to_string(),
                signature: Some("sig-abc".to_string()),
            },
            tool_use(),
        ])]);

        // The unfinished tool call also receives a cancellation result.
        assert_eq!(converted.len(), 2);
        // Order matters as much as presence: the API takes the thinking block
        // only as the first block of the assistant turn that produced it.
        match &converted[0].content[..] {
            [InputContentBlock::Thinking {
                thinking,
                signature,
            }, InputContentBlock::ToolUse { .. }] => {
                assert_eq!(thinking, "weighing the options");
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("expected a signed thinking block then the tool use, got {other:?}"),
        }
    }

    #[test]
    fn unsigned_thinking_block_is_dropped() {
        // An unsigned block is accepted by the API but counts for nothing — the
        // server cannot verify it, so the prefix is rebuilt anyway. Sending it
        // would only add tokens to a request that is already paying for a miss.
        let converted = convert_messages(&[assistant(vec![
            ContentBlock::Thinking {
                thinking: "unverifiable".to_string(),
                signature: None,
            },
            tool_use(),
        ])]);

        // The unfinished tool call also receives a cancellation result.
        assert_eq!(converted.len(), 2);
        assert!(
            matches!(
                converted[0].content[..],
                [InputContentBlock::ToolUse { .. }]
            ),
            "an unsigned thinking block must not reach the wire: {:?}",
            converted[0].content
        );
    }

    #[test]
    fn an_assistant_turn_of_only_unsigned_thinking_is_not_sent_as_an_empty_message() {
        // Dropping the only block would otherwise leave an empty content array,
        // which the API rejects outright.
        let converted = convert_messages(&[assistant(vec![ContentBlock::Thinking {
            thinking: "unverifiable".to_string(),
            signature: None,
        }])]);

        assert!(
            converted.is_empty(),
            "a message whose blocks were all dropped must be skipped: {converted:?}"
        );
    }

    #[test]
    fn redacted_thinking_block_is_replayed_with_its_payload_intact() {
        // No signature to check — the ciphertext is its own proof — so unlike an
        // unsigned thinking block this one always goes back. Dropping it cost
        // the whole prefix on every round-trip: measured turn 2 `read 0 / write
        // 3121` dropped versus `read 3083 / write 109` replayed.
        let converted = convert_messages(&[assistant(vec![
            ContentBlock::RedactedThinking {
                data: "\"opaque-ciphertext\"".to_string(),
            },
            tool_use(),
        ])]);

        // The unfinished tool call also receives a cancellation result.
        assert_eq!(converted.len(), 2);
        match &converted[0].content[..] {
            [InputContentBlock::RedactedThinking { data }, InputContentBlock::ToolUse { .. }] => {
                assert_eq!(data, &serde_json::json!("opaque-ciphertext"));
            }
            other => panic!("expected the redacted block then the tool use, got {other:?}"),
        }
    }

    #[test]
    fn a_redacted_payload_that_is_not_json_is_sent_as_a_string() {
        // Defensive: the stored form is JSON text, but a provider that handed us
        // a bare token must not turn into a dropped block.
        let converted = convert_messages(&[assistant(vec![
            ContentBlock::RedactedThinking {
                data: "not-json-at-all".to_string(),
            },
            tool_use(),
        ])]);

        match &converted[0].content[..] {
            [InputContentBlock::RedactedThinking { data }, InputContentBlock::ToolUse { .. }] => {
                assert_eq!(data, &serde_json::json!("not-json-at-all"));
            }
            other => panic!("expected the redacted block then the tool use, got {other:?}"),
        }
    }
}
