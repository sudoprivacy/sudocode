//! ACP server implementation using the official `agent-client-protocol` SDK.
//!
//! This module provides an SDK-based ACP server with full ACP 1.0 compliance
//! including capabilities declaration, session cancel, permission-mode switching,
//! model switching, image input, and permission-prompt bridging (elicitation).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc as StdArc;
use std::sync::{Arc, Mutex};

use agent_client_protocol::role::acp::{Agent, Client};
// NOTE: `ConnectTo` and `ConnectionTo` are different SDK concepts:
//   - `ConnectTo<R>`:    trait for wiring up a transport (Stdio, Lines, etc.)
//   - `ConnectionTo<R>`: runtime handle passed to handlers for sending messages
use crate::config::{ConfigSource, McpServerConfig, McpStdioServerConfig, ScopedMcpServerConfig};
use crate::conversation::RuntimeObserver;
use crate::hooks::HookAbortSignal;
use crate::permissions::{
    PermissionMode, PermissionPromptDecision, PermissionPrompter, PermissionRequest,
    QuestionPromptAnswer, QuestionPromptRequest, QuestionPrompter,
};
use crate::usage::UsageCostCurrency;
use agent_client_protocol::{
    on_receive_dispatch, on_receive_notification, on_receive_request, ConnectTo, ConnectionTo,
    Dispatch, Error, Handled, JsonRpcRequest, JsonRpcResponse, Responder,
};
use agent_client_protocol_schema::{
    AgentCapabilities, CancelNotification, ClientRequest, CloseSessionRequest,
    CloseSessionResponse, ContentBlock, ContentChunk, ExtRequest, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, McpServer, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionId, PermissionOptionKind, PromptCapabilities, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionInfo, SessionNotification, SessionUpdate,
    SetSessionModelRequest, SetSessionModelResponse, StopReason, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map};
use std::collections::BTreeMap;

/// Error type returned by ACP agent implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpError {
    InvalidParams(String),
    Internal(String),
}

impl AcpError {
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::InvalidParams(message.into())
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Generate a user-friendly error message with actionable suggestions.
    #[must_use]
    pub fn user_friendly_message(&self) -> String {
        let raw_message = match self {
            Self::InvalidParams(msg) | Self::Internal(msg) => msg,
        };

        if raw_message.contains("[context_window_exceeded]") {
            return raw_message.clone();
        }

        // Check for specific error types and provide friendly messages
        if raw_message.contains("context_window_blocked")
            || raw_message.contains("Context window blocked")
        {
            return "[context_window_exceeded][single_request_too_large] 图片或文本内容过大，超出了模型的处理限制。\n\n建议解决方案：\n1. 使用较小的图片（建议压缩或缩小图片尺寸）\n2. 简化输入内容\n3. 使用支持更大上下文的模型\n4. 清除对话历史后重新开始".to_string();
        }

        if raw_message.contains("authentication")
            || raw_message.contains("认证失败")
            || raw_message.contains("AUTH")
        {
            return "认证失败，请检查您的账户配置。\n\n建议解决方案：\n1. 检查 API 密钥或订阅是否有效\n2. 重新登录账户\n3. 检查网络连接".to_string();
        }

        if raw_message.contains("timeout")
            || raw_message.contains("Timeout")
            || raw_message.contains("timed out")
        {
            return "请求超时，模型响应时间过长。\n\n建议解决方案：\n1. 简化输入内容\n2. 检查网络连接\n3. 稍后重试".to_string();
        }

        if raw_message.contains("rate limit")
            || raw_message.contains("RateLimit")
            || raw_message.contains("429")
        {
            return "请求频率过高，请稍后重试。\n\n建议解决方案：\n1. 等待几分钟后重试\n2. 减少请求频率".to_string();
        }

        if raw_message.contains("network")
            || raw_message.contains("connection")
            || raw_message.contains("Connection")
        {
            return "网络连接出现问题。\n\n建议解决方案：\n1. 检查网络连接\n2. 检查代理设置\n3. 稍后重试".to_string();
        }

        if raw_message.contains("permission") || raw_message.contains("Permission") {
            return "权限不足，无法执行此操作。\n\n建议解决方案：\n1. 检查文件或目录权限\n2. 检查账户权限配置".to_string();
        }

        // Default: return a simplified message
        if raw_message.len() > 200 {
            format!(
                "发生错误：{}\n\n请尝试简化输入或稍后重试。",
                raw_message.chars().take(100).collect::<String>()
            )
        } else {
            format!("发生错误：{}\n\n请尝试简化输入或稍后重试。", raw_message)
        }
    }
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParams(message) | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AcpError {}

/// Configuration for the SDK-based ACP server.
#[derive(Debug, Clone)]
pub struct SdkAcpConfig {
    pub agent_version: String,
    pub model: String,
    pub model_flag_raw: Option<String>,
    pub permission_mode_override: Option<PermissionMode>,
    pub reasoning_effort: Option<String>,
}

// ---------------------------------------------------------------------------
// Custom extension: session/setPermissionMode (not in ACP SDK schema)
// ---------------------------------------------------------------------------

/// Request to change the permission mode for a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "session/setPermissionMode", response = SetPermissionModeResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetPermissionModeRequest {
    pub session_id: String,
    pub permission_mode: String,
}

/// Response to a permission mode change.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
pub(crate) struct SetPermissionModeResponse {}

/// Convert the ACP `mcp_servers` carried by `session/new` / `session/load`
/// into scode-internal scoped MCP configs, keyed by server name. The session
/// `cwd` becomes each stdio server's `current_dir` so relative `command`
/// paths resolve against the session working directory.
///
/// Only the `Stdio` variant is loaded. `Http`/`Sse` log a warning and are
/// skipped; any future variant is silently skipped.
fn acp_mcp_servers_to_scoped(
    acp_servers: &[McpServer],
    cwd: &std::path::Path,
) -> BTreeMap<String, ScopedMcpServerConfig> {
    let mut out = BTreeMap::new();
    for server in acp_servers {
        match server {
            McpServer::Stdio(stdio) => {
                let env = stdio
                    .env
                    .iter()
                    .map(|variable| (variable.name.clone(), variable.value.clone()))
                    .collect();
                let config = McpServerConfig::Stdio(McpStdioServerConfig {
                    command: stdio.command.to_string_lossy().into_owned(),
                    args: stdio.args.clone(),
                    env,
                    current_dir: Some(cwd.to_path_buf()),
                    tool_call_timeout_ms: None,
                });
                out.insert(
                    stdio.name.clone(),
                    ScopedMcpServerConfig {
                        scope: ConfigSource::Local,
                        config,
                    },
                );
            }
            McpServer::Http(_) | McpServer::Sse(_) => {
                eprintln!(
                    "[acp] session mcp_servers: http/sse transport skipped (scode loads stdio MCP only)"
                );
            }
            _ => {}
        }
    }
    out
}

/// Callback trait that the CLI crate implements to provide session
/// construction and prompt execution, keeping runtime/provider deps out of
/// this crate.
///
/// Every method takes `&self`: the delegate is shared across all in-flight
/// ACP requests (see [`SharedDelegate`]) and is responsible for its own
/// interior locking. The contract the ACP server relies on is
///
/// * calls on **different** sessions may run concurrently and must not
///   block each other — a session that is parked on a permission prompt or
///   an `AskUserQuestion` must not stall the others;
/// * calls on the **same** session are already serialized by the server
///   (see [`SessionRegistry`]), so the delegate only needs per-session
///   locking for memory safety, not for ordering.
pub trait SdkAcpDelegate: Send + Sync + 'static {
    /// Create a new session for the given working directory, returning
    /// `(session_id, cwd, abort_signal)` on success.
    fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: BTreeMap<String, ScopedMcpServerConfig>,
    ) -> Result<(String, PathBuf, HookAbortSignal), AcpError>;

    /// Run a prompt turn. The implementation should call observer methods
    /// to stream session updates.
    fn run_prompt(
        &self,
        session_id: &str,
        prompt: String,
        observer: &mut SdkSessionObserver,
        trace_id: Option<&str>,
    ) -> Result<(StopReason, Option<PromptUsage>), AcpError>;

    /// Run a prompt with permission prompting bridged to the ACP client.
    fn run_prompt_with_prompter(
        &self,
        session_id: &str,
        prompt: String,
        observer: &mut SdkSessionObserver,
        prompter: &mut dyn PermissionPrompter,
        trace_id: Option<&str>,
    ) -> Result<(StopReason, Option<PromptUsage>), AcpError>;

    /// Install a question prompter for AskUserQuestion tool execution within a session.
    fn set_question_prompter(
        &self,
        session_id: &str,
        prompter: Box<dyn QuestionPrompter>,
    ) -> Result<(), AcpError>;

    /// Handle a slash command, returning text output.
    fn handle_slash_command(
        &self,
        session_id: &str,
        input: &str,
        observer: &mut SdkSessionObserver,
    ) -> Result<(), AcpError>;

    /// List active session IDs with their cwds.
    fn list_sessions(&self) -> Vec<(String, PathBuf)>;

    /// Close (drop) a session by ID. Returns true if it existed.
    fn close_session(&self, session_id: &str) -> bool;

    /// Switch the model for a session. Returns a human-readable report.
    fn set_model(&self, session_id: &str, model_id: &str) -> Result<String, AcpError>;

    /// Return the current model ID and available models.
    fn get_model_info(&self) -> (String, Vec<String>);

    /// Change the permission mode for a session.
    fn set_permission_mode(&self, session_id: &str, mode: PermissionMode) -> Result<(), AcpError>;

    /// Push image content blocks into a session before running a prompt.
    fn push_images(&self, session_id: &str, images: &[(String, String)]) -> Result<(), AcpError>;

    /// Load an existing persisted session by its ID and working directory,
    /// returning `(session_id, cwd, abort_signal)` on success.
    fn load_session(
        &self,
        session_id: &str,
        cwd: PathBuf,
        mcp_servers: BTreeMap<String, ScopedMcpServerConfig>,
    ) -> Result<(String, PathBuf, HookAbortSignal), AcpError>;
}

/// Observer that streams session update notifications to the ACP client in
/// real time via a channel. Implements [`RuntimeObserver`] so existing
/// `run_turn()` machinery can drive it.
pub struct SdkSessionObserver {
    session_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
}

impl SdkSessionObserver {
    /// Create a new observer that sends notifications through `tx`.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            tx,
        }
    }

    fn push(&mut self, update: SessionUpdate) {
        let _ = self
            .tx
            .send(SessionNotification::new(self.session_id.clone(), update));
    }
}

impl RuntimeObserver for SdkSessionObserver {
    fn on_thinking_delta(&mut self, delta: &str) {
        self.push(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(delta)),
        )));
    }

    fn on_text_delta(&mut self, delta: &str) {
        self.push(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(delta)),
        )));
    }

    fn on_tool_use(&mut self, id: &str, name: &str, input: &str) {
        let id_owned = id.to_owned();
        let name_owned = name.to_owned();
        let raw_input = serde_json::from_str(input)
            .unwrap_or_else(|_| serde_json::Value::String(input.to_owned()));
        self.push(SessionUpdate::ToolCall(
            ToolCall::new(id_owned, name_owned)
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        ));
    }

    fn on_tool_result(
        &mut self,
        tool_use_id: &str,
        _tool_name: &str,
        output: &str,
        is_error: bool,
    ) {
        let id_owned = tool_use_id.to_owned();
        let raw_output = serde_json::from_str(output)
            .unwrap_or_else(|_| serde_json::Value::String(output.to_owned()));
        let status = if is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        self.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id_owned,
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(raw_output),
        )));
    }
}

/// Sniff the MIME type of a base64-encoded image from its leading bytes.
///
/// Inspects the first few characters of the base64 data to detect the format.
/// Falls back to `image/png` when the prefix is unrecognised.
pub(crate) fn sniff_image_mime(base64_data: &str) -> &'static str {
    if base64_data.starts_with("iVBOR") {
        "image/png"
    } else if base64_data.starts_with("/9j/") {
        "image/jpeg"
    } else if base64_data.starts_with("R0lGO") {
        "image/gif"
    } else if base64_data.starts_with("UklGR") {
        "image/webp"
    } else {
        "image/png"
    }
}

/// Extract plain text from a slice of ACP `ContentBlock`s. Image blocks are
/// tracked separately and returned as `(text, images)`.
pub(crate) fn extract_content_from_blocks(
    blocks: &[ContentBlock],
) -> Result<(String, Vec<(String, String)>), AcpError> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(tc) => {
                let t = tc.text.trim();
                if !t.is_empty() {
                    texts.push(t.to_owned());
                }
            }
            ContentBlock::Image(ic) => {
                let mime = if ic.mime_type.is_empty() {
                    sniff_image_mime(&ic.data).to_owned()
                } else {
                    ic.mime_type.clone()
                };
                images.push((ic.data.clone(), mime));
            }
            _ => {}
        }
    }
    if texts.is_empty() && images.is_empty() {
        return Err(AcpError::invalid_params(
            "prompt must include at least one non-empty text or image content block",
        ));
    }
    Ok((texts.join("\n"), images))
}

/// Re-export `StopReason` so the CLI crate doesn't need a direct dep on
/// the schema crate.
pub use agent_client_protocol_schema::StopReason as AcpStopReason;

/// Token usage data returned by a prompt turn.
#[derive(Debug, Clone, Default)]
pub struct PromptUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    pub estimated_session_tokens: Option<u64>,
    pub cost_units: Option<u64>,
    pub cost_currency: Option<UsageCostCurrency>,
    /// Cumulative usage for the entire session, exposed via _meta.sudocode.cumulativeUsage
    pub cumulative_usage: Option<CumulativeUsage>,
}

/// Cumulative token usage for the entire session.
#[derive(Debug, Clone, Default)]
pub struct CumulativeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
}

/// Build the `_meta` map for the `initialize` response. Currently advertises
/// sudocode's image-handling capability under `_meta.sudocode.imageCapability`
/// so ACP clients (sudowork) can downsample / route around oversized + wrong-
/// model image cases without surfacing a user-visible error.
///
/// See [`crate::image_registry::capability`] for the source of truth; design
/// rationale in `docs/design/image-handling-non-user-facing.html`.
fn initialize_meta() -> Map<String, serde_json::Value> {
    let cap = crate::image_registry::capability();
    let mut sudocode_ns = Map::new();
    sudocode_ns.insert(
        "imageCapability".to_string(),
        json!({
            "maxBytes": cap.max_bytes,
            "maxDimension": cap.max_dimension,
            "downsampleTargetBytes": cap.downsample_target_bytes,
            "autoHandlesOversized": cap.auto_handles_oversized,
            "autoHandlesWrongModel": cap.auto_handles_wrong_model,
        }),
    );
    let mut meta = Map::new();
    meta.insert("sudocode".to_string(), json!(sudocode_ns));
    meta
}

fn sudocode_meta_from_prompt_usage(u: &PromptUsage) -> Map<String, serde_json::Value> {
    let mut sudocode_meta = Map::new();
    sudocode_meta.insert(
        "contextWindowTokens".to_string(),
        json!(u.context_window_tokens),
    );
    sudocode_meta.insert(
        "estimatedSessionTokens".to_string(),
        json!(u.estimated_session_tokens),
    );
    if let Some(cost_units) = u.cost_units {
        sudocode_meta.insert("costUnits".to_string(), json!(cost_units));
    }
    if let Some(cost_currency) = u.cost_currency {
        sudocode_meta.insert("costCurrency".to_string(), json!(cost_currency.as_str()));
    }
    if let Some(cumulative) = &u.cumulative_usage {
        sudocode_meta.insert(
            "cumulativeUsage".to_string(),
            json!({
                "inputTokens": cumulative.input_tokens,
                "outputTokens": cumulative.output_tokens,
                "totalTokens": cumulative.total_tokens,
                "cachedReadTokens": cumulative.cached_read_tokens,
                "cachedWriteTokens": cumulative.cached_write_tokens,
            }),
        );
    }
    sudocode_meta
}

#[cfg(test)]
mod tests {
    use super::{
        acp_mcp_servers_to_scoped, sudocode_meta_from_prompt_usage, CumulativeUsage, PromptUsage,
        SdkSessionObserver,
    };
    use crate::config::{ConfigSource, McpServerConfig};
    use crate::conversation::RuntimeObserver;
    use crate::usage::UsageCostCurrency;
    use agent_client_protocol_schema::{
        ContentBlock, EnvVariable, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
        SessionUpdate,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn prompt_usage_meta_includes_cost_without_standard_usage_tokens() {
        let meta = sudocode_meta_from_prompt_usage(&PromptUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            cache_read_tokens: Some(3),
            cache_write_tokens: Some(0),
            context_window_tokens: Some(200_000),
            estimated_session_tokens: Some(42),
            cost_units: Some(43_700),
            cost_currency: Some(UsageCostCurrency::SudoPoint),
            cumulative_usage: Some(CumulativeUsage {
                input_tokens: 10,
                output_tokens: 4,
                total_tokens: 14,
                cached_read_tokens: Some(3),
                cached_write_tokens: Some(0),
            }),
        });

        assert_eq!(meta["costUnits"], serde_json::json!(43_700));
        assert_eq!(meta["costCurrency"], serde_json::json!("sudo_point"));
        assert!(meta.get("totalTokens").is_none());
        assert_eq!(
            meta["cumulativeUsage"]["totalTokens"],
            serde_json::json!(14)
        );
    }

    #[test]
    fn sdk_session_observer_streams_thinking_as_agent_thought_chunk() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut observer = SdkSessionObserver::new("session-1", tx);

        observer.on_thinking_delta("hidden reasoning");

        let notification = rx.try_recv().expect("thought notification");
        assert_eq!(&*notification.session_id.0, "session-1");
        match notification.update {
            SessionUpdate::AgentThoughtChunk(chunk) => match chunk.content {
                ContentBlock::Text(text) => {
                    assert_eq!(text.text, "hidden reasoning");
                }
                other => panic!("expected text thought chunk, got {other:?}"),
            },
            other => panic!("expected agent thought chunk, got {other:?}"),
        }
    }

    #[test]
    fn acp_mcp_servers_empty() {
        let out = acp_mcp_servers_to_scoped(&[], Path::new("/tmp"));
        assert!(out.is_empty());
    }

    #[test]
    fn acp_mcp_servers_stdio() {
        let cwd = Path::new("/session/cwd");
        let server = McpServer::Stdio(
            McpServerStdio::new("srv", PathBuf::from("/bin/echo"))
                .args(vec!["a".to_string(), "b".to_string()])
                .env(vec![EnvVariable::new("K", "v")]),
        );
        let out = acp_mcp_servers_to_scoped(&[server], cwd);
        assert_eq!(out.len(), 1);
        let scoped = &out["srv"];
        assert_eq!(scoped.scope, ConfigSource::Local);
        let McpServerConfig::Stdio(stdio) = &scoped.config else {
            panic!("expected stdio config");
        };
        assert_eq!(stdio.command, "/bin/echo");
        assert_eq!(stdio.args, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(stdio.env.get("K"), Some(&"v".to_string()));
        assert_eq!(stdio.current_dir.as_deref(), Some(cwd));
        assert!(stdio.tool_call_timeout_ms.is_none());
    }

    #[test]
    fn acp_mcp_servers_env() {
        let server = McpServer::Stdio(McpServerStdio::new("srv", PathBuf::from("/bin/x")).env(
            vec![
                EnvVariable::new("A", "1"),
                EnvVariable::new("B", "2"),
                EnvVariable::new("C", "3"),
            ],
        ));
        let out = acp_mcp_servers_to_scoped(&[server], Path::new("/tmp"));
        let McpServerConfig::Stdio(stdio) = &out["srv"].config else {
            panic!("expected stdio");
        };
        assert_eq!(stdio.env.len(), 3);
        assert_eq!(stdio.env.get("A"), Some(&"1".to_string()));
        assert_eq!(stdio.env.get("B"), Some(&"2".to_string()));
        assert_eq!(stdio.env.get("C"), Some(&"3".to_string()));
    }

    #[test]
    fn acp_mcp_servers_skips_http_sse() {
        let http = McpServer::Http(McpServerHttp::new("h", "https://e"));
        let sse = McpServer::Sse(McpServerSse::new("s", "https://e"));
        let out = acp_mcp_servers_to_scoped(&[http, sse], Path::new("/tmp"));
        assert!(out.is_empty());
    }

    #[test]
    fn acp_mcp_servers_mixed() {
        let stdio = McpServer::Stdio(McpServerStdio::new("keep", PathBuf::from("/bin/k")));
        let http = McpServer::Http(McpServerHttp::new("drop", "https://e"));
        let out = acp_mcp_servers_to_scoped(&[stdio, http], Path::new("/tmp"));
        assert_eq!(out.len(), 1);
        assert!(out.contains_key("keep"));
        assert!(!out.contains_key("drop"));
    }
}

/// Thread-safe handle to a delegate, shared across async handlers.
///
/// There is deliberately **no** server-side mutex around the delegate: a
/// process-wide lock held for the duration of `session/prompt` is exactly
/// what made one parked session block every other session. Cross-session
/// concurrency is the delegate's responsibility ([`SdkAcpDelegate`]);
/// same-session ordering is the server's ([`SessionRegistry`] lanes).
pub type SharedDelegate = Arc<dyn SdkAcpDelegate>;

/// Shared handle to the [`SessionRegistry`].
pub type SharedSessionRegistry = Arc<SessionRegistry>;

/// Create a new empty session registry. Share this across connections so
/// that cancel notifications on a reconnected transport can still reach
/// sessions created on a previous connection, and so that per-session
/// ordering holds across connections too.
#[must_use]
pub fn new_session_registry() -> SharedSessionRegistry {
    Arc::new(SessionRegistry::default())
}

/// Per-process registry of live ACP sessions.
///
/// It carries three things the server needs *outside* the delegate:
///
/// * the [`HookAbortSignal`] so `session/cancel` fires without touching any
///   session lock (a running turn must be cancellable);
/// * a per-session **lane** (an async mutex) that serializes every
///   session-scoped request — `session/prompt`, `session/setPermissionMode`,
///   `session/setModel`, `session/close` — for that session while leaving
///   other sessions free to run. Two prompts on one session interleaving
///   their JSON-RPC traffic would corrupt the protocol, so this ordering is a
///   hard invariant, not a performance choice;
/// * the working directory of each session, which feeds the
///   [`WorkspaceCwdLease`] (see there for why that is needed).
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    cwd_lease: Arc<WorkspaceCwdLease>,
}

struct SessionEntry {
    abort: HookAbortSignal,
    lane: Arc<tokio::sync::Mutex<()>>,
    cwd: PathBuf,
}

/// Async guard for a session lane; drop it to let the next request on the
/// same session proceed.
pub type SessionLaneGuard = tokio::sync::OwnedMutexGuard<()>;

impl SessionRegistry {
    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register (or re-register) a session after `session/new` / `session/load`.
    pub fn register(&self, session_id: String, abort: HookAbortSignal, cwd: PathBuf) {
        let mut sessions = self.lock_sessions();
        // A reload of a session that is already live keeps its lane so that
        // requests already queued behind it stay ordered.
        let lane = sessions.get(&session_id).map_or_else(
            || Arc::new(tokio::sync::Mutex::new(())),
            |e| Arc::clone(&e.lane),
        );
        sessions.insert(session_id, SessionEntry { abort, lane, cwd });
    }

    /// Forget a session after `session/close`.
    pub fn remove(&self, session_id: &str) {
        self.lock_sessions().remove(session_id);
    }

    /// Abort signal for `session/cancel`; `None` for unknown sessions.
    #[must_use]
    pub fn abort_signal(&self, session_id: &str) -> Option<HookAbortSignal> {
        self.lock_sessions()
            .get(session_id)
            .map(|e| e.abort.clone())
    }

    /// Working directory a session was created / loaded with.
    #[must_use]
    pub fn cwd(&self, session_id: &str) -> Option<PathBuf> {
        self.lock_sessions().get(session_id).map(|e| e.cwd.clone())
    }

    /// Wait for the session's lane. Requests on the same session are served
    /// in arrival order (tokio's mutex is FIFO-fair); requests on other
    /// sessions are unaffected. Unknown sessions get no lane — the delegate
    /// will reject them with `unknown sessionId` — so the caller does not
    /// have to special-case them.
    pub async fn enter_lane(&self, session_id: &str) -> Option<SessionLaneGuard> {
        let lane = self
            .lock_sessions()
            .get(session_id)
            .map(|e| Arc::clone(&e.lane))?;
        Some(lane.lock_owned().await)
    }

    /// The process-wide working-directory lease.
    #[must_use]
    pub fn cwd_lease(&self) -> Arc<WorkspaceCwdLease> {
        Arc::clone(&self.cwd_lease)
    }
}

// ---------------------------------------------------------------------------
// Process working-directory lease
// ---------------------------------------------------------------------------

/// Arbiter for the *process* working directory.
///
/// The conversation runtime resolves relative paths, spawns `bash`, runs
/// hooks and reads project config against `std::env::current_dir()`; a
/// session's turn therefore has to run with the process cwd set to that
/// session's cwd. That is a process-wide resource, so once turns of
/// different sessions run concurrently something has to keep them from
/// flipping the cwd under each other:
///
/// * turns whose sessions share a cwd hold the lease **together** (it is
///   reference-counted) and run fully concurrently;
/// * a turn in a *different* cwd waits until the current holders are gone;
/// * a turn that parks on user input (permission prompt / `AskUserQuestion`)
///   **releases** the lease while it waits ([`CwdLeaseHandle::parked`]) and
///   re-acquires it before continuing, so a parked session never keeps
///   another cwd's session from running — the P0 this exists to fix.
///
/// The lease only ever *sets* the cwd on acquisition; when the last holder
/// leaves, the cwd is left as is (nothing outside a lease may depend on it).
#[derive(Default)]
pub struct WorkspaceCwdLease {
    state: Mutex<CwdLeaseState>,
    released: std::sync::Condvar,
}

#[derive(Default)]
struct CwdLeaseState {
    holder: Option<PathBuf>,
    holders: usize,
}

impl WorkspaceCwdLease {
    /// Block until the process cwd can be `cwd`, set it, and return a guard
    /// that gives the lease back on drop.
    ///
    /// # Errors
    ///
    /// Returns the `set_current_dir` error if `cwd` cannot be entered; the
    /// lease is left untouched in that case.
    pub fn acquire(self: &Arc<Self>, cwd: PathBuf) -> std::io::Result<CwdLeaseGuard> {
        self.enter(&cwd)?;
        Ok(CwdLeaseGuard {
            handle: CwdLeaseHandle(Arc::new(CwdLeaseHandleInner {
                lease: Arc::clone(self),
                cwd,
                held: std::sync::atomic::AtomicBool::new(true),
            })),
        })
    }

    fn enter(&self, cwd: &std::path::Path) -> std::io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.holder.as_deref().is_some_and(|held| held != cwd) {
            state = self
                .released
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.holder.is_none() {
            std::env::set_current_dir(cwd)?;
            state.holder = Some(cwd.to_path_buf());
        }
        state.holders += 1;
        Ok(())
    }

    fn leave(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.holders = state.holders.saturating_sub(1);
        if state.holders == 0 {
            state.holder = None;
            self.released.notify_all();
        }
    }
}

struct CwdLeaseHandleInner {
    lease: Arc<WorkspaceCwdLease>,
    cwd: PathBuf,
    /// Whether this handle currently counts as a holder. Only the turn's
    /// own thread flips it (acquire → park → unpark → drop), the atomic is
    /// for `Sync`, not for contention.
    held: std::sync::atomic::AtomicBool,
}

/// Cheap, cloneable reference to a held lease that lets the code parked
/// deep inside a turn temporarily give the lease back.
#[derive(Clone)]
pub struct CwdLeaseHandle(Arc<CwdLeaseHandleInner>);

impl CwdLeaseHandle {
    /// Run `wait` with the lease released, then re-acquire the lease before
    /// returning. If this handle is not currently holding the lease the
    /// closure simply runs (a stale handle can never re-acquire on its own).
    /// If the cwd cannot be re-entered afterwards, the failure is reported on
    /// stderr and the turn continues without the lease — the directory has
    /// vanished underneath the session, and its tools will report that
    /// themselves.
    pub fn parked<R>(&self, wait: impl FnOnce() -> R) -> R {
        use std::sync::atomic::Ordering;
        let released = self.0.held.swap(false, Ordering::AcqRel);
        if released {
            self.0.lease.leave();
        }
        let result = wait();
        if released {
            match self.0.lease.enter(&self.0.cwd) {
                Ok(()) => self.0.held.store(true, Ordering::Release),
                Err(error) => eprintln!(
                    "[acp] failed to re-enter session cwd {} after user input: {error}",
                    self.0.cwd.display()
                ),
            }
        }
        result
    }
}

/// RAII holder of a [`WorkspaceCwdLease`] acquisition.
pub struct CwdLeaseGuard {
    handle: CwdLeaseHandle,
}

impl CwdLeaseGuard {
    /// Handle to hand to code that may need to park while this guard lives.
    #[must_use]
    pub fn handle(&self) -> CwdLeaseHandle {
        self.handle.clone()
    }
}

impl Drop for CwdLeaseGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if self.handle.0.held.swap(false, Ordering::AcqRel) {
            self.handle.0.lease.leave();
        }
    }
}

/// A permission prompter that bridges to the ACP client over channels.
///
/// From inside the blocking `spawn_blocking` context, `decide()` sends
/// the permission request to an async handler which forwards it to the
/// ACP client, then blocks waiting for the response.
struct AcpPermissionBridge {
    tx: tokio::sync::mpsc::UnboundedSender<(
        PermissionRequest,
        tokio::sync::oneshot::Sender<PermissionPromptDecision>,
    )>,
    /// Lease on the process cwd held by the turn this bridge serves; given
    /// back while we wait on the user so other sessions can run.
    cwd_lease: Option<CwdLeaseHandle>,
}

/// Block on `wait` with the turn's cwd lease (if any) parked for the
/// duration: the turn is idle until the user answers, and nothing in it
/// touches the process cwd meanwhile (tool calls run strictly one at a time),
/// so holding the lease would only keep sessions in other directories from
/// making progress.
fn wait_parked<R>(cwd_lease: Option<&CwdLeaseHandle>, wait: impl FnOnce() -> R) -> R {
    match cwd_lease {
        Some(lease) => lease.parked(wait),
        None => wait(),
    }
}

impl PermissionPrompter for AcpPermissionBridge {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if self.tx.send((request.clone(), response_tx)).is_err() {
            return PermissionPromptDecision::Deny {
                reason: "permission bridge closed".to_string(),
            };
        }
        // `decide()` is reached from inside the conversation runtime's
        // `tokio_runtime.block_on(run_turn)`, so this thread is driving
        // asynchronous tasks. Plain `blocking_recv()` there triggers tokio's
        // "Cannot block the current thread from within a runtime" panic,
        // which aborts the prompt task and surfaces to the client as a
        // generic "blocking task failed" Internal error. `block_in_place`
        // tells the multi-thread scheduler this thread is about to block,
        // allowing the recv to complete safely (same pattern as
        // `AcpQuestionBridge::ask` below).
        wait_parked(self.cwd_lease.as_ref(), || {
            tokio::task::block_in_place(|| {
                response_rx
                    .blocking_recv()
                    .unwrap_or(PermissionPromptDecision::Deny {
                        reason: "permission response channel closed".to_string(),
                    })
            })
        })
    }
}

impl QuestionPrompter for AcpQuestionBridge {
    fn ask(
        &mut self,
        request: &QuestionPromptRequest,
    ) -> Result<Vec<QuestionPromptAnswer>, String> {
        let tool_call_id = format!("ask-{}", uuid_v4());
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send((tool_call_id, request.clone(), response_tx))
            .is_err()
        {
            return Err("question bridge closed".to_string());
        }
        // The LLM tool loop runs synchronously inside the conversation runtime's
        // `tokio_runtime.block_on(run_turn)` (multi-thread runtime), so this
        // `ask()` is reached from a tokio worker thread. Plain `blocking_recv()`
        // there triggers tokio's "Cannot block the current thread from within a
        // runtime" panic, which aborts the entire prompt task and surfaces to
        // the client as a generic "blocking task failed" / Internal error.
        // `block_in_place` informs the multi-thread scheduler that this worker
        // is about to block, allowing the recv to complete safely.
        wait_parked(self.cwd_lease.as_ref(), || {
            tokio::task::block_in_place(|| {
                response_rx
                    .blocking_recv()
                    .unwrap_or_else(|_| Err("question response channel closed".to_string()))
            })
        })
    }
}

struct AcpQuestionBridge {
    tx: tokio::sync::mpsc::UnboundedSender<(
        String,
        QuestionPromptRequest,
        tokio::sync::oneshot::Sender<Result<Vec<QuestionPromptAnswer>, String>>,
    )>,
    /// See [`AcpPermissionBridge::cwd_lease`].
    cwd_lease: Option<CwdLeaseHandle>,
}

const ACP_ASK_USER_QUESTION_METHOD: &str = "_scode/ask_user_question";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpQuestionOptionPayload {
    label: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    recommended: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpQuestionFieldPayload {
    id: String,
    prompt: String,
    kind: String,
    required: bool,
    allow_custom_input: bool,
    custom_input_hint: Option<String>,
    options: Vec<AcpQuestionOptionPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpAskUserQuestionRequestPayload {
    session_id: String,
    tool_call_id: String,
    title: Option<String>,
    description: Option<String>,
    questions: Vec<AcpQuestionFieldPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpAskUserQuestionAnswerPayload {
    id: String,
    value: String,
    label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AcpAskUserQuestionResponsePayload {
    answers: Vec<AcpAskUserQuestionAnswerPayload>,
}

/// Build an ACP `RequestPermissionRequest` from a runtime `PermissionRequest`.
fn build_acp_permission_request(
    session_id: String,
    request: &PermissionRequest,
) -> RequestPermissionRequest {
    let tool_call = ToolCallUpdate::new(
        format!("perm-{}", uuid_v4()),
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .raw_input(serde_json::Value::String(request.input.clone())),
    );

    let options = vec![
        PermissionOption::new(
            PermissionOptionId::new("allow_once"),
            "Allow Once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new("allow_always"),
            "Allow Always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::new("reject_once"),
            "Reject Once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new("reject_always"),
            "Reject Always",
            PermissionOptionKind::RejectAlways,
        ),
    ];

    RequestPermissionRequest::new(session_id, tool_call, options)
}

/// Map an ACP permission response to a `PermissionPromptDecision`.
fn map_permission_response(response: RequestPermissionResponse) -> PermissionPromptDecision {
    match response.outcome {
        RequestPermissionOutcome::Selected(selected) => {
            let id_str: &str = &selected.option_id.0;
            if id_str.starts_with("allow") {
                PermissionPromptDecision::Allow
            } else {
                PermissionPromptDecision::Deny {
                    reason: format!("user selected: {id_str}"),
                }
            }
        }
        RequestPermissionOutcome::Cancelled | _ => PermissionPromptDecision::Deny {
            reason: "user cancelled permission prompt".to_string(),
        },
    }
}

/// Generate a pseudo-random UUID v4 string without pulling in the `uuid` crate.
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:032x}")
}

// ---------------------------------------------------------------------------
// Shared handler chain
// ---------------------------------------------------------------------------

/// Run the ACP agent handler chain on an arbitrary transport.
///
/// This is the shared core used by both the stdio server and the WebSocket
/// server. The transport must implement `ConnectTo<Agent>` (e.g. `Stdio` or
/// `Lines`).
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_acp_on_transport(
    config: &SdkAcpConfig,
    delegate: SharedDelegate,
    registry: SharedSessionRegistry,
    transport: impl ConnectTo<Agent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent_version = config.agent_version.clone();

    Agent
        .builder()
        .name("scode")
        // --- initialize ---
        .on_receive_request(
            {
                let version = agent_version.clone();
                async move |req: InitializeRequest,
                            responder: Responder<InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    let resp = InitializeResponse::new(req.protocol_version)
                        .agent_info(Implementation::new("scode", &version))
                        .agent_capabilities(
                            AgentCapabilities::new()
                                // `session/load` re-opens a persisted session
                                // (same cwd) in a fresh process; the handler
                                // below has always existed, the flag had just
                                // never been advertised, so spec-conformant
                                // clients never tried it.
                                .load_session(true)
                                .prompt_capabilities(PromptCapabilities::new().image(true))
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .close(SessionCloseCapabilities::new()),
                                ),
                        )
                        .meta(initialize_meta());
                    responder.respond(resp)?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/new ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let registry = Arc::clone(&registry);
                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&registry);
                    cx.spawn(async move {
                        let lease = registry.cwd_lease();
                        let result = tokio::task::spawn_blocking(move || {
                            // Session construction resolves config / model /
                            // permission mode against the process cwd, so it
                            // runs under the cwd lease like a turn does.
                            let _cwd = lease
                                .acquire(session_lease_cwd(&req.cwd))
                                .map_err(|e| AcpError::internal(format!("failed to enter cwd: {e}")))?;
                            let mcp_servers = acp_mcp_servers_to_scoped(&req.mcp_servers, &req.cwd);
                            d.new_session(req.cwd, mcp_servers)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok((session_id, cwd, signal)) => {
                                registry.register(session_id.clone(), signal, cwd);
                                responder.respond(NewSessionResponse::new(session_id))?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/prompt (with permission-prompt bridging) ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let registry = Arc::clone(&registry);
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    let (prompt_text, images) = match extract_content_from_blocks(&req.prompt) {
                        Ok(r) => r,
                        Err(e) => {
                            responder.respond_with_error(acp_error_to_sdk(&e))?;
                            return Ok(());
                        }
                    };
                    // Text is required (images alone aren't enough to drive a turn).
                    if prompt_text.is_empty() {
                        responder.respond_with_error(acp_error_to_sdk(
                            &AcpError::invalid_params(
                                "prompt must include at least one non-empty text content block",
                            ),
                        ))?;
                        return Ok(());
                    }

                    // Extract traceId from _meta if present
                    let trace_id = req.meta.as_ref().and_then(|m| {
                        m.get("traceId").and_then(|v| v.as_str().map(String::from))
                    });

                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&registry);
                    let sid = req.session_id.to_string();
                    let cx_inner = cx.clone();
                    let cx_perm = cx.clone();
                    cx.spawn(async move {
                        // Same-session ordering: wait (asynchronously — no
                        // thread is tied up) for this session's lane. Other
                        // sessions have their own lanes and are unaffected.
                        let _lane = registry.enter_lane(&sid).await;
                        let session_cwd = registry.cwd(&sid);
                        let cwd_lease = registry.cwd_lease();

                        // Set up permission-prompt bridge channels.
                        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel::<(
                            PermissionRequest,
                            tokio::sync::oneshot::Sender<PermissionPromptDecision>,
                        )>();
                        let (question_tx, mut question_rx) = tokio::sync::mpsc::unbounded_channel::<(
                            String,
                            QuestionPromptRequest,
                            tokio::sync::oneshot::Sender<Result<Vec<QuestionPromptAnswer>, String>>,
                        )>();

                        // Set up notification streaming channel.
                        let (notif_tx, mut notif_rx) =
                            tokio::sync::mpsc::unbounded_channel::<SessionNotification>();

                        let sid_for_blocking = sid.clone();
                        let sid_for_perm = sid.clone();
                        let images_for_blocking = images.clone();
                        let prompt_text_for_blocking = prompt_text.clone();
                        let trace_id_for_blocking = trace_id.clone();
                        let blocking_handle = tokio::task::spawn_blocking(move || {
                            // Hold the process cwd for this session's directory
                            // for the whole turn (shared with concurrent turns
                            // in the same directory; parked while waiting on
                            // the user — see `WorkspaceCwdLease`). Unknown
                            // sessions have no cwd and fall through to the
                            // delegate's `unknown sessionId` error.
                            let cwd_guard = match session_cwd {
                                Some(cwd) => Some(cwd_lease.acquire(cwd).map_err(|e| {
                                    AcpError::internal(format!("failed to enter session cwd: {e}"))
                                })?),
                                None => None,
                            };
                            let lease_handle = cwd_guard.as_ref().map(CwdLeaseGuard::handle);

                            let mut observer = SdkSessionObserver::new(&sid_for_blocking, notif_tx);
                            let mut bridge = AcpPermissionBridge {
                                tx: bridge_tx,
                                cwd_lease: lease_handle.clone(),
                            };
                            let question_bridge = AcpQuestionBridge {
                                tx: question_tx,
                                cwd_lease: lease_handle,
                            };
                            let _ = d.set_question_prompter(
                                &sid_for_blocking,
                                Box::new(question_bridge),
                            );

                            // Push image content blocks into the session before
                            // running the prompt so the API client includes them.
                            if !images_for_blocking.is_empty() {
                                let _ = d.push_images(&sid_for_blocking, &images_for_blocking);
                            }

                            let stop = if prompt_text_for_blocking.starts_with('/') {
                                d.handle_slash_command(
                                    &sid_for_blocking,
                                    &prompt_text_for_blocking,
                                    &mut observer,
                                )
                                .map(|()| (StopReason::EndTurn, None))
                            } else {
                                d.run_prompt_with_prompter(
                                    &sid_for_blocking,
                                    prompt_text_for_blocking,
                                    &mut observer,
                                    &mut bridge,
                                    trace_id_for_blocking.as_deref(),
                                )
                            };
                            drop(cwd_guard);
                            // Return the Result instead of unwrapping, so we can handle errors
                            stop
                        });

                        // Concurrently serve permission requests and stream
                        // notifications from the blocking thread while waiting
                        // for it to finish.
                        let mut blocking_handle = blocking_handle;
                        let mut notif_rx_open = true;
                        let result: Result<(StopReason, Option<PromptUsage>), AcpError> = loop {
                            tokio::select! {
                                biased;
                                notif = notif_rx.recv(), if notif_rx_open => {
                                    if let Some(n) = notif {
                                        let _ = cx_inner.send_notification(n);
                                    } else {
                                        // Sender dropped — stop polling this channel.
                                        notif_rx_open = false;
                                    }
                                }
                                perm = bridge_rx.recv() => {
                                    if let Some((perm_req, response_tx)) = perm {
                                        let acp_req = build_acp_permission_request(
                                            sid_for_perm.clone(),
                                            &perm_req,
                                        );
                                        let decision = match cx_perm
                                            .send_request(acp_req)
                                            .block_task()
                                            .await
                                        {
                                            Ok(resp) => map_permission_response(resp),
                                            Err(_) => PermissionPromptDecision::Deny {
                                                reason: "ACP permission request failed"
                                                    .to_string(),
                                            },
                                        };
                                        let _ = response_tx.send(decision);
                                    } else {
                                        // Channel closed — blocking task dropped the sender.
                                        // Await the result directly to avoid a busy loop
                                        // (biased select would keep picking this branch).
                                        break blocking_handle.await
                                            .unwrap_or(Err(AcpError::internal("blocking task failed")));
                                    }
                                }
                                question = question_rx.recv() => {
                                    if let Some((tool_call_id, question_req, response_tx)) = question {
                                        let payload = AcpAskUserQuestionRequestPayload {
                                            session_id: sid_for_perm.clone(),
                                            tool_call_id,
                                            title: question_req.title.clone(),
                                            description: question_req.description.clone(),
                                            questions: question_req
                                                .fields
                                                .iter()
                                                .map(|field| AcpQuestionFieldPayload {
                                                    id: field.id.clone(),
                                                    prompt: field.prompt.clone(),
                                                    kind: field.kind.as_str().to_string(),
                                                    required: field.required,
                                                    allow_custom_input: field.allow_custom_input,
                                                    custom_input_hint: field.custom_input_hint.clone(),
                                                    options: field
                                                        .options
                                                        .iter()
                                                        .map(|option| AcpQuestionOptionPayload {
                                                            label: option.label.clone(),
                                                            value: option.value.clone(),
                                                            description: option.description.clone(),
                                                            recommended: option.recommended,
                                                        })
                                                        .collect(),
                                                })
                                                .collect(),
                                        };

                                        let outcome = match serde_json::value::to_raw_value(&payload) {
                                            Ok(raw) => {
                                                match cx_perm
                                                    .send_request(ClientRequest::ExtMethodRequest(
                                                        ExtRequest::new(ACP_ASK_USER_QUESTION_METHOD, StdArc::from(raw)),
                                                    ))
                                                    .block_task()
                                                    .await
                                                {
                                                    Ok(resp) => {
                                                        serde_json::from_value::<AcpAskUserQuestionResponsePayload>(resp)
                                                            .map_err(|error| format!("deserialize: {}", error))
                                                            .map(|payload| {
                                                                payload
                                                                    .answers
                                                                    .into_iter()
                                                                    .map(|answer| QuestionPromptAnswer {
                                                                        id: answer.id,
                                                                        value: answer.value,
                                                                        label: answer.label,
                                                                    })
                                                                    .collect::<Vec<_>>()
                                                            })
                                                    }
                                                    Err(error) => Err(error.to_string()),
                                                }
                                            }
                                            Err(error) => Err(error.to_string()),
                                        };
                                        let _ = response_tx.send(outcome);
                                    } else {
                                        break blocking_handle.await
                                            .unwrap_or(Err(AcpError::internal("blocking task failed")));
                                    }
                                }
                                done = &mut blocking_handle => {
                                    break done.unwrap_or(Err(AcpError::internal("blocking task join failed")));
                                }
                            }
                        };

                        // Drain any residual notifications that were buffered
                        // before the blocking task returned.
                        while let Ok(n) = notif_rx.try_recv() {
                            let _ = cx_inner.send_notification(n);
                        }

                        // Handle errors by sending an error message notification to the client
                        match result {
                            Ok((stop_reason, prompt_usage)) => {
                                let mut response = PromptResponse::new(stop_reason);
                                if let Some(u) = prompt_usage {
                                    let sudocode_meta = sudocode_meta_from_prompt_usage(&u);
                                    let mut meta = Map::new();
                                    meta.insert("sudocode".to_string(), json!(sudocode_meta));
                                    response = response.usage(
                                        Usage::new(u.total_tokens, u.input_tokens, u.output_tokens)
                                            .cached_read_tokens(u.cache_read_tokens)
                                            .cached_write_tokens(u.cache_write_tokens),
                                    ).meta(Some(meta));
                                }
                                responder.respond(response)?;
                            }
                            Err(error) => {
                                // Send user-friendly error message as a notification to the client
                                let user_message = error.user_friendly_message();
                                let error_notification = SessionNotification::new(
                                    sid.clone(),
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(&user_message)),
                                    )),
                                );
                                let _ = cx_inner.send_notification(error_notification);

                                // Respond with an error
                                responder.respond_with_error(acp_error_to_sdk(&error))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/cancel (notification) ---
        .on_receive_notification(
            {
                let registry = Arc::clone(&registry);
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    // Deliberately lock-free w.r.t. sessions: cancel must reach
                    // a session whose lane is busy running the very turn being
                    // cancelled.
                    if let Some(signal) = registry.abort_signal(&notif.session_id.to_string()) {
                        signal.abort();
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        // --- session/close ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let registry = Arc::clone(&registry);
                async move |req: CloseSessionRequest,
                            responder: Responder<CloseSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&registry);
                    let sid = req.session_id.to_string();
                    cx.spawn(async move {
                        // Queue behind any in-flight turn on this session (as
                        // before: close waits for the turn to finish).
                        let _lane = registry.enter_lane(&sid).await;
                        let sid_clone = sid.clone();
                        tokio::task::spawn_blocking(move || {
                            d.close_session(&sid_clone);
                        })
                        .await
                        .ok();
                        registry.remove(&sid);
                        responder.respond(CloseSessionResponse::new())?;
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/list ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                async move |_req: ListSessionsRequest,
                            responder: Responder<ListSessionsResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    cx.spawn(async move {
                        let infos = tokio::task::spawn_blocking(move || {
                            d.list_sessions()
                                .into_iter()
                                .map(|(id, cwd)| SessionInfo::new(id, cwd))
                                .collect::<Vec<_>>()
                        })
                        .await
                        .unwrap_or_default();

                        responder.respond(ListSessionsResponse::new(infos))?;
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/setModel (unstable) ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let registry = Arc::clone(&registry);
                async move |req: SetSessionModelRequest,
                            responder: Responder<SetSessionModelResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&registry);
                    let sid = req.session_id.to_string();
                    let model_id: String = req.model_id.0.to_string();
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&sid).await;
                        let session_cwd = registry.cwd(&sid);
                        let lease = registry.cwd_lease();
                        let result = tokio::task::spawn_blocking(move || {
                            // The model switch rebuilds the session runtime,
                            // resolving config against the process cwd.
                            let _cwd = match session_cwd {
                                Some(cwd) => Some(lease.acquire(cwd).map_err(|e| {
                                    AcpError::internal(format!("failed to enter session cwd: {e}"))
                                })?),
                                None => None,
                            };
                            d.set_model(&sid, &model_id)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok(_report) => {
                                responder.respond(SetSessionModelResponse::new())?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/load ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let registry = Arc::clone(&registry);
                async move |req: LoadSessionRequest,
                            responder: Responder<LoadSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&registry);
                    let sid = req.session_id.to_string();
                    let cwd = req.cwd;
                    cx.spawn(async move {
                        // If the session is already live in this process, a
                        // reload must not race a turn on it.
                        let _lane = registry.enter_lane(&sid).await;
                        let lease = registry.cwd_lease();
                        let result = tokio::task::spawn_blocking(move || {
                            let _cwd = lease
                                .acquire(session_lease_cwd(&cwd))
                                .map_err(|e| AcpError::internal(format!("failed to enter cwd: {e}")))?;
                            let mcp_servers = acp_mcp_servers_to_scoped(&req.mcp_servers, &cwd);
                            d.load_session(&sid, cwd, mcp_servers)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok((session_id, cwd, signal)) => {
                                registry.register(session_id, signal, cwd);
                                responder.respond(LoadSessionResponse::new())?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/setPermissionMode (custom extension, not in SDK schema) ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let registry = Arc::clone(&registry);
                async move |req: SetPermissionModeRequest,
                            responder: Responder<SetPermissionModeResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&registry);
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&req.session_id).await;
                        let result = tokio::task::spawn_blocking(move || {
                            let mode = match req.permission_mode.as_str() {
                                "read-only" => Ok(PermissionMode::ReadOnly),
                                "workspace-write" => Ok(PermissionMode::WorkspaceWrite),
                                "danger-full-access" => Ok(PermissionMode::DangerFullAccess),
                                "prompt" => Ok(PermissionMode::Prompt),
                                "allow" => Ok(PermissionMode::Allow),
                                other => Err(AcpError::invalid_params(format!(
                                    "unknown permission mode: {other}"
                                ))),
                            };
                            match mode {
                                Ok(m) => d.set_permission_mode(&req.session_id, m),
                                Err(e) => Err(e),
                            }
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok(()) => {
                                responder.respond(SetPermissionModeResponse {})?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- catch-all for unhandled methods ---
        // Only respond with method_not_found for Request/Notification.
        // Response MUST be passed through (Handled::No) so the SDK's ResponseRouter
        // can deliver the result to the waiting oneshot channel.
        .on_receive_dispatch(
            async move |dispatch: Dispatch, cx: ConnectionTo<Client>| {
                match &dispatch {
                    Dispatch::Request(_, _) | Dispatch::Notification(_) => {
                        dispatch.respond_with_error(Error::method_not_found(), cx)?;
                        Ok(Handled::Yes)
                    }
                    Dispatch::Response(_, _) => {
                        // Pass through to SDK's default ResponseRouter
                        Ok(Handled::No {
                            message: dispatch,
                            retry: false,
                        })
                    }
                }
            },
            on_receive_dispatch!(),
        )
        .connect_to(transport)
        .await?;

    Ok(())
}

/// Key the cwd lease by the canonical directory so that two sessions
/// naming the same directory differently (`./x` vs `/abs/x`, symlinks) share
/// one lease instead of serializing against each other. Falls back to the
/// path as given when it cannot be canonicalized (the delegate then rejects
/// the request with a proper `params.cwd` error).
fn session_lease_cwd(cwd: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Map our `AcpError` to the SDK's `Error` type.
pub(crate) fn acp_error_to_sdk(e: &AcpError) -> Error {
    match e {
        AcpError::InvalidParams(msg) => {
            Error::invalid_params().data(serde_json::Value::String(msg.clone()))
        }
        AcpError::Internal(msg) => {
            Error::internal_error().data(serde_json::Value::String(msg.clone()))
        }
    }
}
