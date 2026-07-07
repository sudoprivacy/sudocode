use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

/// Incremental WHATWG Server-Sent-Events parser that operates on **raw bytes**.
///
/// Feeding is done at the byte level so a multi-byte UTF-8 character split
/// across TCP segments is reassembled in `buffer` before any decoding. Lines
/// are split on the `\n` (0x0A) byte boundary; because the UTF-8 encoding
/// guarantees that ASCII bytes (0x00–0x7F) never appear inside a multi-byte
/// continuation sequence, `\n` is always a safe line boundary, and each
/// complete line can be decoded independently without corrupting characters.
#[derive(Debug, Clone, Default)]
pub struct IncrementalSseParser {
    buffer: Vec<u8>,
    event_name: Option<String>,
    data_lines: Vec<String>,
    id: Option<String>,
    retry: Option<u64>,
}

impl IncrementalSseParser {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw bytes. Returns any events delimited by a blank
    /// line contained fully within the chunk(s) seen so far.
    pub fn push_chunk(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(index) = self.buffer.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=index).collect();
            let line = line_to_str(&line_bytes);
            self.process_line(&line, &mut events);
        }

        events
    }

    /// Flush any trailing buffered bytes (no closing newline) as a final line.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let remaining = std::mem::take(&mut self.buffer);
            let line = line_to_str(&remaining);
            self.process_line(&line, &mut events);
        }
        if let Some(event) = self.take_event() {
            events.push(event);
        }
        events
    }

    fn process_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if let Some(event) = self.take_event() {
                events.push(event);
            }
            return;
        }

        if line.starts_with(':') {
            return;
        }

        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| {
            let trimmed = value.strip_prefix(' ').unwrap_or(value);
            (field, trimmed)
        });

        match field {
            "event" => self.event_name = Some(value.to_owned()),
            "data" => self.data_lines.push(value.to_owned()),
            "id" => self.id = Some(value.to_owned()),
            "retry" => self.retry = value.parse::<u64>().ok(),
            _ => {}
        }
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        if self.data_lines.is_empty()
            && self.event_name.is_none()
            && self.id.is_none()
            && self.retry.is_none()
        {
            return None;
        }

        let data = self.data_lines.join("\n");
        self.data_lines.clear();

        Some(SseEvent {
            event: self.event_name.take(),
            data,
            id: self.id.take(),
            retry: self.retry.take(),
        })
    }
}

/// Decode a raw line (with its trailing `\n` and optional `\r` stripped) as
/// UTF-8, replacing any (non-spec-compliant) invalid bytes with the replacement
/// char. The strip is byte-safe: `\n`/`\r` are ASCII single bytes.
fn line_to_str(line_bytes: &[u8]) -> String {
    let mut end = line_bytes.len();
    if end > 0 && line_bytes[end - 1] == b'\n' {
        end -= 1;
    }
    if end > 0 && line_bytes[end - 1] == b'\r' {
        end -= 1;
    }
    String::from_utf8_lossy(&line_bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{IncrementalSseParser, SseEvent};

    #[test]
    fn parses_streaming_events() {
        // given
        let mut parser = IncrementalSseParser::new();

        // when
        let first = parser.push_chunk(b"event: message\ndata: hel");

        // then
        assert!(first.is_empty());

        let second = parser.push_chunk(b"lo\n\nid: 1\ndata: world\n\n");
        assert_eq!(
            second,
            vec![
                SseEvent {
                    event: Some(String::from("message")),
                    data: String::from("hello"),
                    id: None,
                    retry: None,
                },
                SseEvent {
                    event: None,
                    data: String::from("world"),
                    id: Some(String::from("1")),
                    retry: None,
                },
            ]
        );
    }

    #[test]
    fn finish_flushes_a_trailing_event_without_separator() {
        // given
        let mut parser = IncrementalSseParser::new();
        parser.push_chunk(b"event: message\ndata: trailing");

        // when
        let events = parser.finish();

        // then
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("message".to_string()),
                data: "trailing".to_string(),
                id: None,
                retry: None,
            }]
        );
    }

    /// Regression: a multi-byte UTF-8 character split across two byte chunks
    /// must be reassembled, not corrupted into replacement chars. "中" is
    /// 0xE4 0xB8 0xAD; the split lands between the 2nd and 3rd bytes.
    #[test]
    fn reassembles_multibyte_char_split_across_chunks() {
        let mut parser = IncrementalSseParser::new();

        let first = parser.push_chunk(b"data: \xe4\xb8");
        assert!(first.is_empty(), "no event until a blank line");

        let second = parser.push_chunk(b"\xad\n\n");
        assert_eq!(
            second,
            vec![SseEvent {
                event: None,
                data: "中".to_string(),
                id: None,
                retry: None,
            }]
        );
    }
}
