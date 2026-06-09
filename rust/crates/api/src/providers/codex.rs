//! `OpenAI` Codex provider — Responses API via chatgpt.com backend.
//!
//! Subscription auth reads credentials from `~/.codex/auth.json` (written by
//! the Codex CLI). The endpoint is `https://chatgpt.com/backend-api/codex/responses`
//! which speaks the `OpenAI` Responses API (not Chat Completions).

use std::collections::{BTreeMap, VecDeque};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::http_transport::{parse_retry_after, HttpTransport, RetryPolicy};
use crate::types::{
    ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
    InputContentBlock, InputMessage, MessageDelta, MessageDeltaEvent, MessageRequest,
    MessageResponse, MessageStartEvent, MessageStopEvent, OutputContentBlock, StreamEvent,
    ToolDefinition, ToolResultContentBlock, Usage,
};

use super::registry::preflight_message_request;

pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const AUTH_FILE_REL: &str = ".codex/auth.json";

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CodexAuthTokens {
    access_token: String,
    #[serde(default)]
    account_id: String,
}

/// The Codex CLI writes `~/.codex/auth.json` with a nested `tokens` object.
/// We also support a flat layout for backwards compat / testing.
#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    /// Nested format: `{ "tokens": { "access_token": "...", "account_id": "..." } }`
    tokens: Option<CodexAuthTokens>,
    /// Flat format: `{ "access_token": "...", "account_id": "..." }`
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

impl CodexAuthFile {
    fn into_credentials(self) -> Result<(String, String), &'static str> {
        if let Some(tokens) = self.tokens {
            return Ok((tokens.access_token, tokens.account_id));
        }
        if let Some(token) = self.access_token {
            return Ok((token, self.account_id.unwrap_or_default()));
        }
        Err("no access_token found in auth file")
    }
}

fn read_auth_file() -> Result<(String, String), ApiError> {
    let home = std::env::var("HOME").map_err(|_| {
        ApiError::Auth(
            "cannot determine home directory (HOME not set) for ~/.codex/auth.json".to_string(),
        )
    })?;
    let path = std::path::Path::new(&home).join(AUTH_FILE_REL);
    let content = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::Auth(format!(
            "failed to read {}: {e}; run `codex` first to authenticate",
            path.display()
        ))
    })?;
    let auth_file: CodexAuthFile = serde_json::from_str(&content)
        .map_err(|e| ApiError::Auth(format!("failed to parse {}: {e}", path.display())))?;
    auth_file
        .into_credentials()
        .map_err(|msg| ApiError::Auth(format!("{}: {msg}", path.display())))
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CodexClient {
    http: HttpTransport,
    base_url: String,
    access_token: String,
    account_id: String,
    retry_policy: RetryPolicy,
}

impl CodexClient {
    /// Build from resolved config values (base URL + credentials).
    #[must_use]
    pub fn new(base_url: String, access_token: String, account_id: String) -> Self {
        Self {
            http: HttpTransport::new(),
            base_url,
            access_token,
            account_id,
            retry_policy: RetryPolicy::DEFAULT,
        }
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: telemetry::SessionTracer) -> Self {
        self.http.set_session_tracer(session_tracer);
        self
    }

    #[must_use]
    pub fn session_tracer(&self) -> Option<&telemetry::SessionTracer> {
        self.http.session_tracer()
    }

    /// Build from `~/.codex/auth.json`.
    pub fn from_auth_file() -> Result<Self, ApiError> {
        let (access_token, account_id) = read_auth_file()?;
        let base_url =
            std::env::var("CODEX_BASE_URL").unwrap_or_else(|_| DEFAULT_CODEX_BASE_URL.to_string());
        Ok(Self {
            http: HttpTransport::new(),
            base_url,
            access_token,
            account_id,
            retry_policy: RetryPolicy::DEFAULT,
        })
    }

    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn send_message(
        &self,
        request: &MessageRequest,
        trace_id: Option<&str>,
    ) -> Result<MessageResponse, ApiError> {
        // Codex subscription requires streaming; collect the stream into a
        // single response.
        let mut stream = self.stream_message(request, trace_id).await?;
        collect_stream(&mut stream, request).await
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
        trace_id: Option<&str>,
    ) -> Result<MessageStream, ApiError> {
        preflight_message_request(request)?;

        let payload = build_codex_request(request);
        let url = format!("{}/responses", self.base_url);

        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {}", self.access_token),
            ),
            ("chatgpt-account-id".to_string(), self.account_id.clone()),
            (
                "openai-beta".to_string(),
                "responses=experimental".to_string(),
            ),
            ("originator".to_string(), "codex_cli_rs".to_string()),
        ];

        let result = self
            .http
            .send_json(
                &url,
                &headers,
                &payload,
                &self.retry_policy,
                |response| check_codex_response(response),
                trace_id,
            )
            .await?;

        Ok(MessageStream {
            response: result.response,
            parser: SseParser::new(),
            pending: VecDeque::new(),
            done: false,
            state: StreamState::new(request.model.clone()),
        })
    }
}

async fn check_codex_response(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = parse_retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    let (error_type, message) = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            Some((
                e.get("type").and_then(Value::as_str).map(String::from),
                e.get("message").and_then(Value::as_str).map(String::from),
            ))
        })
        .unwrap_or((None, None));
    Err(ApiError::Api {
        status,
        error_type,
        message,
        request_id: None,
        body,
        retryable: matches!(status.as_u16(), 429 | 500 | 502 | 503),
        suggested_action: None,
        retry_after,
    })
}

// ---------------------------------------------------------------------------
// Request building — translate MessageRequest → Responses API
// ---------------------------------------------------------------------------

fn build_codex_request(request: &MessageRequest) -> Value {
    let mut input: Vec<Value> = Vec::new();

    for msg in &request.messages {
        translate_input_message(msg, &mut input);
    }

    let wire_model = strip_codex_prefix(&request.model);

    // The Responses API uses a top-level `instructions` field for the system
    // prompt (required by the chatgpt.com backend).
    let instructions = request
        .system
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("You are a helpful assistant.");

    let mut payload = json!({
        "model": wire_model,
        "instructions": instructions,
        "input": input,
        "stream": true,
        "store": false,
    });

    if let Some(tools) = &request.tools {
        payload["tools"] = Value::Array(tools.iter().map(codex_tool_definition).collect());
    }

    payload
}

fn translate_input_message(message: &InputMessage, input: &mut Vec<Value>) {
    if message.role.as_str() == "assistant" {
        let mut text_buf = String::new();
        for block in &message.content {
            match block {
                InputContentBlock::Text { text } => text_buf.push_str(text),
                InputContentBlock::ToolUse {
                    id,
                    name,
                    input: args,
                    ..
                } => {
                    flush_text(&mut text_buf, "assistant", input);
                    input.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": args.to_string(),
                    }));
                }
                InputContentBlock::ToolResult { .. }
                | InputContentBlock::Image { .. }
                | InputContentBlock::Thinking { .. } => {}
            }
        }
        flush_text(&mut text_buf, "assistant", input);
    } else {
        let mut user_parts: Vec<Value> = Vec::new();
        for block in &message.content {
            match block {
                InputContentBlock::Text { text } => {
                    user_parts.push(json!({ "type": "input_text", "text": text }));
                }
                InputContentBlock::Image { source } => {
                    user_parts.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", source.media_type, source.data),
                    }));
                }
                InputContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    // Flush accumulated user parts before the tool result.
                    if !user_parts.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "user",
                            "content": user_parts,
                        }));
                        user_parts = Vec::new();
                    }
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": flatten_tool_result(content),
                    }));
                }
                InputContentBlock::ToolUse { .. } | InputContentBlock::Thinking { .. } => {}
            }
        }
        // Flush remaining user parts.
        if !user_parts.is_empty() {
            // Plain string for text-only messages (broad compatibility).
            if user_parts.len() == 1 && user_parts[0]["type"] == "input_text" {
                input.push(json!({"role": "user", "content": user_parts[0]["text"]}));
            } else {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": user_parts,
                }));
            }
        }
    }
}

fn flush_text(buf: &mut String, role: &str, input: &mut Vec<Value>) {
    if !buf.is_empty() {
        input.push(json!({"role": role, "content": buf.as_str()}));
        buf.clear();
    }
}

fn flatten_tool_result(content: &[ToolResultContentBlock]) -> String {
    content
        .iter()
        .map(|c| match c {
            ToolResultContentBlock::Text { text } => text.clone(),
            ToolResultContentBlock::Json { value } => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn codex_tool_definition(tool: &ToolDefinition) -> Value {
    let mut params = tool.input_schema.clone();
    // Codex API requires "properties" on object schemas; add empty one if missing.
    if params.get("type").and_then(|v| v.as_str()) == Some("object")
        && params.get("properties").is_none()
    {
        params["properties"] = json!({});
    }
    let mut def = json!({
        "type": "function",
        "name": tool.name,
        "parameters": params,
    });
    if let Some(desc) = &tool.description {
        def["description"] = json!(desc);
    }
    def
}

fn strip_codex_prefix(model: &str) -> &str {
    model.strip_prefix("codex/").unwrap_or(model)
}

// ---------------------------------------------------------------------------
// SSE frame parser
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();

        while let Some(frame) = self.next_frame() {
            frames.push(frame);
        }

        frames
    }

    fn next_frame(&mut self) -> Option<SseFrame> {
        let text = std::str::from_utf8(&self.buffer).ok()?;

        // Look for the double-newline frame separator.
        let (frame_end, sep_len) = if let Some(pos) = text.find("\n\n") {
            (pos, 2)
        } else if let Some(pos) = text.find("\r\n\r\n") {
            (pos, 4)
        } else {
            return None;
        };

        let frame_text = &text[..frame_end];
        let mut event_type = String::new();
        let mut data_lines: Vec<String> = Vec::new();

        for line in frame_text.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event_type = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.trim().to_string());
            }
        }

        self.buffer.drain(..frame_end + sep_len);

        let data = data_lines.join("\n");
        if event_type.is_empty() && data.is_empty() {
            return None;
        }

        Some(SseFrame { event_type, data })
    }
}

#[derive(Debug)]
struct SseFrame {
    event_type: String,
    data: String,
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct MessageStream {
    response: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<StreamEvent>,
    done: bool,
    state: StreamState,
}

impl MessageStream {
    #[allow(clippy::unused_self)]
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        None
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }

            if self.done {
                self.pending.extend(self.state.finish());
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }

            match self.response.chunk().await? {
                Some(chunk) => {
                    for frame in self.parser.push(&chunk) {
                        self.pending.extend(self.state.ingest_frame(&frame)?);
                    }
                }
                None => {
                    self.done = true;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stream state machine
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct StreamState {
    model: String,
    message_started: bool,
    finished: bool,

    // Text content block tracking.
    text_block_started: bool,
    text_block_index: u32,

    // Tool (function_call) block tracking: output_index → state.
    tool_blocks: BTreeMap<u32, ToolBlockState>,

    next_block_index: u32,
    usage: Option<Usage>,
    stop_reason: Option<String>,
}

#[derive(Debug)]
struct ToolBlockState {
    block_index: u32,
    started: bool,
    stopped: bool,
}

impl StreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            message_started: false,
            finished: false,
            text_block_started: false,
            text_block_index: 0,
            tool_blocks: BTreeMap::new(),
            next_block_index: 0,
            usage: None,
            stop_reason: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_frame(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, ApiError> {
        if frame.data.is_empty() || frame.data == "[DONE]" {
            return Ok(Vec::new());
        }

        let json: Value = serde_json::from_str(&frame.data)
            .map_err(|e| ApiError::json_deserialize("Codex", &self.model, &frame.data, e))?;

        let mut events = Vec::new();

        match frame.event_type.as_str() {
            "response.created" => {
                if !self.message_started {
                    self.message_started = true;
                    let id = json_str(&json, "id");
                    let model = json
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or(&self.model);
                    events.push(StreamEvent::MessageStart(MessageStartEvent {
                        message: MessageResponse {
                            id: id.to_string(),
                            kind: "message".to_string(),
                            role: "assistant".to_string(),
                            content: Vec::new(),
                            model: model.to_string(),
                            stop_reason: None,
                            stop_sequence: None,
                            usage: Usage::default(),
                            request_id: None,
                        },
                    }));
                }
            }

            "response.content_part.added" => {
                self.ensure_text_block(&mut events);
            }

            "response.output_text.delta" => {
                let delta = json_str(&json, "delta");
                if !delta.is_empty() {
                    self.ensure_text_block(&mut events);
                    events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                        index: self.text_block_index,
                        delta: ContentBlockDelta::TextDelta {
                            text: delta.to_string(),
                        },
                    }));
                }
            }

            "response.output_text.done" => {
                if self.text_block_started {
                    events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                        index: self.text_block_index,
                    }));
                }
            }

            "response.output_item.added" => {
                let item = json.get("item").unwrap_or(&Value::Null);
                if json_str(item, "type") == "function_call" {
                    let output_index = json_u32(&json, "output_index");
                    let block_index = self.next_block_index;
                    self.next_block_index += 1;

                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = json_str(item, "name").to_string();

                    events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                        index: block_index,
                        content_block: OutputContentBlock::ToolUse {
                            id: call_id,
                            name,
                            input: json!({}),
                            thought_signature: None,
                        },
                    }));

                    self.tool_blocks.insert(
                        output_index,
                        ToolBlockState {
                            block_index,
                            started: true,
                            stopped: false,
                        },
                    );
                }
            }

            "response.function_call_arguments.delta" => {
                let output_index = json_u32(&json, "output_index");
                if let Some(ts) = self.tool_blocks.get(&output_index) {
                    let delta = json_str(&json, "delta");
                    if !delta.is_empty() {
                        events.push(StreamEvent::ContentBlockDelta(ContentBlockDeltaEvent {
                            index: ts.block_index,
                            delta: ContentBlockDelta::InputJsonDelta {
                                partial_json: delta.to_string(),
                            },
                        }));
                    }
                }
            }

            "response.function_call_arguments.done" | "response.output_item.done" => {
                let output_index = json_u32(&json, "output_index");
                if let Some(ts) = self.tool_blocks.get_mut(&output_index) {
                    if !ts.stopped {
                        ts.stopped = true;
                        events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                            index: ts.block_index,
                        }));
                    }
                }
            }

            "response.completed" => {
                let resp = json.get("response").unwrap_or(&json);
                if let Some(u) = resp.get("usage") {
                    self.usage = Some(Usage {
                        input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0)
                            as u32,
                        output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0)
                            as u32,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        ..Usage::default()
                    });
                }
                self.stop_reason = Some(if self.tool_blocks.is_empty() {
                    "end_turn".to_string()
                } else {
                    "tool_use".to_string()
                });
            }

            _ => {} // Ignore unknown event types.
        }

        Ok(events)
    }

    /// Lazily open the text content block.
    fn ensure_text_block(&mut self, events: &mut Vec<StreamEvent>) {
        if !self.text_block_started {
            self.text_block_started = true;
            self.text_block_index = self.next_block_index;
            self.next_block_index += 1;
            events.push(StreamEvent::ContentBlockStart(ContentBlockStartEvent {
                index: self.text_block_index,
                content_block: OutputContentBlock::Text {
                    text: String::new(),
                },
            }));
        }
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut events = Vec::new();

        // Close any unclosed tool blocks.
        for ts in self.tool_blocks.values_mut() {
            if ts.started && !ts.stopped {
                ts.stopped = true;
                events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                    index: ts.block_index,
                }));
            }
        }

        if self.message_started {
            events.push(StreamEvent::MessageDelta(MessageDeltaEvent {
                delta: MessageDelta {
                    stop_reason: Some(
                        self.stop_reason
                            .clone()
                            .unwrap_or_else(|| "end_turn".to_string()),
                    ),
                    stop_sequence: None,
                },
                usage: self.usage.clone().unwrap_or_default(),
            }));
            events.push(StreamEvent::MessageStop(MessageStopEvent {}));
        }

        events
    }
}

// ---------------------------------------------------------------------------
// Collect a full stream into a MessageResponse (for send_message)
// ---------------------------------------------------------------------------

async fn collect_stream(
    stream: &mut MessageStream,
    request: &MessageRequest,
) -> Result<MessageResponse, ApiError> {
    let mut content: Vec<OutputContentBlock> = Vec::new();
    let mut model = request.model.clone();
    let mut id = String::new();
    let mut usage = Usage::default();
    let mut stop_reason = None;

    while let Some(event) = stream.next_event().await? {
        match event {
            StreamEvent::MessageStart(start) => {
                id = start.message.id;
                model = start.message.model;
            }
            StreamEvent::ContentBlockStart(start) => {
                content.push(start.content_block);
            }
            StreamEvent::ContentBlockDelta(delta) => {
                apply_delta(&mut content, &delta);
            }
            StreamEvent::ContentBlockStop(_) | StreamEvent::MessageStop(_) => {}
            StreamEvent::MessageDelta(d) => {
                stop_reason = d.delta.stop_reason;
                usage = d.usage;
            }
        }
    }

    // Parse accumulated JSON strings in tool-use blocks.
    for block in &mut content {
        if let OutputContentBlock::ToolUse { input, .. } = block {
            if let Some(s) = input.as_str() {
                if let Ok(parsed) = serde_json::from_str(s) {
                    *input = parsed;
                }
            }
        }
    }

    Ok(MessageResponse {
        id,
        kind: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model,
        stop_reason,
        stop_sequence: None,
        usage,
        request_id: None,
    })
}

fn apply_delta(content: &mut [OutputContentBlock], delta: &ContentBlockDeltaEvent) {
    let Some(block) = content.last_mut() else {
        return;
    };
    match (&mut *block, &delta.delta) {
        (OutputContentBlock::Text { text }, ContentBlockDelta::TextDelta { text: new_text }) => {
            text.push_str(new_text);
        }
        (
            OutputContentBlock::ToolUse { input, .. },
            ContentBlockDelta::InputJsonDelta { partial_json },
        ) => {
            if let Some(existing) = input.as_str() {
                *input = Value::String(format!("{existing}{partial_json}"));
            } else {
                *input = Value::String(partial_json.clone());
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tiny JSON helpers
// ---------------------------------------------------------------------------

fn json_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn json_u32(v: &Value, key: &str) -> u32 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputContentBlock, InputMessage, ToolDefinition};
    use serde_json::json;

    #[test]
    fn build_codex_request_includes_system_and_messages() {
        let request = MessageRequest {
            model: "gpt-5.4-mini".to_string(),
            max_tokens: 16_384,
            messages: vec![InputMessage::user_text("Hello")],
            system: Some("You are helpful.".to_string()),
            tools: None,
            tool_choice: None,
            stream: true,
            ..Default::default()
        };

        let payload = build_codex_request(&request);
        assert_eq!(payload["instructions"], "You are helpful.");
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], "Hello");
        assert_eq!(payload["model"], "gpt-5.4-mini");
        assert!(payload["stream"].as_bool().unwrap());
        assert!(!payload["store"].as_bool().unwrap());
    }

    #[test]
    fn build_codex_request_translates_tool_use_to_function_call() {
        let request = MessageRequest {
            model: "gpt-5.4".to_string(),
            max_tokens: 16_384,
            messages: vec![
                InputMessage::user_text("What's the weather?"),
                InputMessage {
                    role: "assistant".to_string(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "call_123".to_string(),
                        name: "get_weather".to_string(),
                        input: json!({"city": "SF"}),
                        thought_signature: None,
                    }],
                },
                InputMessage::user_tool_result("call_123", "72F sunny", false),
            ],
            system: None,
            tools: Some(vec![ToolDefinition {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}}
                }),
            }]),
            tool_choice: None,
            stream: true,
            ..Default::default()
        };

        let payload = build_codex_request(&request);
        let input = payload["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_123");
        assert_eq!(input[1]["name"], "get_weather");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_123");
        assert_eq!(input[2]["output"], "72F sunny");

        let tools = payload["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "get_weather");
    }

    #[test]
    fn strip_codex_prefix_removes_prefix() {
        assert_eq!(strip_codex_prefix("codex/gpt-5.4-mini"), "gpt-5.4-mini");
        assert_eq!(strip_codex_prefix("gpt-5.4"), "gpt-5.4");
    }

    #[test]
    fn sse_parser_extracts_frames() {
        let mut parser = SseParser::new();
        let raw = b"event: response.created\n\
                     data: {\"id\":\"resp_1\",\"model\":\"gpt-5.4-mini\"}\n\n\
                     event: response.output_text.delta\n\
                     data: {\"delta\":\"hi\"}\n\n";

        let frames = parser.push(raw);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event_type, "response.created");
        assert!(frames[0].data.contains("resp_1"));
        assert_eq!(frames[1].event_type, "response.output_text.delta");
        assert!(frames[1].data.contains("hi"));
    }

    #[test]
    fn stream_state_text_response() {
        let mut state = StreamState::new("gpt-5.4-mini".to_string());

        let frame = |et: &str, data: &str| SseFrame {
            event_type: et.to_string(),
            data: data.to_string(),
        };

        let events = state
            .ingest_frame(&frame(
                "response.created",
                r#"{"id":"resp_1","model":"gpt-5.4-mini"}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::MessageStart(_)));

        let events = state
            .ingest_frame(&frame(
                "response.content_part.added",
                r#"{"output_index":0}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentBlockStart(_)));

        let events = state
            .ingest_frame(&frame("response.output_text.delta", r#"{"delta":"Hello"}"#))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentBlockDelta(_)));

        let events = state
            .ingest_frame(&frame("response.output_text.done", r"{}"))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentBlockStop(_)));

        let events = state
            .ingest_frame(&frame("response.completed", r"{}"))
            .unwrap();
        assert!(events.is_empty());

        let events = state.finish();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], StreamEvent::MessageDelta(_)));
        assert!(matches!(&events[1], StreamEvent::MessageStop(_)));
    }

    #[test]
    fn stream_state_tool_call() {
        let mut state = StreamState::new("gpt-5.4".to_string());

        let frame = |et: &str, data: &str| SseFrame {
            event_type: et.to_string(),
            data: data.to_string(),
        };

        state
            .ingest_frame(&frame(
                "response.created",
                r#"{"id":"resp_2","model":"gpt-5.4"}"#,
            ))
            .unwrap();

        let events = state
            .ingest_frame(&frame(
                "response.output_item.added",
                r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_abc","name":"read_file"}}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);
        if let StreamEvent::ContentBlockStart(start) = &events[0] {
            assert_eq!(start.index, 0);
            if let OutputContentBlock::ToolUse { id, name, .. } = &start.content_block {
                assert_eq!(id, "call_abc");
                assert_eq!(name, "read_file");
            } else {
                panic!("expected ToolUse block");
            }
        } else {
            panic!("expected ContentBlockStart");
        }

        let events = state
            .ingest_frame(&frame(
                "response.function_call_arguments.delta",
                r#"{"output_index":0,"delta":"{\"path\":"}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentBlockDelta(_)));

        let events = state
            .ingest_frame(&frame(
                "response.function_call_arguments.done",
                r#"{"output_index":0}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentBlockStop(_)));
    }
}
