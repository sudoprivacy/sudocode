use std::fmt::{Debug, Formatter};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const SUDOCLAW_LOG_ROTATE_AFTER_BYTES: u64 = 10 * 1024 * 1024; // 10MB
const SUDOCLAW_LOG_MAX_ROTATED_FILES: usize = 3;

pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_APP_NAME: &str = "claude-code";
pub const DEFAULT_RUNTIME: &str = "rust";
pub const DEFAULT_AGENTIC_BETA: &str = "claude-code-20250219";
pub const DEFAULT_PROMPT_CACHING_SCOPE_BETA: &str = "prompt-caching-scope-2026-01-05";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub app_name: String,
    pub app_version: String,
    pub runtime: String,
}

impl ClientIdentity {
    #[must_use]
    pub fn new(app_name: impl Into<String>, app_version: impl Into<String>) -> Self {
        Self {
            app_name: app_name.into(),
            app_version: app_version.into(),
            runtime: DEFAULT_RUNTIME.to_string(),
        }
    }

    #[must_use]
    pub fn with_runtime(mut self, runtime: impl Into<String>) -> Self {
        self.runtime = runtime.into();
        self
    }

    #[must_use]
    pub fn user_agent(&self) -> String {
        format!("{}/{}", self.app_name, self.app_version)
    }
}

impl Default for ClientIdentity {
    fn default() -> Self {
        Self::new(DEFAULT_APP_NAME, env!("CARGO_PKG_VERSION"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicRequestProfile {
    pub anthropic_version: String,
    pub client_identity: ClientIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub betas: Vec<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra_body: Map<String, Value>,
}

impl AnthropicRequestProfile {
    #[must_use]
    pub fn new(client_identity: ClientIdentity) -> Self {
        Self {
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_string(),
            client_identity,
            betas: vec![
                DEFAULT_AGENTIC_BETA.to_string(),
                DEFAULT_PROMPT_CACHING_SCOPE_BETA.to_string(),
            ],
            extra_body: Map::new(),
        }
    }

    #[must_use]
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        let beta = beta.into();
        if !self.betas.contains(&beta) {
            self.betas.push(beta);
        }
        self
    }

    #[must_use]
    pub fn with_betas(mut self, betas: Vec<String>) -> Self {
        self.betas = betas;
        self
    }

    #[must_use]
    pub fn with_extra_body(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra_body.insert(key.into(), value);
        self
    }

    #[must_use]
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "anthropic-version".to_string(),
                self.anthropic_version.clone(),
            ),
            ("user-agent".to_string(), self.client_identity.user_agent()),
        ];
        if !self.betas.is_empty() {
            headers.push(("anthropic-beta".to_string(), self.betas.join(",")));
        }
        headers
    }

    pub fn render_json_body<T: Serialize>(&self, request: &T) -> Result<Value, serde_json::Error> {
        let mut body = serde_json::to_value(request)?;
        let object = body.as_object_mut().ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request body must serialize to a JSON object",
            ))
        })?;
        for (key, value) in &self.extra_body {
            object.insert(key.clone(), value.clone());
        }
        if !self.betas.is_empty() {
            object.insert(
                "betas".to_string(),
                Value::Array(self.betas.iter().cloned().map(Value::String).collect()),
            );
        }
        Ok(body)
    }
}

impl Default for AnthropicRequestProfile {
    fn default() -> Self {
        Self::new(ClientIdentity::default())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyticsEvent {
    pub namespace: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub properties: Map<String, Value>,
}

impl AnalyticsEvent {
    #[must_use]
    pub fn new(namespace: impl Into<String>, action: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            action: action.into(),
            properties: Map::new(),
        }
    }

    #[must_use]
    pub fn with_property(mut self, key: impl Into<String>, value: Value) -> Self {
        self.properties.insert(key.into(), value);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTraceRecord {
    pub session_id: String,
    pub sequence: u64,
    pub name: String,
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub attributes: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TelemetryEvent {
    HttpRequestStarted {
        session_id: String,
        attempt: u32,
        method: String,
        path: String,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        attributes: Map<String, Value>,
    },
    HttpRequestSucceeded {
        session_id: String,
        attempt: u32,
        method: String,
        path: String,
        status: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        attributes: Map<String, Value>,
    },
    HttpRequestFailed {
        session_id: String,
        attempt: u32,
        method: String,
        path: String,
        error: String,
        retryable: bool,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        attributes: Map<String, Value>,
    },
    HttpRequestDebug {
        session_id: String,
        timestamp_ms: u64,
        url: String,
        method: String,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        headers: Map<String, Value>,
        body: Value,
    },
    /// Token usage snapshot emitted after a streaming response completes.
    HttpResponseUsage {
        session_id: String,
        timestamp_ms: u64,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    },
    Analytics(AnalyticsEvent),
    SessionTrace(SessionTraceRecord),
    /// Emitted when a scode session starts.
    SessionStarted {
        session_id: String,
        timestamp_ms: u64,
        version: String,
        cwd: String,
        mode: String,
        model: String,
    },
    /// Emitted when a scode session ends.
    SessionEnded {
        session_id: String,
        timestamp_ms: u64,
        total_turns: u32,
        total_input_tokens: u64,
        total_output_tokens: u64,
        duration_ms: u64,
    },
}

pub trait TelemetrySink: Send + Sync {
    fn record(&self, event: TelemetryEvent);
}

#[derive(Default)]
pub struct MemoryTelemetrySink {
    events: Mutex<Vec<TelemetryEvent>>,
}

impl MemoryTelemetrySink {
    #[must_use]
    pub fn events(&self) -> Vec<TelemetryEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl TelemetrySink for MemoryTelemetrySink {
    fn record(&self, event: TelemetryEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

pub struct JsonlTelemetrySink {
    path: PathBuf,
    file: Mutex<File>,
}

impl Debug for JsonlTelemetrySink {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlTelemetrySink")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl JsonlTelemetrySink {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TelemetrySink for JsonlTelemetrySink {
    fn record(&self, event: TelemetryEvent) {
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// A telemetry sink that writes logs to sudoclaw.log or scode.log.
///
/// This sink detects the runtime mode:
/// - If `SCODE_LOG_PATH` environment variable is set, uses that path
/// - If `SUDOWORK_CHILD_PROCESS` is set, writes to ~/.nexus/logs/sudoclaw.log (child process mode)
/// - Otherwise, writes to ~/.nexus/logs/scode.log (standalone mode)
///
/// Supports log rotation when file exceeds 10MB, keeping up to 3 rotated files.
pub struct SudoclawLogSink {
    path: PathBuf,
    file: Mutex<File>,
}

impl Debug for SudoclawLogSink {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SudoclawLogSink")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SudoclawLogSink {
    /// Creates a new SudoclawLogSink with automatic path detection.
    pub fn new() -> Result<Self, std::io::Error> {
        let path = Self::resolve_log_path()?;
        Self::with_path(&path)
    }

    /// Creates a new SudoclawLogSink with a specific path.
    pub fn with_path(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Resolves the log file path based on environment variables and runtime mode.
    ///
    /// Priority:
    /// 1. `SCODE_LOG_PATH` environment variable (if set)
    /// 2. `SUDOWORK_CHILD_PROCESS` environment variable (if set) -> ~/.nexus/logs/sudoclaw.log
    /// 3. Default -> ~/.nexus/logs/scode.log
    fn resolve_log_path() -> Result<PathBuf, std::io::Error> {
        // Check for explicit log path override
        if let Ok(log_path) = std::env::var("SCODE_LOG_PATH") {
            return Ok(PathBuf::from(log_path));
        }

        // Determine log directory
        let log_dir = dirs::home_dir()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not determine home directory",
                )
            })?
            .join(".nexus")
            .join("logs");

        // Check if running as child process
        let log_filename = if std::env::var("SUDOWORK_CHILD_PROCESS").is_ok() {
            "sudoclaw.log"
        } else {
            "scode.log"
        };

        Ok(log_dir.join(log_filename))
    }

    /// Returns the path to the log file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rotates the log file, keeping up to SUDOCLAW_LOG_MAX_ROTATED_FILES rotated files.
    fn rotate_log_file(&self) -> Result<(), std::io::Error> {
        // Remove the oldest rotated file if it exists
        let oldest_rotated = format!("{}.{}", self.path.display(), SUDOCLAW_LOG_MAX_ROTATED_FILES);
        if Path::new(&oldest_rotated).exists() {
            std::fs::remove_file(&oldest_rotated)?;
        }

        // Shift existing rotated files
        for i in (1..=SUDOCLAW_LOG_MAX_ROTATED_FILES - 1).rev() {
            let current = format!("{}.{}", self.path.display(), i);
            let next = format!("{}.{}", self.path.display(), i + 1);
            if Path::new(&current).exists() {
                std::fs::rename(&current, &next)?;
            }
        }

        // Rename current log to .1
        let first_rotated = format!("{}.1", self.path.display());
        std::fs::rename(&self.path, &first_rotated)?;

        // Reopen the log file (it will be created on next write)
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut file_guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *file_guard = file;

        Ok(())
    }
}

impl TelemetrySink for SudoclawLogSink {
    fn record(&self, event: TelemetryEvent) {
        let log_entry = format_log_entry(&event);
        let Ok(json) = serde_json::to_string(&log_entry) else {
            return;
        };

        // Acquire lock first to prevent race condition during rotation check
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Check for rotation while holding the lock
        if let Ok(metadata) = std::fs::metadata(&self.path) {
            if metadata.len() >= SUDOCLAW_LOG_ROTATE_AFTER_BYTES {
                // Flush before rotation
                let _ = file.flush();
                // Release lock before rotation (rotate_log_file needs to acquire it)
                drop(file);

                // Perform rotation
                if let Err(e) = self.rotate_log_file() {
                    eprintln!("[scode telemetry] Failed to rotate log: {}", e);
                }

                // Re-acquire lock after rotation
                file = self
                    .file
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }

        if let Err(e) = writeln!(file, "{json}") {
            eprintln!("[scode telemetry] Failed to write log: {}", e);
        }
        if let Err(e) = file.flush() {
            eprintln!("[scode telemetry] Failed to flush log: {}", e);
        }
    }
}

/// Formats a TelemetryEvent into a structured log entry.
fn format_log_entry(event: &TelemetryEvent) -> Map<String, Value> {
    let mut entry = Map::new();

    entry.insert("timestamp".to_string(), Value::String(format_timestamp()));
    entry.insert("level".to_string(), Value::String("info".to_string()));

    let (session_id, event_name, attributes) = extract_event_info(event);

    entry.insert("session_id".to_string(), Value::String(session_id));
    entry.insert("component".to_string(), Value::String("scode".to_string()));
    entry.insert("event".to_string(), Value::String(event_name));

    if !attributes.is_empty() {
        entry.insert("attributes".to_string(), Value::Object(attributes));
    }

    entry
}

/// Extracts event information from a TelemetryEvent.
fn extract_event_info(event: &TelemetryEvent) -> (String, String, Map<String, Value>) {
    match event {
        TelemetryEvent::HttpRequestStarted {
            session_id,
            attempt,
            method,
            path,
            attributes,
        } => {
            let mut attrs = attributes.clone();
            attrs.insert("attempt".to_string(), Value::from(*attempt));
            attrs.insert("method".to_string(), Value::String(method.clone()));
            attrs.insert("path".to_string(), Value::String(path.clone()));
            (session_id.clone(), "request_started".to_string(), attrs)
        }
        TelemetryEvent::HttpRequestSucceeded {
            session_id,
            attempt,
            method,
            path,
            status,
            request_id,
            attributes,
        } => {
            let mut attrs = attributes.clone();
            attrs.insert("attempt".to_string(), Value::from(*attempt));
            attrs.insert("method".to_string(), Value::String(method.clone()));
            attrs.insert("path".to_string(), Value::String(path.clone()));
            attrs.insert("status".to_string(), Value::from(*status));
            if let Some(rid) = request_id {
                attrs.insert("request_id".to_string(), Value::String(rid.clone()));
            }
            (session_id.clone(), "request_succeeded".to_string(), attrs)
        }
        TelemetryEvent::HttpRequestFailed {
            session_id,
            attempt,
            method,
            path,
            error,
            retryable,
            attributes,
        } => {
            let mut attrs = attributes.clone();
            attrs.insert("attempt".to_string(), Value::from(*attempt));
            attrs.insert("method".to_string(), Value::String(method.clone()));
            attrs.insert("path".to_string(), Value::String(path.clone()));
            attrs.insert("error".to_string(), Value::String(error.clone()));
            attrs.insert("retryable".to_string(), Value::Bool(*retryable));
            (session_id.clone(), "request_failed".to_string(), attrs)
        }
        TelemetryEvent::HttpRequestDebug {
            session_id,
            timestamp_ms,
            url,
            method,
            headers,
            body,
        } => {
            let mut attrs = Map::new();
            attrs.insert("timestamp_ms".to_string(), Value::from(*timestamp_ms));
            attrs.insert("url".to_string(), Value::String(url.clone()));
            attrs.insert("method".to_string(), Value::String(method.clone()));
            attrs.insert("headers".to_string(), Value::Object(headers.clone()));
            attrs.insert("body".to_string(), body.clone());
            (session_id.clone(), "request_debug".to_string(), attrs)
        }
        TelemetryEvent::HttpResponseUsage {
            session_id,
            timestamp_ms,
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        } => {
            let mut attrs = Map::new();
            attrs.insert("timestamp_ms".to_string(), Value::from(*timestamp_ms));
            attrs.insert("input_tokens".to_string(), Value::from(*input_tokens));
            attrs.insert("output_tokens".to_string(), Value::from(*output_tokens));
            attrs.insert(
                "cache_creation_input_tokens".to_string(),
                Value::from(*cache_creation_input_tokens),
            );
            attrs.insert(
                "cache_read_input_tokens".to_string(),
                Value::from(*cache_read_input_tokens),
            );
            (session_id.clone(), "response_usage".to_string(), attrs)
        }
        TelemetryEvent::Analytics(event) => {
            let mut attrs = event.properties.clone();
            attrs.insert(
                "namespace".to_string(),
                Value::String(event.namespace.clone()),
            );
            attrs.insert("action".to_string(), Value::String(event.action.clone()));
            (String::new(), "event".to_string(), attrs)
        }
        TelemetryEvent::SessionTrace(record) => (
            record.session_id.clone(),
            record.name.clone(),
            record.attributes.clone(),
        ),
        TelemetryEvent::SessionStarted {
            session_id,
            timestamp_ms: _,
            version,
            cwd,
            mode,
            model,
        } => {
            let attrs = Map::from_iter([
                ("version".to_string(), Value::String(version.clone())),
                ("cwd".to_string(), Value::String(cwd.clone())),
                ("mode".to_string(), Value::String(mode.clone())),
                ("model".to_string(), Value::String(model.clone())),
            ]);
            (session_id.clone(), "session_started".to_string(), attrs)
        }
        TelemetryEvent::SessionEnded {
            session_id,
            timestamp_ms: _,
            total_turns,
            total_input_tokens,
            total_output_tokens,
            duration_ms,
        } => {
            let attrs = Map::from_iter([
                ("total_turns".to_string(), Value::from(*total_turns)),
                (
                    "total_input_tokens".to_string(),
                    Value::from(*total_input_tokens),
                ),
                (
                    "total_output_tokens".to_string(),
                    Value::from(*total_output_tokens),
                ),
                ("duration_ms".to_string(), Value::from(*duration_ms)),
            ]);
            (session_id.clone(), "session_ended".to_string(), attrs)
        }
    }
}

/// Formats the current timestamp in ISO 8601 format.
fn format_timestamp() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
}

#[derive(Clone)]
pub struct SessionTracer {
    session_id: String,
    sequence: Arc<AtomicU64>,
    sink: Arc<dyn TelemetrySink>,
}

impl Debug for SessionTracer {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTracer")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionTracer {
    #[must_use]
    pub fn new(session_id: impl Into<String>, sink: Arc<dyn TelemetrySink>) -> Self {
        Self {
            session_id: session_id.into(),
            sequence: Arc::new(AtomicU64::new(0)),
            sink,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn record(&self, name: impl Into<String>, attributes: Map<String, Value>) {
        let record = SessionTraceRecord {
            session_id: self.session_id.clone(),
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            timestamp_ms: current_timestamp_ms(),
            attributes,
        };
        self.sink.record(TelemetryEvent::SessionTrace(record));
    }

    pub fn record_http_request_started(
        &self,
        attempt: u32,
        method: impl Into<String>,
        path: impl Into<String>,
        attributes: Map<String, Value>,
    ) {
        let method = method.into();
        let path = path.into();
        self.sink.record(TelemetryEvent::HttpRequestStarted {
            session_id: self.session_id.clone(),
            attempt,
            method: method.clone(),
            path: path.clone(),
            attributes: attributes.clone(),
        });
        self.record(
            "http_request_started",
            merge_trace_fields(method, path, attempt, attributes),
        );
    }

    pub fn record_http_request_succeeded(
        &self,
        attempt: u32,
        method: impl Into<String>,
        path: impl Into<String>,
        status: u16,
        request_id: Option<String>,
        attributes: Map<String, Value>,
    ) {
        let method = method.into();
        let path = path.into();
        self.sink.record(TelemetryEvent::HttpRequestSucceeded {
            session_id: self.session_id.clone(),
            attempt,
            method: method.clone(),
            path: path.clone(),
            status,
            request_id: request_id.clone(),
            attributes: attributes.clone(),
        });
        let mut trace_attributes = merge_trace_fields(method, path, attempt, attributes);
        trace_attributes.insert("status".to_string(), Value::from(status));
        if let Some(request_id) = request_id {
            trace_attributes.insert("request_id".to_string(), Value::String(request_id));
        }
        self.record("http_request_succeeded", trace_attributes);
    }

    pub fn record_http_request_failed(
        &self,
        attempt: u32,
        method: impl Into<String>,
        path: impl Into<String>,
        error: impl Into<String>,
        retryable: bool,
        attributes: Map<String, Value>,
    ) {
        let method = method.into();
        let path = path.into();
        let error = error.into();
        self.sink.record(TelemetryEvent::HttpRequestFailed {
            session_id: self.session_id.clone(),
            attempt,
            method: method.clone(),
            path: path.clone(),
            error: error.clone(),
            retryable,
            attributes: attributes.clone(),
        });
        let mut trace_attributes = merge_trace_fields(method, path, attempt, attributes);
        trace_attributes.insert("error".to_string(), Value::String(error));
        trace_attributes.insert("retryable".to_string(), Value::Bool(retryable));
        self.record("http_request_failed", trace_attributes);
    }

    pub fn record_http_request_debug(
        &self,
        url: impl Into<String>,
        method: impl Into<String>,
        headers: Map<String, Value>,
        body: Value,
    ) {
        self.sink.record(TelemetryEvent::HttpRequestDebug {
            session_id: self.session_id.clone(),
            timestamp_ms: current_timestamp_ms(),
            url: url.into(),
            method: method.into(),
            headers,
            body,
        });
    }

    pub fn record_usage(
        &self,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    ) {
        self.sink.record(TelemetryEvent::HttpResponseUsage {
            session_id: self.session_id.clone(),
            timestamp_ms: current_timestamp_ms(),
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
        });
    }

    pub fn record_analytics(&self, event: AnalyticsEvent) {
        let mut attributes = event.properties.clone();
        attributes.insert(
            "namespace".to_string(),
            Value::String(event.namespace.clone()),
        );
        attributes.insert("action".to_string(), Value::String(event.action.clone()));
        self.sink.record(TelemetryEvent::Analytics(event));
        self.record("analytics", attributes);
    }

    pub fn record_session_started(
        &self,
        version: impl Into<String>,
        cwd: impl Into<String>,
        mode: impl Into<String>,
        model: impl Into<String>,
    ) {
        let version = version.into();
        let cwd = cwd.into();
        let mode = mode.into();
        let model = model.into();
        self.sink.record(TelemetryEvent::SessionStarted {
            session_id: self.session_id.clone(),
            timestamp_ms: current_timestamp_ms(),
            version: version.clone(),
            cwd: cwd.clone(),
            mode: mode.clone(),
            model: model.clone(),
        });
        let mut attributes = Map::new();
        attributes.insert("version".to_string(), Value::String(version));
        attributes.insert("cwd".to_string(), Value::String(cwd));
        attributes.insert("mode".to_string(), Value::String(mode));
        attributes.insert("model".to_string(), Value::String(model));
        self.record("session_started", attributes);
    }

    pub fn record_session_ended(
        &self,
        total_turns: u32,
        total_input_tokens: u64,
        total_output_tokens: u64,
        duration_ms: u64,
    ) {
        self.sink.record(TelemetryEvent::SessionEnded {
            session_id: self.session_id.clone(),
            timestamp_ms: current_timestamp_ms(),
            total_turns,
            total_input_tokens,
            total_output_tokens,
            duration_ms,
        });
        let mut attributes = Map::new();
        attributes.insert("total_turns".to_string(), Value::from(total_turns));
        attributes.insert(
            "total_input_tokens".to_string(),
            Value::from(total_input_tokens),
        );
        attributes.insert(
            "total_output_tokens".to_string(),
            Value::from(total_output_tokens),
        );
        attributes.insert("duration_ms".to_string(), Value::from(duration_ms));
        self.record("session_ended", attributes);
    }

    /// Record a prompt error that occurred during a turn.
    pub fn record_prompt_error(&self, error_type: impl Into<String>, error_message: impl Into<String>) {
        let error_type = error_type.into();
        let error_message = error_message.into();
        let mut attributes = Map::new();
        attributes.insert("error_type".to_string(), Value::String(error_type));
        attributes.insert("error_message".to_string(), Value::String(error_message));
        self.record("prompt_error", attributes);
    }
}

/// Mask sensitive header values for debug capture logs.
/// Authorization and x-api-key headers are redacted.
#[must_use]
pub fn mask_sensitive_headers(headers: &[(String, String)]) -> Map<String, Value> {
    let mut map = Map::new();
    for (key, value) in headers {
        let lower = key.to_ascii_lowercase();
        let masked = if lower == "authorization" || lower == "x-api-key" {
            mask_credential_value(value)
        } else {
            value.clone()
        };
        map.insert(key.clone(), Value::String(masked));
    }
    map
}

fn mask_credential_value(value: &str) -> String {
    let trimmed = value.trim();
    if let Some(token) = trimmed.strip_prefix("Bearer ") {
        let visible = &token[..token.len().min(10)];
        return format!("Bearer {visible}... (hidden)");
    }
    let visible = &trimmed[..trimmed.len().min(10)];
    format!("{visible}... (hidden)")
}

fn merge_trace_fields(
    method: String,
    path: String,
    attempt: u32,
    mut attributes: Map<String, Value>,
) -> Map<String, Value> {
    attributes.insert("method".to_string(), Value::String(method));
    attributes.insert("path".to_string(), Value::String(path));
    attributes.insert("attempt".to_string(), Value::from(attempt));
    attributes
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn request_profile_emits_headers_and_merges_body() {
        let profile = AnthropicRequestProfile::new(
            ClientIdentity::new("claude-code", "1.2.3").with_runtime("rust-cli"),
        )
        .with_beta("tools-2026-04-01")
        .with_extra_body("metadata", serde_json::json!({"source": "test"}));

        assert_eq!(
            profile.header_pairs(),
            vec![
                (
                    "anthropic-version".to_string(),
                    DEFAULT_ANTHROPIC_VERSION.to_string()
                ),
                ("user-agent".to_string(), "claude-code/1.2.3".to_string()),
                (
                    "anthropic-beta".to_string(),
                    "claude-code-20250219,prompt-caching-scope-2026-01-05,tools-2026-04-01"
                        .to_string(),
                ),
            ]
        );

        let body = profile
            .render_json_body(&serde_json::json!({"model": "claude-sonnet"}))
            .expect("body should serialize");
        assert_eq!(
            body["metadata"]["source"],
            Value::String("test".to_string())
        );
        assert_eq!(
            body["betas"],
            serde_json::json!([
                "claude-code-20250219",
                "prompt-caching-scope-2026-01-05",
                "tools-2026-04-01"
            ])
        );
    }

    #[test]
    fn session_tracer_records_structured_events_and_trace_sequence() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-123", sink.clone());

        tracer.record_http_request_started(1, "POST", "/v1/messages", Map::new());
        tracer.record_analytics(
            AnalyticsEvent::new("cli", "prompt_sent")
                .with_property("model", Value::String("claude-opus".to_string())),
        );

        let events = sink.events();
        assert!(matches!(
            &events[0],
            TelemetryEvent::HttpRequestStarted {
                session_id,
                attempt: 1,
                method,
                path,
                ..
            } if session_id == "session-123" && method == "POST" && path == "/v1/messages"
        ));
        assert!(matches!(
            &events[1],
            TelemetryEvent::SessionTrace(SessionTraceRecord { sequence: 0, name, .. })
            if name == "http_request_started"
        ));
        assert!(matches!(&events[2], TelemetryEvent::Analytics(_)));
        assert!(matches!(
            &events[3],
            TelemetryEvent::SessionTrace(SessionTraceRecord { sequence: 1, name, .. })
            if name == "analytics"
        ));
    }

    #[test]
    fn jsonl_sink_persists_events() {
        let path =
            std::env::temp_dir().join(format!("telemetry-jsonl-{}.log", current_timestamp_ms()));
        let sink = JsonlTelemetrySink::new(&path).expect("sink should create file");

        sink.record(TelemetryEvent::Analytics(
            AnalyticsEvent::new("cli", "turn_completed").with_property("ok", Value::Bool(true)),
        ));

        let contents = std::fs::read_to_string(&path).expect("telemetry log should be readable");
        assert!(contents.contains("\"type\":\"analytics\""));
        assert!(contents.contains("\"action\":\"turn_completed\""));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    #[serial]
    fn sudoclaw_log_sink_resolves_child_process_path() {
        std::env::set_var("SUDOWORK_CHILD_PROCESS", "1");
        let path = SudoclawLogSink::resolve_log_path().expect("should resolve path");
        assert!(path.to_string_lossy().contains("sudoclaw.log"));
        std::env::remove_var("SUDOWORK_CHILD_PROCESS");
    }

    #[test]
    #[serial]
    fn sudoclaw_log_sink_resolves_standalone_path() {
        std::env::remove_var("SUDOWORK_CHILD_PROCESS");
        std::env::remove_var("SCODE_LOG_PATH");
        let path = SudoclawLogSink::resolve_log_path().expect("should resolve path");
        assert!(path.to_string_lossy().contains("scode.log"));
    }

    #[test]
    #[serial]
    fn sudoclaw_log_sink_respects_env_override() {
        std::env::set_var("SCODE_LOG_PATH", "/custom/path.log");
        let path = SudoclawLogSink::resolve_log_path().expect("should resolve path");
        assert_eq!(path.to_string_lossy(), "/custom/path.log");
        std::env::remove_var("SCODE_LOG_PATH");
    }

    #[test]
    fn sudoclaw_log_sink_writes_json_events() {
        let temp_dir = std::env::temp_dir();
        let log_path = temp_dir.join(format!("test-sudoclaw-{}.log", current_timestamp_ms()));

        let sink = SudoclawLogSink::with_path(&log_path).expect("sink should create file");
        sink.record(TelemetryEvent::SessionStarted {
            session_id: "test-session".to_string(),
            timestamp_ms: 1234567890,
            version: "0.1.0".to_string(),
            cwd: "/test".to_string(),
            mode: "standalone".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        });

        let contents = std::fs::read_to_string(&log_path).expect("log should be readable");
        assert!(contents.contains("\"event\":\"session_started\""));
        assert!(contents.contains("\"session_id\":\"test-session\""));
        assert!(contents.contains("\"model\":\"claude-sonnet-4-6\""));

        let _ = std::fs::remove_file(log_path);
    }

    #[test]
    fn format_log_entry_produces_valid_json() {
        let event = TelemetryEvent::HttpRequestStarted {
            session_id: "session-123".to_string(),
            attempt: 1,
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            attributes: Map::new(),
        };

        let entry = format_log_entry(&event);
        let json = serde_json::to_string(&entry).expect("should serialize to JSON");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("should be valid JSON");

        assert_eq!(parsed["level"], "info");
        assert_eq!(parsed["session_id"], "session-123");
        assert_eq!(parsed["event"], "request_started");
        assert_eq!(parsed["component"], "scode");
    }
}
