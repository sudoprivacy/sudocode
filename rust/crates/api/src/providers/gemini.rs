//! Google Gemini provider -- `GenerateContent` API via Cloud Code or API key.
//!
//! Subscription auth reads OAuth credentials from `~/.gemini/oauth_creds.json`
//! (written by the Gemini CLI). The endpoint is the Cloud Code proxy at
//! `https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse`.
//!
//! API-key auth uses the public `generativelanguage.googleapis.com` endpoint.

use std::collections::VecDeque;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::http_transport::{HttpTransport, RetryPolicy};
use crate::providers::registry::Credential;
use crate::types::{
    ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
    InputContentBlock, InputMessage, MessageDelta, MessageDeltaEvent, MessageRequest,
    MessageResponse, MessageStartEvent, MessageStopEvent, OutputContentBlock, StreamEvent,
    ToolDefinition, ToolResultContentBlock, Usage,
};

use super::registry::{preflight_message_request, ResolvedProvider};

// ---------------------------------------------------------------------------
// OAuth credential file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OAuthCreds {
    access_token: String,
    refresh_token: Option<String>,
    /// Expiry as epoch milliseconds.
    #[serde(default)]
    expiry_date: u64,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    /// JWT `id_token` — the `azp` claim contains the OAuth client ID.
    #[serde(default)]
    id_token: Option<String>,
}

impl OAuthCreds {
    /// Resolve the OAuth client ID: explicit field, or extracted from the
    /// `azp` claim in the `id_token` JWT.
    fn resolve_client_id(&self) -> Option<String> {
        if let Some(ref id) = self.client_id {
            return Some(id.clone());
        }
        let token = self.id_token.as_deref()?;
        let payload = token.split('.').nth(1)?;
        let decoded = base64_decode_jwt_segment(payload)?;
        let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
        val.get("azp").and_then(|v| v.as_str()).map(String::from)
    }
}

/// Decode a base64url-encoded JWT segment (no padding required).
fn base64_decode_jwt_segment(segment: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.decode(segment).ok()
}

#[derive(Debug, Deserialize)]
struct RefreshTokenResponse {
    access_token: String,
    expires_in: Option<u64>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct GeminiClient {
    http: HttpTransport,
    base_url: String,
    credential: Credential,
    is_subscription: bool,
    /// Cached project ID from `loadCodeAssist`.
    project_id: tokio::sync::Mutex<Option<String>>,
    /// Cached access token + expiry (epoch ms) after refresh.
    token_cache: tokio::sync::Mutex<Option<(String, u64)>>,
    retry_policy: RetryPolicy,
}

impl Clone for GeminiClient {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            base_url: self.base_url.clone(),
            credential: self.credential.clone(),
            is_subscription: self.is_subscription,
            project_id: tokio::sync::Mutex::new(None),
            token_cache: tokio::sync::Mutex::new(None),
            retry_policy: self.retry_policy.clone(),
        }
    }
}

impl GeminiClient {
    /// Build from resolved config values.
    pub fn from_resolved(resolved: &ResolvedProvider) -> Result<Self, ApiError> {
        let is_subscription = matches!(
            resolved.credential,
            Credential::AuthFile(_) | Credential::Token(_)
        );
        Ok(Self {
            http: HttpTransport::new(),
            base_url: resolved.base_url.clone(),
            credential: resolved.credential.clone(),
            is_subscription,
            project_id: tokio::sync::Mutex::new(None),
            token_cache: tokio::sync::Mutex::new(None),
            retry_policy: RetryPolicy::DEFAULT,
        })
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

    // -----------------------------------------------------------------------
    // Auth helpers
    // -----------------------------------------------------------------------

    /// Resolve a valid bearer token. For subscription mode this reads the
    /// OAuth credential file, checks expiry, and refreshes if needed.
    async fn resolve_auth_token(&self) -> Result<String, ApiError> {
        match &self.credential {
            Credential::Token(t) => Ok(t.clone()),
            Credential::ApiKey(k) => Ok(k.clone()),
            Credential::AuthFile(path) => {
                // Check token cache first.
                {
                    let cache = self.token_cache.lock().await;
                    if let Some((ref token, expiry)) = *cache {
                        let now_ms = now_epoch_ms();
                        if now_ms + 60_000 < expiry {
                            return Ok(token.clone());
                        }
                    }
                }

                let content = std::fs::read_to_string(path).map_err(|e| {
                    ApiError::Auth(format!(
                        "failed to read gemini auth file {}: {e}",
                        path.display()
                    ))
                })?;
                let creds: OAuthCreds = serde_json::from_str(&content).map_err(|e| {
                    ApiError::Auth(format!(
                        "failed to parse gemini auth file {}: {e}",
                        path.display()
                    ))
                })?;

                let now_ms = now_epoch_ms();
                if creds.expiry_date > 0 && now_ms + 60_000 < creds.expiry_date {
                    // Token is still valid; cache and return.
                    let mut cache = self.token_cache.lock().await;
                    *cache = Some((creds.access_token.clone(), creds.expiry_date));
                    return Ok(creds.access_token);
                }

                // Token expired -- refresh.
                let refresh_token = creds.refresh_token.as_deref().ok_or_else(|| {
                    ApiError::Auth(
                        "gemini OAuth token is expired and no refresh_token is available"
                            .to_string(),
                    )
                })?;

                let client_id = creds.resolve_client_id().ok_or_else(|| {
                    ApiError::Auth(
                        "gemini OAuth token is expired and no client_id (or id_token with azp claim) is available in ~/.gemini/oauth_creds.json"
                            .to_string(),
                    )
                })?;

                let client_secret = creds.client_secret.as_deref().ok_or_else(|| {
                    ApiError::Auth(
                        "gemini OAuth token is expired and no client_secret is available in ~/.gemini/oauth_creds.json"
                            .to_string(),
                    )
                })?;

                let resp = self
                    .http
                    .raw()
                    .post("https://oauth2.googleapis.com/token")
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", refresh_token),
                        ("client_id", &client_id),
                        ("client_secret", client_secret),
                    ])
                    .send()
                    .await?;

                if !resp.status().is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(ApiError::Auth(format!(
                        "failed to refresh gemini OAuth token: {body}"
                    )));
                }

                let refresh_resp: RefreshTokenResponse = resp.json().await.map_err(|e| {
                    ApiError::Auth(format!("failed to parse token refresh response: {e}"))
                })?;

                let new_expiry = now_epoch_ms() + refresh_resp.expires_in.unwrap_or(3600) * 1000;

                let mut cache = self.token_cache.lock().await;
                *cache = Some((refresh_resp.access_token.clone(), new_expiry));

                Ok(refresh_resp.access_token)
            }
            Credential::None => Err(ApiError::Configuration(
                "no credential available for Gemini provider".to_string(),
            )),
        }
    }

    /// Discover the Cloud AI Companion project ID for subscription auth.
    async fn resolve_project_id(&self, token: &str) -> Result<String, ApiError> {
        {
            let cached = self.project_id.lock().await;
            if let Some(ref id) = *cached {
                return Ok(id.clone());
            }
        }

        let url = format!("{}/v1internal:loadCodeAssist", self.base_url);
        let resp = self
            .http
            .raw()
            .post(&url)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("user-agent", gemini_cli_user_agent("unknown"))
            .json(&json!({}))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::Auth(format!(
                "failed to load Gemini project ID: {body}"
            )));
        }

        let body: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Auth(format!("failed to parse loadCodeAssist response: {e}")))?;

        let project = body
            .get("cloudaicompanionProject")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ApiError::Auth(
                    "loadCodeAssist response missing cloudaicompanionProject".to_string(),
                )
            })?
            .to_string();

        let mut cached = self.project_id.lock().await;
        *cached = Some(project.clone());

        Ok(project)
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        let mut stream = self.stream_message(request).await?;
        collect_stream(&mut stream, request).await
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        preflight_message_request(request)?;

        let token = self.resolve_auth_token().await?;

        let (url, payload) = if self.is_subscription {
            // Subscription: Cloud Code proxy.
            let project = self.resolve_project_id(&token).await?;
            let inner = build_gemini_request_body(request);
            let envelope = json!({
                "model": request.model,
                "project": project,
                "request": inner,
                "enabled_credit_types": ["GOOGLE_ONE_AI"],
            });
            let url = format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url);
            (url, envelope)
        } else {
            // API-key mode: direct endpoint.
            let payload = build_gemini_request_body(request);
            let url = format!(
                "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
                self.base_url, request.model, token
            );
            (url, payload)
        };

        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            (
                "user-agent".to_string(),
                gemini_cli_user_agent(&request.model),
            ),
        ];
        if self.is_subscription {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }

        let result = self
            .http
            .send_json(&url, &headers, &payload, &self.retry_policy, |response| {
                check_gemini_response(response)
            })
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

fn gemini_cli_user_agent(model: &str) -> String {
    format!(
        "GeminiCLI/{}/{} ({}; {}; terminal)",
        env!("CARGO_PKG_VERSION"),
        model,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

async fn check_gemini_response(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let (error_type, message) = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            let e = v.get("error")?;
            Some((
                e.get("status").and_then(Value::as_str).map(String::from),
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
    })
}

// ---------------------------------------------------------------------------
// Request building -- translate MessageRequest -> Gemini GenerateContent
// ---------------------------------------------------------------------------

fn build_gemini_request_body(request: &MessageRequest) -> Value {
    let mut contents: Vec<Value> = Vec::new();

    // Gemini requires `functionResponse.name` to match the original
    // `functionCall.name`. Build a tool_use_id -> name lookup from all prior
    // assistant turns so each tool result can recover the original function
    // name. Without this, Gemini rejects (or silently drops) the response
    // turn, manifesting as an empty assistant stream.
    let mut tool_name_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in &request.messages {
        for block in &msg.content {
            if let InputContentBlock::ToolUse { id, name, .. } = block {
                tool_name_by_id.insert(id.clone(), name.clone());
            }
        }
    }

    for msg in &request.messages {
        translate_input_message(msg, &mut contents, &tool_name_by_id);
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": request.max_tokens,
        },
    });

    // System instruction.
    if let Some(system) = &request.system {
        if !system.is_empty() {
            body["systemInstruction"] = json!({
                "parts": [{"text": system}]
            });
        }
    }

    // Tools.
    if let Some(tools) = &request.tools {
        if !tools.is_empty() {
            let declarations: Vec<Value> = tools.iter().map(gemini_function_declaration).collect();
            body["tools"] = json!([{
                "functionDeclarations": declarations,
            }]);
        }
    }

    body
}

fn translate_input_message(
    message: &InputMessage,
    contents: &mut Vec<Value>,
    tool_name_by_id: &std::collections::HashMap<String, String>,
) {
    let role = match message.role.as_str() {
        "assistant" => "model",
        _ => "user",
    };

    let mut parts: Vec<Value> = Vec::new();

    for block in &message.content {
        match block {
            InputContentBlock::Text { text } => {
                parts.push(json!({"text": text}));
            }
            InputContentBlock::ToolUse {
                id: _,
                name,
                input: args,
                thought_signature,
            } => {
                let mut part = json!({
                    "functionCall": {
                        "name": name,
                        "args": args,
                    }
                });
                if let Some(sig) = thought_signature {
                    part["thoughtSignature"] = json!(sig);
                }
                parts.push(part);
            }
            InputContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                // Gemini uses functionResponse in user turns.
                // We emit as a separate content entry with role "user".
                // First, flush any accumulated parts.
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": role,
                        "parts": parts,
                    }));
                    parts = Vec::new();
                }
                let response_text = flatten_tool_result(content);
                // Gemini requires the functionResponse name to match the
                // original functionCall name. Look it up from the map built
                // by scanning prior assistant turns. Fall back to the id only
                // if we somehow never saw the corresponding tool_use.
                let fn_name = tool_name_by_id
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| tool_use_id.clone());
                contents.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": fn_name,
                            "response": {
                                "result": response_text,
                            }
                        }
                    }]
                }));
            }
            InputContentBlock::Image { source } => {
                parts.push(json!({
                    "inlineData": {
                        "mimeType": source.media_type,
                        "data": source.data,
                    }
                }));
            }
            InputContentBlock::Thinking { .. } => {}
        }
    }

    if !parts.is_empty() {
        contents.push(json!({
            "role": role,
            "parts": parts,
        }));
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

fn gemini_function_declaration(tool: &ToolDefinition) -> Value {
    let mut decl = json!({
        "name": tool.name,
        "parameters": sanitize_schema_for_gemini(&tool.input_schema),
    });
    if let Some(desc) = &tool.description {
        decl["description"] = json!(desc);
    }
    decl
}

/// Sanitize a JSON Schema value to be Gemini-compatible.
///
/// Gemini's proto-based API is stricter than `OpenAPI` / JSON Schema:
/// - `type` must be a single string, not an array (e.g. `["string","null"]` → `"string"`)
/// - `additionalProperties` is not supported and must be removed
/// - Nested objects and arrays must be recursively sanitized
fn sanitize_schema_for_gemini(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                match key.as_str() {
                    // Remove unsupported fields. Gemini's function-calling schema is
                    // an OpenAPI subset that does not accept JSON Schema combinators
                    // (anyOf/oneOf/allOf), `additionalProperties`, `$schema`, or `default`.
                    // We drop them here so upstream ToolSpec authors can keep the full
                    // JSON Schema form for OpenAI/Anthropic without breaking Gemini.
                    "additionalProperties"
                    | "$schema"
                    | "default"
                    | "anyOf"
                    | "oneOf"
                    | "allOf" => {}

                    // Flatten type arrays: ["string","null"] → "string"
                    "type" => {
                        if let Some(arr) = value.as_array() {
                            let non_null: Vec<&Value> =
                                arr.iter().filter(|v| v.as_str() != Some("null")).collect();
                            if non_null.len() == 1 {
                                out.insert("type".to_string(), non_null[0].clone());
                            } else if non_null.is_empty() {
                                out.insert("type".to_string(), json!("string"));
                            } else {
                                // Multiple non-null types — pick the first.
                                out.insert("type".to_string(), non_null[0].clone());
                            }
                        } else {
                            out.insert("type".to_string(), value.clone());
                        }
                    }
                    // Recursively sanitize nested schemas.
                    "properties" => {
                        if let Some(props) = value.as_object() {
                            let mut sanitized_props = serde_json::Map::new();
                            for (prop_key, prop_val) in props {
                                sanitized_props
                                    .insert(prop_key.clone(), sanitize_schema_for_gemini(prop_val));
                            }
                            out.insert("properties".to_string(), Value::Object(sanitized_props));
                        } else {
                            out.insert(key.clone(), value.clone());
                        }
                    }
                    "items" => {
                        out.insert("items".to_string(), sanitize_schema_for_gemini(value));
                    }
                    _ => {
                        out.insert(key.clone(), sanitize_schema_for_gemini(value));
                    }
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
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

        let (frame_end, sep_len) = if let Some(pos) = text.find("\n\n") {
            (pos, 2)
        } else if let Some(pos) = text.find("\r\n\r\n") {
            (pos, 4)
        } else {
            return None;
        };

        let frame_text = &text[..frame_end];
        let mut data_lines: Vec<String> = Vec::new();

        for line in frame_text.lines() {
            if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.trim().to_string());
            }
        }

        self.buffer.drain(..frame_end + sep_len);

        let data = data_lines.join("\n");
        if data.is_empty() {
            return None;
        }

        Some(SseFrame { data })
    }
}

#[derive(Debug)]
struct SseFrame {
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
#[allow(clippy::struct_excessive_bools)]
struct StreamState {
    model: String,
    message_started: bool,
    finished: bool,

    // Text content block tracking.
    text_block_started: bool,
    text_block_index: u32,

    next_block_index: u32,
    usage: Option<Usage>,
    has_tool_calls: bool,
}

impl StreamState {
    fn new(model: String) -> Self {
        Self {
            model,
            message_started: false,
            finished: false,
            text_block_started: false,
            text_block_index: 0,
            next_block_index: 0,
            usage: None,
            has_tool_calls: false,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn ingest_frame(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, ApiError> {
        if frame.data.is_empty() || frame.data == "[DONE]" {
            return Ok(Vec::new());
        }

        let raw: Value = serde_json::from_str(&frame.data)
            .map_err(|e| ApiError::json_deserialize("Gemini", &self.model, &frame.data, e))?;

        // Cloud Code proxy wraps the response in a `response` envelope.
        // Direct API responses have candidates at the top level.
        let json = raw.get("response").unwrap_or(&raw);

        let mut events = Vec::new();

        // Ensure message_start is emitted once.
        if !self.message_started {
            self.message_started = true;
            events.push(StreamEvent::MessageStart(MessageStartEvent {
                message: MessageResponse {
                    id: String::new(),
                    kind: "message".to_string(),
                    role: "assistant".to_string(),
                    content: Vec::new(),
                    model: self.model.clone(),
                    stop_reason: None,
                    stop_sequence: None,
                    usage: Usage::default(),
                    request_id: None,
                },
            }));
        }

        // Extract candidates[0].content.parts.
        if let Some(candidates) = json.get("candidates").and_then(Value::as_array) {
            if let Some(candidate) = candidates.first() {
                if let Some(content) = candidate.get("content") {
                    if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                // Text part.
                                if !text.is_empty() {
                                    self.ensure_text_block(&mut events);
                                    events.push(StreamEvent::ContentBlockDelta(
                                        ContentBlockDeltaEvent {
                                            index: self.text_block_index,
                                            delta: ContentBlockDelta::TextDelta {
                                                text: text.to_string(),
                                            },
                                        },
                                    ));
                                }
                            } else if let Some(fc) = part.get("functionCall") {
                                // Function call part -- emit as a tool use block.
                                self.has_tool_calls = true;

                                // Close text block if open.
                                if self.text_block_started {
                                    events.push(StreamEvent::ContentBlockStop(
                                        ContentBlockStopEvent {
                                            index: self.text_block_index,
                                        },
                                    ));
                                    self.text_block_started = false;
                                }

                                let name = fc
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let args = fc.get("args").cloned().unwrap_or(json!({}));
                                let call_id = format!("gemini_call_{}", self.next_block_index);
                                // Capture thought_signature (sibling of functionCall in the part).
                                let thought_sig = part
                                    .get("thoughtSignature")
                                    .and_then(Value::as_str)
                                    .map(String::from);

                                let block_index = self.next_block_index;
                                self.next_block_index += 1;

                                events.push(StreamEvent::ContentBlockStart(
                                    ContentBlockStartEvent {
                                        index: block_index,
                                        content_block: OutputContentBlock::ToolUse {
                                            id: call_id,
                                            name,
                                            input: json!({}),
                                            thought_signature: thought_sig,
                                        },
                                    },
                                ));

                                // Emit the full args as a single JSON delta.
                                let args_str = serde_json::to_string(&args).unwrap_or_default();
                                events.push(StreamEvent::ContentBlockDelta(
                                    ContentBlockDeltaEvent {
                                        index: block_index,
                                        delta: ContentBlockDelta::InputJsonDelta {
                                            partial_json: args_str,
                                        },
                                    },
                                ));

                                events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                                    index: block_index,
                                }));
                            }
                        }
                    }
                }
            }
        }

        // Extract usage metadata.
        if let Some(usage_meta) = json.get("usageMetadata") {
            let input_tokens = usage_meta
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let output_tokens = usage_meta
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            self.usage = Some(Usage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            });
        }

        Ok(events)
    }

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

        // Close text block if still open.
        if self.text_block_started {
            events.push(StreamEvent::ContentBlockStop(ContentBlockStopEvent {
                index: self.text_block_index,
            }));
        }

        if self.message_started {
            let stop_reason = if self.has_tool_calls {
                "tool_use"
            } else {
                "end_turn"
            };

            events.push(StreamEvent::MessageDelta(MessageDeltaEvent {
                delta: MessageDelta {
                    stop_reason: Some(stop_reason.to_string()),
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
                model.clone_from(&start.message.model);
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputContentBlock, InputMessage, ToolDefinition};
    use serde_json::json;

    #[test]
    fn build_gemini_request_includes_system_and_messages() {
        let request = MessageRequest {
            model: "gemini-3.1-pro-preview".to_string(),
            max_tokens: 16_384,
            messages: vec![InputMessage::user_text("Hello")],
            system: Some("You are helpful.".to_string()),
            tools: None,
            tool_choice: None,
            stream: true,
            ..Default::default()
        };

        let payload = build_gemini_request_body(&request);
        assert!(payload.get("systemInstruction").is_some());
        let parts = payload["systemInstruction"]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "You are helpful.");
        let contents = payload["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Hello");
        assert_eq!(payload["generationConfig"]["maxOutputTokens"], 16_384);
    }

    #[test]
    fn build_gemini_request_translates_tool_use_to_function_call() {
        let request = MessageRequest {
            model: "gemini-3-flash-preview".to_string(),
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

        let payload = build_gemini_request_body(&request);
        let contents = payload["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][0].get("functionCall").is_some());
        assert_eq!(
            contents[1]["parts"][0]["functionCall"]["name"],
            "get_weather"
        );
        // Tool result is a functionResponse. Gemini requires that the
        // functionResponse name matches the original functionCall name —
        // anything else (e.g. a generic "_tool_result") causes Gemini to
        // reject the turn or emit an empty assistant stream.
        assert!(contents[2]["parts"][0].get("functionResponse").is_some());
        assert_eq!(
            contents[2]["parts"][0]["functionResponse"]["name"],
            "get_weather"
        );

        let tools = payload["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        let decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "get_weather");
    }

    #[test]
    fn sse_parser_extracts_frames() {
        let mut parser = SseParser::new();
        let raw = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n\
                     data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]}}]}\n\n";

        let frames = parser.push(raw);
        assert_eq!(frames.len(), 2);
        assert!(frames[0].data.contains("hi"));
        assert!(frames[1].data.contains("there"));
    }

    #[test]
    fn stream_state_text_response() {
        let mut state = StreamState::new("gemini-3.1-pro-preview".to_string());

        let frame = |data: &str| SseFrame {
            data: data.to_string(),
        };

        let events = state
            .ingest_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"}}]}"#,
            ))
            .unwrap();
        // Should produce: MessageStart, ContentBlockStart (text), ContentBlockDelta (text)
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], StreamEvent::MessageStart(_)));
        assert!(matches!(&events[1], StreamEvent::ContentBlockStart(_)));
        assert!(matches!(&events[2], StreamEvent::ContentBlockDelta(_)));

        let events = state
            .ingest_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"text":" world"}],"role":"model"}}]}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::ContentBlockDelta(_)));

        // Usage in final chunk.
        let events = state
            .ingest_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"text":"!"}],"role":"model"}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}}"#,
            ))
            .unwrap();
        assert_eq!(events.len(), 1);

        let events = state.finish();
        assert_eq!(events.len(), 3); // ContentBlockStop, MessageDelta, MessageStop
        assert!(matches!(&events[0], StreamEvent::ContentBlockStop(_)));
        assert!(matches!(&events[1], StreamEvent::MessageDelta(_)));
        assert!(matches!(&events[2], StreamEvent::MessageStop(_)));

        if let StreamEvent::MessageDelta(md) = &events[1] {
            assert_eq!(md.delta.stop_reason.as_deref(), Some("end_turn"));
            assert_eq!(md.usage.input_tokens, 10);
            assert_eq!(md.usage.output_tokens, 5);
        }
    }

    #[test]
    fn stream_state_tool_call() {
        let mut state = StreamState::new("gemini-3-flash-preview".to_string());

        let frame = |data: &str| SseFrame {
            data: data.to_string(),
        };

        let events = state
            .ingest_frame(&frame(
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read_file","args":{"path":"/tmp/test.rs"}}}],"role":"model"}}]}"#,
            ))
            .unwrap();
        // MessageStart + ContentBlockStart (tool) + ContentBlockDelta (args) + ContentBlockStop
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[0], StreamEvent::MessageStart(_)));
        if let StreamEvent::ContentBlockStart(start) = &events[1] {
            assert_eq!(start.index, 0);
            if let OutputContentBlock::ToolUse { name, .. } = &start.content_block {
                assert_eq!(name, "read_file");
            } else {
                panic!("expected ToolUse block");
            }
        } else {
            panic!("expected ContentBlockStart");
        }

        let events = state.finish();
        assert_eq!(events.len(), 2); // MessageDelta + MessageStop
        if let StreamEvent::MessageDelta(md) = &events[0] {
            assert_eq!(md.delta.stop_reason.as_deref(), Some("tool_use"));
        }
    }
}
