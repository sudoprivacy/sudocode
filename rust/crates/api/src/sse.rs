use crate::error::ApiError;
use crate::types::{
    MessageDelta, MessageDeltaEvent, MessageResponse, MessageStartEvent, MessageStopEvent,
    StreamEvent,
};

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    provider: Option<String>,
    model: Option<String>,
}

impl SseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the provider name and model to this parser so that JSON
    /// deserialization failures within streamed frames carry enough context
    /// for callers to understand which upstream produced the unparseable
    /// payload.
    #[must_use]
    pub fn with_context(mut self, provider: impl Into<String>, model: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self.model = Some(model.into());
        self
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<StreamEvent>, ApiError> {
        self.buffer.extend_from_slice(chunk);
        if body_opens_as_json(&self.buffer) {
            // Not an SSE stream at all — an upstream that ignored
            // `stream: true` and is answering with one JSON object. Frame
            // splitting must not run over it: a blank line anywhere inside a
            // pretty-printed body would be read as a frame terminator and cut
            // the object in half. Hold everything for `finish`, which knows
            // how to read a whole body.
            return Ok(Vec::new());
        }
        let mut events = Vec::new();

        while let Some(frame) = self.next_frame() {
            if let Some(event) = self.parse_frame_with_context(&frame)? {
                events.push(event);
            }
        }

        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ApiError> {
        if self.buffer.is_empty() {
            return Ok(Vec::new());
        }

        let trailing = std::mem::take(&mut self.buffer);
        let tail = String::from_utf8_lossy(&trailing);
        // A non-empty buffer at end-of-stream is a frame that never got its
        // `\n\n` terminator. Some servers legitimately omit the final blank
        // line, so a clean parse must still succeed. But when the tail cannot
        // be parsed it is a truncation, not a malformed model payload: report
        // it as a retryable IncompleteStream carrying the tail, never as a
        // JSON "failed to parse ... for model X" error that blames the model.
        match self.parse_frame_with_context(&tail) {
            Ok(Some(event)) => Ok(vec![event]),
            // Nothing SSE-shaped and no error envelope. Before giving up on
            // the bytes, check whether they are a complete message the
            // upstream sent unframed.
            Ok(None) => Ok(non_sse_message_events(tail.trim()).unwrap_or_default()),
            // The tail did not parse. If it is the *start* of a terminal frame
            // (a `message_stop` — the last event Anthropic emits), the stream
            // reached its logical end and only the closing `}\n\n` bytes were
            // dropped by a proxy cutting the connection. That is a complete
            // message, not a truncated one: swallow it rather than forcing a
            // wasteful whole-turn retry (or a retry loop against a proxy that
            // keeps truncating the same terminal frame).
            Err(_) if tail_began_terminal_frame(&tail) => Ok(Vec::new()),
            Err(_) => Err(ApiError::incomplete_stream(
                self.provider.as_deref().unwrap_or("unknown"),
                self.model.as_deref().unwrap_or("unknown"),
                &tail,
            )),
        }
    }

    fn parse_frame_with_context(&self, frame: &str) -> Result<Option<StreamEvent>, ApiError> {
        let provider = self.provider.as_deref().unwrap_or("unknown");
        let model = self.model.as_deref().unwrap_or("unknown");
        parse_frame_with_provider(frame, provider, model)
    }

    fn next_frame(&mut self) -> Option<String> {
        let separator = self
            .buffer
            .windows(2)
            .position(|window| window == b"\n\n")
            .map(|position| (position, 2))
            .or_else(|| {
                self.buffer
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|position| (position, 4))
            })?;

        let (position, separator_len) = separator;
        let frame = self
            .buffer
            .drain(..position + separator_len)
            .collect::<Vec<_>>();
        let frame_len = frame.len().saturating_sub(separator_len);
        Some(String::from_utf8_lossy(&frame[..frame_len]).into_owned())
    }
}

pub fn parse_frame(frame: &str) -> Result<Option<StreamEvent>, ApiError> {
    parse_frame_with_provider(frame, "unknown", "unknown")
}

/// Whether the body starts with a JSON value rather than SSE framing.
///
/// An SSE stream's first non-whitespace bytes are always a field name
/// (`event:`, `data:`) or a comment (`:`), never `{` or `[`.
fn body_opens_as_json(buffer: &[u8]) -> bool {
    matches!(
        buffer.iter().find(|byte| !byte.is_ascii_whitespace()),
        Some(b'{' | b'[')
    )
}

/// Turn a complete `/v1/messages` JSON body into the events a stream would
/// have produced, for an upstream that ignored `stream: true` and answered
/// with the whole object.
///
/// The answer is already generated and already paid for. Dropping it — which
/// is what happened before, since the body matches no SSE frame — reported an
/// empty response to the caller, whose only recourse was to re-send the entire
/// conversation: a second upload, a second generation, and on this path in the
/// one request shape that a ~50s byte-quiet close kills.
///
/// Emits `message_start` (carrying the content, so consumers that read blocks
/// from it need no special case), then `message_delta` for the stop reason and
/// usage, then `message_stop` — so a consumer counting a completed message
/// sees one.
fn non_sse_message_events(trimmed: &str) -> Option<Vec<StreamEvent>> {
    let raw = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
    // Require the discriminator rather than relying on a permissive
    // deserialize: every field of `MessageResponse` except `content` has a
    // default, so some unrelated JSON object would otherwise be accepted as an
    // empty assistant message and reported as a successful empty turn.
    if raw.get("type").and_then(serde_json::Value::as_str) != Some("message") {
        return None;
    }
    let message = serde_json::from_value::<MessageResponse>(raw).ok()?;
    let delta = MessageDelta {
        stop_reason: message.stop_reason.clone(),
        stop_sequence: message.stop_sequence.clone(),
    };
    let usage = message.usage.clone();
    Some(vec![
        StreamEvent::MessageStart(MessageStartEvent { message }),
        StreamEvent::MessageDelta(MessageDeltaEvent { delta, usage }),
        StreamEvent::MessageStop(MessageStopEvent {}),
    ])
}

/// Whether an unparseable trailing frame is the *beginning* of a terminal
/// event — the last frame Anthropic emits (`message_stop`). Used by
/// [`SseParser::finish`] to distinguish a proxy dropping the closing bytes of
/// the final frame (the message is complete) from a mid-content truncation
/// (retryable). Matches on the SSE `event:` line and, as a fallback, the
/// leading `"type":"message_stop"` of the partial `data:` JSON, so it works
/// whether the cut landed before or after the event line.
fn tail_began_terminal_frame(tail: &str) -> bool {
    for line in tail.trim().lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("event:") {
            if name.trim() == "message_stop" {
                return true;
            }
        }
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            // The JSON may be cut anywhere after the type; a prefix match is
            // enough to recognize the terminal event.
            if data.starts_with("{\"type\":\"message_stop\"")
                || data.starts_with("{ \"type\": \"message_stop\"")
            {
                return true;
            }
        }
    }
    false
}

pub(crate) fn parse_frame_with_provider(
    frame: &str,
    provider: &str,
    model: &str,
) -> Result<Option<StreamEvent>, ApiError> {
    let trimmed = frame.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut data_lines = Vec::new();
    let mut event_name: Option<&str> = None;

    for line in trimmed.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            event_name = Some(name.trim());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.trim_start());
        }
    }

    if matches!(event_name, Some("ping")) {
        return Ok(None);
    }

    if data_lines.is_empty() {
        // No `data:` lines at all. If the frame is not even SSE-shaped the
        // server sent something else entirely (an HTML error page from a
        // proxy, or a bare JSON error body). Surface it instead of silently
        // dropping it — otherwise the user sees an empty response with no
        // hint of what went wrong.
        if event_name.is_none() {
            if let Some(error) = detect_non_sse_error(trimmed) {
                return Err(error);
            }
        }
        return Ok(None);
    }

    let payload = data_lines.join("\n");
    if payload == "[DONE]" {
        return Ok(None);
    }

    serde_json::from_str::<StreamEvent>(&payload)
        .map(Some)
        .map_err(|error| ApiError::json_deserialize(provider, model, &payload, error))
}

/// Detect a response body that is not an SSE stream at all: an HTML error
/// page (misconfigured endpoint, proxy outage page) or a bare JSON error
/// envelope sent without SSE framing. Returns an error carrying a short body
/// snippet so the failure is visible to the user; returns `None` for
/// anything that still looks like benign SSE noise (comments, keep-alives).
pub(crate) fn detect_non_sse_error(trimmed: &str) -> Option<ApiError> {
    if trimmed.starts_with('<') {
        let snippet = crate::error::truncate_body_snippet(trimmed, 200);
        return Some(ApiError::Api {
            status: reqwest::StatusCode::BAD_GATEWAY,
            error_type: Some("invalid_response".to_string()),
            message: Some(format!(
                "provider returned HTML instead of an SSE stream (check the endpoint URL): {snippet}"
            )),
            request_id: None,
            body: snippet,
            retryable: false,
            suggested_action: Some("verify the API endpoint URL is correct".to_string()),
            retry_after: None,
        });
    }

    let raw = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
    let err_obj = raw.get("error")?;
    let message = err_obj
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("provider returned an error instead of an SSE stream")
        .to_string();
    let status = err_obj
        .get("code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .and_then(|code| reqwest::StatusCode::from_u16(code).ok())
        .unwrap_or(reqwest::StatusCode::BAD_GATEWAY);
    Some(ApiError::Api {
        status,
        error_type: err_obj
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        message: Some(message),
        request_id: None,
        body: crate::error::truncate_body_snippet(trimmed, 500),
        retryable: false,
        suggested_action: None,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_frame, SseParser};
    use crate::error::ApiError;
    use crate::types::{ContentBlockDelta, MessageDelta, OutputContentBlock, StreamEvent, Usage};

    /// An upstream that ignores `stream: true` and answers with the whole
    /// message body. The answer is complete and already paid for; matching no
    /// frame and reporting nothing left the caller re-sending the entire
    /// conversation to get a reply it had already received.
    #[test]
    fn a_whole_message_body_sent_instead_of_a_stream_becomes_events() {
        let body = concat!(
            "{\"id\":\"msg_json\",\"type\":\"message\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"text\",\"text\":\"Hello\"}],",
            "\"model\":\"claude-sonnet-4-6\",\"stop_reason\":\"end_turn\",\"stop_sequence\":null,",
            "\"usage\":{\"input_tokens\":11,\"cache_read_input_tokens\":7,\"output_tokens\":3}}"
        );

        let mut parser = SseParser::new();
        assert!(
            parser
                .push(body.as_bytes())
                .expect("a whole body is not an error")
                .is_empty(),
            "an unframed body cannot be read until the stream ends"
        );
        let events = parser.finish().expect("a complete body should parse");

        assert_eq!(events.len(), 3, "{events:?}");
        match &events[0] {
            StreamEvent::MessageStart(start) => {
                assert_eq!(start.message.id, "msg_json");
                assert_eq!(
                    start.message.content,
                    vec![OutputContentBlock::Text {
                        text: "Hello".to_string(),
                    }]
                );
                assert_eq!(start.message.usage.cache_read_input_tokens, 7);
            }
            other => panic!("expected message_start, got {other:?}"),
        }
        match &events[1] {
            StreamEvent::MessageDelta(delta) => {
                assert_eq!(delta.delta.stop_reason.as_deref(), Some("end_turn"));
                assert_eq!(delta.usage.input_tokens, 11);
            }
            other => panic!("expected message_delta, got {other:?}"),
        }
        assert!(
            matches!(events[2], StreamEvent::MessageStop(_)),
            "a consumer waiting for message_stop has to see one: {:?}",
            events[2]
        );
    }

    /// A blank line inside a pretty-printed body is not a frame terminator.
    /// Splitting on it would cut the object in half and lose the answer.
    #[test]
    fn a_pretty_printed_body_is_not_shredded_by_frame_splitting() {
        let body = concat!(
            "{\n  \"id\": \"msg_pretty\",\n  \"type\": \"message\",\n\n",
            "  \"role\": \"assistant\",\n",
            "  \"content\": [{\"type\": \"text\", \"text\": \"Hi\"}],\n",
            "  \"model\": \"m\",\n  \"usage\": {\"output_tokens\": 2}\n}"
        );
        let (head, tail) = body.split_at(40);

        let mut parser = SseParser::new();
        assert!(parser.push(head.as_bytes()).expect("head").is_empty());
        assert!(parser.push(tail.as_bytes()).expect("tail").is_empty());
        let events = parser.finish().expect("a complete body should parse");

        assert_eq!(events.len(), 3, "{events:?}");
        match &events[0] {
            StreamEvent::MessageStart(start) => assert_eq!(
                start.message.content,
                vec![OutputContentBlock::Text {
                    text: "Hi".to_string(),
                }]
            ),
            other => panic!("expected message_start, got {other:?}"),
        }
    }

    /// Holding an unframed body back until the stream ends must not swallow an
    /// error body: reporting that as an empty success is how a failed turn
    /// becomes a silently empty answer.
    #[test]
    fn an_unframed_error_envelope_is_still_reported_as_an_error() {
        let body =
            "{\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}";
        let mut parser = SseParser::new();

        let error = match parser.push(body.as_bytes()) {
            Err(error) => error,
            Ok(events) => {
                assert!(events.is_empty(), "{events:?}");
                parser
                    .finish()
                    .expect_err("an error body must not read as a successful empty turn")
            }
        };

        assert!(
            format!("{error}").contains("Overloaded"),
            "the error has to name the upstream's reason: {error:?}"
        );
    }

    /// Only a body that says it is a message becomes one. Every field of a
    /// message response except `content` has a serde default, so a permissive
    /// parse would turn unrelated JSON into a successful empty turn.
    #[test]
    fn an_unrelated_json_object_does_not_become_an_empty_message() {
        let mut parser = SseParser::new();
        parser
            .push(b"{\"detail\":\"not found\"}")
            .expect("push should not error");

        assert!(
            parser
                .finish()
                .expect("no error envelope, no message")
                .is_empty(),
            "an unrecognized body must stay unrecognized, not be invented into a reply"
        );
    }

    #[test]
    fn parses_single_frame() {
        let frame = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"Hi\"}}\n\n"
        );

        let event = parse_frame(frame).expect("frame should parse");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockStart(
                crate::types::ContentBlockStartEvent {
                    index: 0,
                    content_block: OutputContentBlock::Text {
                        text: "Hi".to_string(),
                    },
                },
            ))
        );
    }

    #[test]
    fn parses_chunked_stream() {
        let mut parser = SseParser::new();
        let first = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel";
        let second = b"lo\"}}\n\n";

        assert!(parser
            .push(first)
            .expect("first chunk should buffer")
            .is_empty());
        let events = parser.push(second).expect("second chunk should parse");

        assert_eq!(
            events,
            vec![StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::TextDelta {
                        text: "Hello".to_string(),
                    },
                }
            )]
        );
    }

    #[test]
    fn ignores_ping_and_done() {
        let mut parser = SseParser::new();
        let payload = concat!(
            ": keepalive\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n\n"
        );

        let events = parser
            .push(payload.as_bytes())
            .expect("parser should succeed");
        assert_eq!(
            events,
            vec![
                StreamEvent::MessageDelta(crate::types::MessageDeltaEvent {
                    delta: MessageDelta {
                        stop_reason: Some("tool_use".to_string()),
                        stop_sequence: None,
                    },
                    usage: Usage {
                        input_tokens: 1,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        output_tokens: 2,
                        ..Usage::default()
                    },
                }),
                StreamEvent::MessageStop(crate::types::MessageStopEvent {}),
            ]
        );
    }

    #[test]
    fn ignores_data_less_event_frames() {
        let frame = "event: ping\n\n";
        let event = parse_frame(frame).expect("frame without data should be ignored");
        assert_eq!(event, None);
    }

    #[test]
    fn parses_split_json_across_data_lines() {
        let frame = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\n",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n"
        );

        let event = parse_frame(frame).expect("frame should parse");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::TextDelta {
                        text: "Hello".to_string(),
                    },
                }
            ))
        );
    }

    #[test]
    fn parses_thinking_content_block_start() {
        let frame = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":null}}\n\n"
        );

        let event = parse_frame(frame).expect("frame should parse");
        assert_eq!(
            event,
            Some(StreamEvent::ContentBlockStart(
                crate::types::ContentBlockStartEvent {
                    index: 0,
                    content_block: OutputContentBlock::Thinking {
                        thinking: String::new(),
                        signature: None,
                    },
                },
            ))
        );
    }

    #[test]
    fn parses_thinking_related_deltas() {
        let thinking = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"step 1\"}}\n\n"
        );
        let signature = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_123\"}}\n\n"
        );

        let thinking_event = parse_frame(thinking).expect("thinking delta should parse");
        let signature_event = parse_frame(signature).expect("signature delta should parse");

        assert_eq!(
            thinking_event,
            Some(StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::ThinkingDelta {
                        thinking: "step 1".to_string(),
                    },
                }
            ))
        );
        assert_eq!(
            signature_event,
            Some(StreamEvent::ContentBlockDelta(
                crate::types::ContentBlockDeltaEvent {
                    index: 0,
                    delta: ContentBlockDelta::SignatureDelta {
                        signature: "sig_123".to_string(),
                    },
                }
            ))
        );
    }

    #[test]
    fn given_message_delta_frame_with_empty_usage_when_parsed_then_usage_defaults_to_zero() {
        // given
        let frame = concat!(
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{}}\n\n"
        );

        // when
        let event = parse_frame(frame).expect("frame should parse");

        // then
        assert_eq!(
            event,
            Some(StreamEvent::MessageDelta(crate::types::MessageDeltaEvent {
                delta: MessageDelta {
                    stop_reason: Some("end_turn".to_string()),
                    stop_sequence: None,
                },
                usage: Usage::default(),
            }))
        );
    }

    #[test]
    fn finish_on_truncated_frame_reports_incomplete_stream_not_json_error() {
        // A stream that ends mid-frame (no terminating blank line, partial
        // JSON) is a transport truncation. finish() must surface it as a
        // retryable IncompleteStream naming the truncation — never as a JSON
        // parse error "for model X" that blames the model.
        let mut parser = SseParser::new().with_context("anthropic", "claude-opus-5");
        let partial = b"event: content_block_delta\ndata: {\"type\":\"content_block_del";
        assert!(parser.push(partial).expect("partial buffers").is_empty());

        let err = parser.finish().expect_err("truncated tail must error");
        assert!(
            matches!(err, ApiError::IncompleteStream { .. }),
            "expected IncompleteStream, got: {err:?}"
        );
        assert!(err.is_retryable(), "a truncated stream must be retryable");
        let rendered = err.to_string();
        assert!(
            rendered.contains("ended mid-frame") && rendered.contains("claude-opus-5"),
            "message should name the truncation and model: {rendered}"
        );
        assert!(
            !rendered.contains("failed to parse"),
            "must not render as a JSON parse failure: {rendered}"
        );
    }

    #[test]
    fn finish_parses_a_final_frame_missing_its_trailing_blank_line() {
        // Some servers omit the final `\n\n`. A complete-but-unterminated last
        // frame must still parse cleanly (not be misclassified as truncated).
        let mut parser = SseParser::new().with_context("anthropic", "claude-opus-5");
        let whole = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{}}";
        assert!(parser
            .push(whole)
            .expect("buffers, no terminator yet")
            .is_empty());

        let events = parser
            .finish()
            .expect("unterminated final frame still parses");
        assert_eq!(events.len(), 1, "the final frame should yield its event");
    }

    #[test]
    fn finish_swallows_a_truncated_message_stop_frame() {
        // Real incident: a proxy cut the connection right after the terminal
        // `message_stop` event, leaving `event: message_stop\ndata: {"type":"m`
        // in the buffer. The message is logically complete — only the closing
        // bytes were dropped — so finish() must NOT surface a retryable
        // IncompleteStream (which forces a wasteful whole-turn retry, or loops
        // against a proxy that keeps truncating the same terminal frame).
        let mut parser = SseParser::new().with_context("anthropic", "claude-opus-5");
        let partial = b"event: message_stop\ndata: {\"type\":\"m";
        assert!(parser.push(partial).expect("partial buffers").is_empty());

        let events = parser
            .finish()
            .expect("a truncated terminal frame is a complete message, not an error");
        assert!(
            events.is_empty(),
            "no event is emitted from the dropped closing bytes"
        );
    }

    #[test]
    fn finish_swallows_truncated_message_stop_even_before_the_event_line() {
        // Same completion signal, but the cut landed inside the data JSON with
        // no preceding event line survived — the `"type":"message_stop"` prefix
        // alone must still be recognized as the terminal frame.
        let mut parser = SseParser::new().with_context("anthropic", "claude-opus-5");
        let partial = b"data: {\"type\":\"message_stop\"";
        assert!(parser.push(partial).expect("partial buffers").is_empty());

        let events = parser.finish().expect("terminal frame is complete");
        assert!(events.is_empty());
    }

    #[test]
    fn finish_still_errors_on_a_truncated_content_frame() {
        // A mid-content truncation (not a terminal frame) is a real transport
        // failure and must remain a retryable IncompleteStream.
        let mut parser = SseParser::new().with_context("anthropic", "claude-opus-5");
        let partial = b"event: content_block_delta\ndata: {\"type\":\"content_block_del";
        assert!(parser.push(partial).expect("partial buffers").is_empty());

        let err = parser
            .finish()
            .expect_err("a truncated content frame must still error");
        assert!(matches!(err, ApiError::IncompleteStream { .. }));
        assert!(err.is_retryable());
    }
}
