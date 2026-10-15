use std::time::Instant;

use telemetry::SessionTracer;

use crate::types::{ContentBlockDelta, StreamEvent};

/// Metadata-only progress shared by provider adapters. Logarithmic sampling
/// retains evidence of stalled streams without recording response contents.
#[derive(Debug)]
pub(crate) struct StreamProgress {
    tracer: Option<SessionTracer>,
    request_id: String,
    model: String,
    started: Instant,
    pub chunks_read: u64,
    pub sse_events_read: u64,
    bytes_read: u64,
    buffered_bytes: u64,
    unframed_json: bool,
    normalized_events: u64,
    text_deltas: u64,
    thinking_deltas: u64,
    tool_deltas: u64,
    finished: bool,
}

impl StreamProgress {
    pub fn new(tracer: Option<SessionTracer>, request_id: Option<String>, model: String) -> Self {
        let progress = Self {
            tracer,
            request_id: request_id.unwrap_or_else(|| "unknown".to_string()),
            model,
            started: Instant::now(),
            chunks_read: 0,
            sse_events_read: 0,
            bytes_read: 0,
            buffered_bytes: 0,
            unframed_json: false,
            normalized_events: 0,
            text_deltas: 0,
            thinking_deltas: 0,
            tool_deltas: 0,
            finished: false,
        };
        progress.record("headers");
        progress
    }

    pub fn observe_chunk(&mut self, bytes: usize) {
        self.chunks_read += 1;
        self.bytes_read = self
            .bytes_read
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if self.chunks_read.is_power_of_two() {
            self.record("chunk_received");
        }
    }

    pub fn parsed_chunk(&mut self, pending: &[u8]) {
        self.buffered_bytes = u64::try_from(pending.len()).unwrap_or(u64::MAX);
        self.unframed_json = crate::sse::body_opens_as_json(pending);
        if self.chunks_read.is_power_of_two() {
            self.record("chunk_parsed");
        }
    }

    pub fn observe_event(&mut self, event: &StreamEvent) {
        self.normalized_events += 1;
        let is_first_delta = if let StreamEvent::ContentBlockDelta(event) = event {
            let count = match &event.delta {
                ContentBlockDelta::TextDelta { .. } => Some(&mut self.text_deltas),
                ContentBlockDelta::ThinkingDelta { .. } => Some(&mut self.thinking_deltas),
                ContentBlockDelta::InputJsonDelta { .. } => Some(&mut self.tool_deltas),
                ContentBlockDelta::SignatureDelta { .. } => None,
            };
            count.is_some_and(|count| {
                *count += 1;
                *count == 1
            })
        } else {
            false
        };
        if matches!(event, StreamEvent::MessageStop(_)) {
            self.finish();
        } else if is_first_delta {
            self.record("content_event");
        }
    }

    fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.record("complete");
        }
    }

    fn record(&self, phase: &str) {
        let Some(tracer) = &self.tracer else { return };
        let attributes = serde_json::json!({
            "request_id": self.request_id,
            "phase": phase,
            "model": self.model,
            "chunks_read": self.chunks_read,
            "bytes_read": self.bytes_read,
            "buffered_bytes": self.buffered_bytes,
            "unframed_json": self.unframed_json,
            "sse_events_read": self.sse_events_read,
            "normalized_events": self.normalized_events,
            "text_deltas": self.text_deltas,
            "thinking_deltas": self.thinking_deltas,
            "tool_deltas": self.tool_deltas,
            "elapsed_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "finished": self.finished,
        });
        tracer.record(
            "response_stream_progress",
            attributes.as_object().unwrap().clone(),
        );
    }
}
