//! Assemble one whole `MessageResponse` out of a stream of `StreamEvent`s.
//!
//! Every provider needs this, because streaming is the transport even for
//! callers that want a single value. A non-streaming request writes nothing to
//! the socket until generation finishes, and on our path a connection that
//! carries no bytes for ~50 seconds is closed with no HTTP response at all —
//! measured with the same prompt, model and route and only `stream` changed,
//! `stream: false` was killed at 50.3s while `stream: true` produced its first
//! byte in 1.7s and ran to completion in 201.8s. So the slower the answer, the
//! more certainly a non-streaming request fails, which is the opposite of what
//! a timeout should do.
//!
//! This module exists because three providers had already reached that
//! conclusion separately: `codex`, `gemini`, and `openai_compat`'s Responses
//! path each carried their own copy of this loop. The copies had drifted, and
//! the drift was all in the same direction — data silently dropped:
//!
//! - two of the three applied every delta to the *last* block instead of the
//!   block the delta names, so any interleaving appended text to the wrong
//!   block;
//! - none handled `SignatureDelta`, and two dropped `ThinkingDelta`, so a
//!   thinking block collected through them lost its content, its signature, or
//!   both — and an Anthropic thinking block without its signature cannot be
//!   replayed in the next turn's history;
//! - all three overwrote `usage` from `message_delta`, discarding the
//!   `input_tokens` and `cache_read_input_tokens` that only `message_start`
//!   carries — i.e. exactly the numbers that say whether the prompt cache hit;
//! - all three left unparseable tool arguments in place as a JSON *string*,
//!   handing the tool a string where its schema says object and reporting no
//!   error at all.
//!
//! One implementation, one set of semantics, and the semantics are pinned by
//! the tests at the bottom of this file rather than by whichever provider
//! happened to be exercised.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::ApiError;
use crate::types::{ContentBlockDelta, MessageResponse, OutputContentBlock, StreamEvent, Usage};

/// Folds `StreamEvent`s into the response a non-streaming call would return.
///
/// Deliberately synchronous and transport-free: it takes events, not a stream.
/// Every provider's stream type is different, and the interesting behaviour is
/// in the folding, so keeping the two apart is what makes this testable without
/// a network or a mock server.
#[derive(Debug)]
pub struct ResponseAccumulator {
    provider: String,
    model: String,
    id: String,
    /// Keyed by the index the events carry, never by arrival order — a delta
    /// names its block and providers may interleave blocks.
    blocks: BTreeMap<u32, OutputContentBlock>,
    /// Tool arguments arrive as fragments that are only valid JSON once
    /// concatenated, so they are buffered here and parsed at
    /// `content_block_stop`. Keeping them out of the block means a partial
    /// fragment can never be mistaken for a finished argument value.
    tool_input_json: BTreeMap<u32, String>,
    usage: Usage,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    saw_any_event: bool,
}

impl ResponseAccumulator {
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            id: String::new(),
            blocks: BTreeMap::new(),
            tool_input_json: BTreeMap::new(),
            usage: Usage::default(),
            stop_reason: None,
            stop_sequence: None,
            saw_any_event: false,
        }
    }

    /// Whether any event at all has been observed. A caller that has to decide
    /// how to retry needs this: "the stream produced nothing" and "the stream
    /// produced events but no usable content" are different failures with
    /// different correct responses.
    #[must_use]
    pub const fn saw_any_event(&self) -> bool {
        self.saw_any_event
    }

    pub fn push(&mut self, event: StreamEvent) {
        self.saw_any_event = true;
        match event {
            StreamEvent::MessageStart(start) => {
                self.id = start.message.id;
                if !start.message.model.is_empty() {
                    self.model = start.message.model;
                }
                // Anthropic reports `input_tokens`, `cache_creation_input_tokens`
                // and `cache_read_input_tokens` here and nowhere else.
                merge_usage(&mut self.usage, start.message.usage);
                if start.message.stop_reason.is_some() {
                    self.stop_reason = start.message.stop_reason;
                }
                if start.message.stop_sequence.is_some() {
                    self.stop_sequence = start.message.stop_sequence;
                }
                // Anthropic's own streams start with an empty `content`, but an
                // upstream that answered `stream: true` with a whole JSON body
                // carries the entire message here (see `sse::finish`). Dropping
                // it would report a completed, paid-for turn as empty. Indices
                // follow the order the blocks arrived in; a later
                // `content_block_start` for the same index overwrites, which is
                // the right answer for anything that sends both.
                for (index, block) in start.message.content.into_iter().enumerate() {
                    let index = u32::try_from(index).unwrap_or(u32::MAX);
                    self.blocks.insert(index, block);
                }
            }
            StreamEvent::ContentBlockStart(start) => {
                self.blocks.insert(start.index, start.content_block);
            }
            StreamEvent::ContentBlockDelta(event) => {
                self.apply_delta(event.index, event.delta);
            }
            StreamEvent::ContentBlockStop(_) | StreamEvent::MessageStop(_) => {}
            StreamEvent::MessageDelta(event) => {
                if event.delta.stop_reason.is_some() {
                    self.stop_reason = event.delta.stop_reason;
                }
                if event.delta.stop_sequence.is_some() {
                    self.stop_sequence = event.delta.stop_sequence;
                }
                merge_usage(&mut self.usage, event.usage);
            }
        }
    }

    fn apply_delta(&mut self, index: u32, delta: ContentBlockDelta) {
        // Tool arguments are buffered rather than applied, so this arm has to
        // come first: it touches a different field than every other delta.
        if let ContentBlockDelta::InputJsonDelta { partial_json } = &delta {
            // Only for a block we have already seen start. Without the start
            // event there is no tool id or name, so there is nothing to attach
            // the arguments to and inventing a block would invent a tool call.
            if matches!(
                self.blocks.get(&index),
                Some(OutputContentBlock::ToolUse { .. })
            ) {
                self.tool_input_json
                    .entry(index)
                    .or_default()
                    .push_str(partial_json);
            }
            return;
        }

        // A delta for an index we never saw start is still data. Providers that
        // emit deltas without a `content_block_start` exist, and silently
        // dropping their output is worse than inferring the block type from the
        // delta.
        let block = self.blocks.entry(index).or_insert_with(|| match &delta {
            ContentBlockDelta::ThinkingDelta { .. } | ContentBlockDelta::SignatureDelta { .. } => {
                OutputContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                }
            }
            _ => OutputContentBlock::Text {
                text: String::new(),
            },
        });
        match (block, delta) {
            (
                OutputContentBlock::Text { text },
                ContentBlockDelta::TextDelta { text: fragment },
            ) => {
                text.push_str(&fragment);
            }
            (
                OutputContentBlock::Thinking { thinking, .. },
                ContentBlockDelta::ThinkingDelta { thinking: fragment },
            ) => {
                thinking.push_str(&fragment);
            }
            (
                OutputContentBlock::Thinking { signature, .. },
                ContentBlockDelta::SignatureDelta {
                    signature: fragment,
                },
            ) => {
                // Arrives in one piece today, appended anyway: a signature that
                // is silently truncated is indistinguishable from a valid one
                // until the *next* request rejects the replayed thinking block.
                signature
                    .get_or_insert_with(String::new)
                    .push_str(&fragment);
            }
            // Mismatched pairs (a text delta for a tool block, say) are a
            // provider bug we cannot repair here; dropping the fragment is the
            // only option that cannot corrupt a neighbouring block.
            _ => {}
        }
    }

    /// Finish the response, or explain why there isn't one.
    pub fn finish(mut self, request_id: Option<String>) -> Result<MessageResponse, ApiError> {
        if !self.saw_any_event {
            // Returning an empty-but-successful response here is the dangerous
            // alternative, and it is what the per-provider copies did: a
            // compaction that "succeeded" with no summary silently discards the
            // conversation it was meant to preserve. `IncompleteStream` is
            // retryable, which is the correct response to a stream that ended
            // before it began.
            return Err(ApiError::incomplete_stream(
                self.provider.clone(),
                self.model.clone(),
                "stream closed without emitting a single event",
            ));
        }

        for (index, raw) in std::mem::take(&mut self.tool_input_json) {
            let provider = self.provider.clone();
            let model = self.model.clone();
            let Some(OutputContentBlock::ToolUse { input, .. }) = self.blocks.get_mut(&index)
            else {
                continue;
            };
            *input = parse_tool_input(&raw).map_err(|_| {
                // Not swallowed: a tool called with a JSON string where its
                // schema says object fails later, somewhere unrelated, with a
                // message about the schema. Truncated arguments are nearly
                // always a cut-off stream, so this is retryable and names the
                // fragment it choked on.
                ApiError::incomplete_stream(
                    provider,
                    model,
                    &format!("tool_use block {index} has unparseable arguments: {raw}"),
                )
            })?;
        }

        Ok(MessageResponse {
            id: self.id,
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: self.blocks.into_values().collect(),
            model: self.model,
            stop_reason: self.stop_reason,
            stop_sequence: self.stop_sequence,
            usage: self.usage,
            request_id,
            gateway_request_id: None,
        })
    }
}

/// An empty argument buffer means a tool was called with no arguments, which is
/// `{}` and not a parse error — `serde_json` disagrees, so say so here once.
fn parse_tool_input(raw: &str) -> Result<Value, serde_json::Error> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(raw)
}

/// Merge a usage report into the running total, field by field.
///
/// Overwrite-if-non-zero rather than assignment. Providers spread usage across
/// `message_start` (input and cache tokens) and `message_delta` (output
/// tokens), and each event reports a cumulative snapshot of the fields it knows
/// about while leaving the rest at zero. Plain assignment therefore keeps only
/// whichever event came last — which is how the prompt-cache counters went
/// missing from every collected response.
fn merge_usage(total: &mut Usage, incoming: Usage) {
    if incoming.input_tokens > 0 {
        total.input_tokens = incoming.input_tokens;
    }
    if incoming.output_tokens > 0 {
        total.output_tokens = incoming.output_tokens;
    }
    if incoming.cache_creation_input_tokens > 0 {
        total.cache_creation_input_tokens = incoming.cache_creation_input_tokens;
    }
    if incoming.cache_read_input_tokens > 0 {
        total.cache_read_input_tokens = incoming.cache_read_input_tokens;
    }
    if incoming.cost_units.is_some() {
        total.cost_units = incoming.cost_units;
    }
    if incoming.cost_currency.is_some() {
        total.cost_currency = incoming.cost_currency;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{ResponseAccumulator, StreamEvent};
    use crate::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
        MessageDelta, MessageDeltaEvent, MessageResponse, MessageStartEvent, MessageStopEvent,
        OutputContentBlock, Usage,
    };

    fn message_start(usage: Usage) -> StreamEvent {
        StreamEvent::MessageStart(MessageStartEvent {
            message: MessageResponse {
                id: "msg_123".to_string(),
                kind: "message".to_string(),
                role: "assistant".to_string(),
                content: Vec::new(),
                model: "claude-haiku-4-5-20251001".to_string(),
                stop_reason: None,
                stop_sequence: None,
                usage,
                request_id: None,
                gateway_request_id: None,
            },
        })
    }

    fn block_start(index: u32, content_block: OutputContentBlock) -> StreamEvent {
        StreamEvent::ContentBlockStart(ContentBlockStartEvent {
            index,
            content_block,
        })
    }

    fn delta(index: u32, delta: ContentBlockDelta) -> StreamEvent {
        StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent { index, delta })
    }

    fn block_stop(index: u32) -> StreamEvent {
        StreamEvent::ContentBlockStop(ContentBlockStopEvent { index })
    }

    fn text_block() -> OutputContentBlock {
        OutputContentBlock::Text {
            text: String::new(),
        }
    }

    fn tool_block(id: &str, name: &str) -> OutputContentBlock {
        OutputContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: json!({}),
            thought_signature: None,
        }
    }

    fn collect(events: Vec<StreamEvent>) -> Result<MessageResponse, crate::error::ApiError> {
        let mut accumulator = ResponseAccumulator::new("anthropic", "requested-model");
        for event in events {
            accumulator.push(event);
        }
        accumulator.finish(Some("req_1".to_string()))
    }

    /// A `message_start` that already carries the whole message — what
    /// `sse::finish` synthesizes for an upstream that answered `stream: true`
    /// with one JSON body. Real Anthropic streams send an empty `content` here,
    /// so dropping it looked free; on that path it discarded the entire answer.
    #[test]
    fn keeps_content_that_arrives_on_message_start() {
        // given
        let start_with_content = MessageResponse {
            id: "msg_json".to_string(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![
                OutputContentBlock::Text {
                    text: "Done.".to_string(),
                },
                tool_block("toolu_1", "get_weather"),
            ],
            model: "claude-haiku-4-5-20251001".to_string(),
            stop_reason: Some("tool_use".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 40,
                cache_read_input_tokens: 9_000,
                output_tokens: 12,
                ..Usage::default()
            },
            request_id: None,
            gateway_request_id: None,
        };
        let events = vec![
            StreamEvent::MessageStart(MessageStartEvent {
                message: start_with_content,
            }),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("a whole message is a complete stream");

        // then
        assert_eq!(
            response.content,
            vec![
                OutputContentBlock::Text {
                    text: "Done.".to_string(),
                },
                tool_block("toolu_1", "get_weather"),
            ],
        );
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(response.usage.cache_read_input_tokens, 9_000);
    }

    #[test]
    fn collects_text_deltas_into_one_block() {
        // given
        let events = vec![
            message_start(Usage::default()),
            block_start(0, text_block()),
            delta(
                0,
                ContentBlockDelta::TextDelta {
                    text: "Hello ".to_string(),
                },
            ),
            delta(
                0,
                ContentBlockDelta::TextDelta {
                    text: "world".to_string(),
                },
            ),
            block_stop(0),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("a complete stream must collect");

        // then
        assert_eq!(response.id, "msg_123");
        assert_eq!(response.model, "claude-haiku-4-5-20251001");
        assert_eq!(response.request_id.as_deref(), Some("req_1"));
        assert_eq!(
            response.content,
            vec![OutputContentBlock::Text {
                text: "Hello world".to_string()
            }]
        );
    }

    #[test]
    fn deltas_land_on_the_block_they_name_not_the_last_one_started() {
        // given – block 0 keeps streaming after block 1 has started, which is
        // the shape that silently corrupted output when deltas were applied to
        // whichever block arrived last.
        let events = vec![
            message_start(Usage::default()),
            block_start(0, text_block()),
            delta(
                0,
                ContentBlockDelta::TextDelta {
                    text: "first".to_string(),
                },
            ),
            block_start(1, text_block()),
            delta(
                1,
                ContentBlockDelta::TextDelta {
                    text: "second".to_string(),
                },
            ),
            delta(
                0,
                ContentBlockDelta::TextDelta {
                    text: "-again".to_string(),
                },
            ),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("a complete stream must collect");

        // then
        assert_eq!(
            response.content,
            vec![
                OutputContentBlock::Text {
                    text: "first-again".to_string()
                },
                OutputContentBlock::Text {
                    text: "second".to_string()
                },
            ]
        );
    }

    #[test]
    fn keeps_thinking_text_and_signature() {
        // given – a thinking block replayed into the next turn's history without
        // its signature is rejected, so losing the signature is not cosmetic.
        let events = vec![
            message_start(Usage::default()),
            block_start(
                0,
                OutputContentBlock::Thinking {
                    thinking: String::new(),
                    signature: None,
                },
            ),
            delta(
                0,
                ContentBlockDelta::ThinkingDelta {
                    thinking: "step one ".to_string(),
                },
            ),
            delta(
                0,
                ContentBlockDelta::ThinkingDelta {
                    thinking: "step two".to_string(),
                },
            ),
            delta(
                0,
                ContentBlockDelta::SignatureDelta {
                    signature: "sig-abc".to_string(),
                },
            ),
            block_stop(0),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("a complete stream must collect");

        // then
        assert_eq!(
            response.content,
            vec![OutputContentBlock::Thinking {
                thinking: "step one step two".to_string(),
                signature: Some("sig-abc".to_string()),
            }]
        );
    }

    #[test]
    fn parses_tool_arguments_assembled_from_fragments() {
        // given – each fragment is invalid JSON on its own
        let events = vec![
            message_start(Usage::default()),
            block_start(0, tool_block("toolu_1", "bash")),
            delta(
                0,
                ContentBlockDelta::InputJsonDelta {
                    partial_json: "{\"command\":".to_string(),
                },
            ),
            delta(
                0,
                ContentBlockDelta::InputJsonDelta {
                    partial_json: "\"ls -la\"}".to_string(),
                },
            ),
            block_stop(0),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("a complete stream must collect");

        // then
        assert_eq!(
            response.content,
            vec![OutputContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "bash".to_string(),
                input: json!({ "command": "ls -la" }),
                thought_signature: None,
            }]
        );
    }

    #[test]
    fn a_tool_called_with_no_arguments_gets_an_empty_object() {
        // given – providers send a single empty fragment for a no-arg tool
        let events = vec![
            message_start(Usage::default()),
            block_start(0, tool_block("toolu_1", "list_files")),
            delta(
                0,
                ContentBlockDelta::InputJsonDelta {
                    partial_json: String::new(),
                },
            ),
            block_stop(0),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("an empty argument list is not an error");

        // then
        assert_eq!(
            response.content,
            vec![OutputContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "list_files".to_string(),
                input: json!({}),
                thought_signature: None,
            }]
        );
    }

    #[test]
    fn unparseable_tool_arguments_are_reported_not_passed_through() {
        // given – a stream cut off mid-argument
        let events = vec![
            message_start(Usage::default()),
            block_start(0, tool_block("toolu_1", "bash")),
            delta(
                0,
                ContentBlockDelta::InputJsonDelta {
                    partial_json: "{\"command\":\"ls -l".to_string(),
                },
            ),
            block_stop(0),
        ];

        // when
        let error = collect(events).expect_err("truncated arguments must not look like success");

        // then – retryable, and it names the fragment rather than blaming a schema
        assert!(
            matches!(error, crate::error::ApiError::IncompleteStream { .. }),
            "expected IncompleteStream, got: {error:?}"
        );
        assert!(error.is_retryable(), "a cut-off stream is worth retrying");
    }

    #[test]
    fn usage_merges_input_tokens_from_start_with_output_tokens_from_delta() {
        // given – the split that made cache counters vanish from collected
        // responses: only `message_start` carries the cache fields.
        let events = vec![
            message_start(Usage {
                input_tokens: 1200,
                cache_read_input_tokens: 30_000,
                cache_creation_input_tokens: 400,
                ..Usage::default()
            }),
            block_start(0, text_block()),
            delta(
                0,
                ContentBlockDelta::TextDelta {
                    text: "ok".to_string(),
                },
            ),
            StreamEvent::MessageDelta(MessageDeltaEvent {
                delta: MessageDelta {
                    stop_reason: Some("end_turn".to_string()),
                    stop_sequence: None,
                },
                usage: Usage {
                    output_tokens: 7,
                    ..Usage::default()
                },
            }),
            StreamEvent::MessageStop(MessageStopEvent {}),
        ];

        // when
        let response = collect(events).expect("a complete stream must collect");

        // then
        assert_eq!(response.usage.input_tokens, 1200);
        assert_eq!(response.usage.cache_read_input_tokens, 30_000);
        assert_eq!(response.usage.cache_creation_input_tokens, 400);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn a_stream_that_emits_nothing_is_an_error_not_an_empty_answer() {
        // given / when
        let error = collect(Vec::new())
            .expect_err("an empty stream must not collect into a successful empty response");

        // then
        assert!(
            matches!(error, crate::error::ApiError::IncompleteStream { .. }),
            "expected IncompleteStream, got: {error:?}"
        );
    }

    #[test]
    fn saw_any_event_distinguishes_silence_from_unusable_output() {
        // given
        let mut accumulator = ResponseAccumulator::new("anthropic", "model");

        // then – before anything arrives
        assert!(!accumulator.saw_any_event());

        // when
        accumulator.push(message_start(Usage::default()));

        // then
        assert!(accumulator.saw_any_event());
    }

    #[test]
    fn falls_back_to_the_requested_model_when_the_stream_does_not_name_one() {
        // given – a provider whose message_start omits the model
        let events = vec![
            message_start(Usage::default()),
            block_start(0, text_block()),
        ];
        let mut accumulator = ResponseAccumulator::new("gemini", "gemini-3.1-pro-preview");
        for event in events {
            match event {
                StreamEvent::MessageStart(mut start) => {
                    start.message.model = String::new();
                    accumulator.push(StreamEvent::MessageStart(start));
                }
                other => accumulator.push(other),
            }
        }

        // when
        let response = accumulator.finish(None).expect("must collect");

        // then
        assert_eq!(response.model, "gemini-3.1-pro-preview");
    }
}
