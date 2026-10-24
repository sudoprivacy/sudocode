//! ACP server implementation using the official `agent-client-protocol` SDK.
//!
//! This module provides an SDK-based ACP server with full ACP 1.0 compliance
//! including capabilities declaration, session cancel, permission-mode switching,
//! model switching, image input, and permission-prompt bridging (elicitation).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc as StdArc;
use std::sync::{Arc, Mutex};

use commands::AcpSlashCommandSpec;
use futures::{FutureExt, StreamExt};

use agent_client_protocol::role::acp::{Agent, Client};
// NOTE: `ConnectTo` and `ConnectionTo` are different SDK concepts:
//   - `ConnectTo<R>`:    trait for wiring up a transport (Stdio, Lines, etc.)
//   - `ConnectionTo<R>`: runtime handle passed to handlers for sending messages
use agent_client_protocol::{
    on_receive_dispatch, on_receive_notification, on_receive_request, ConnectTo, ConnectionTo,
    Dispatch, Error, Handled, JsonRpcRequest, JsonRpcResponse, Responder,
};
use agent_client_protocol_schema::{
    AgentCapabilities, AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate,
    CancelNotification, ClientRequest, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, ExtRequest, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, McpServer, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionInfo, SessionListCapabilities,
    SessionNotification, SessionUpdate, SetSessionModelRequest, SetSessionModelResponse,
    StopReason, TextContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    UnstructuredCommandInput, Usage,
};
use engine_core::engine_events::CompletionAction;
use engine_core::{AuthMode, DirectToolError, EngineDelegate, EngineEvent};
use engine_host::config::AllowedToolSet;
use engine_host::{SessionEngine, SessionLifecycle};
use runtime::config::{
    ConfigSource, McpRemoteServerConfig, McpServerConfig, McpStdioServerConfig,
    ScopedMcpServerConfig,
};
use runtime::workspace_root::WorkspaceRootScope;
use runtime::HookAbortSignal;
use runtime::SystemPromptOverrides;
use runtime::UsageCostCurrency;
use runtime::{PermissionMode, PermissionPromptDecision, PermissionRequest, QuestionPromptAnswer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::session_ops;
use crate::subagent_completions::{
    self, Delivery, ReplayRequest, ReplayResponse, TaskOperation, TaskPurgeRequest,
    TaskReplayRequest, TaskResponse, TaskSuppressRequest,
};

/// Error type returned by ACP agent implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpError {
    InvalidParams(String),
    Internal(String),
    /// A prompt-turn failure that arrived already classified by the runtime
    /// (via [`runtime::RuntimeError`]'s typed bits). Carrying the classification
    /// here — instead of flattening to a bare `Internal(String)` and later
    /// re-guessing it by string-matching — is what lets the ACP surface pick
    /// the right user-facing message from the who-can-act taxonomy.
    Turn {
        message: String,
        /// The request exceeded the model's context window (taxonomy: Model,
        /// then Human once compaction is exhausted). Terminal for the turn.
        context_window: bool,
    },
}

/// What a client shows when context maintenance ends the run.
const COMPACTION_FAILED_MESSAGE: &str = "上下文压缩失败，本次对话已停止。已有对话历史已保留。";

/// Friendly guidance shown when a request exceeds the model's context window.
/// Single source so the typed (turn) path and the legacy string-sniff fallback
/// cannot drift apart.
const CONTEXT_WINDOW_EXCEEDED_MESSAGE: &str = "[context_window_exceeded] 请求超出了模型的上下文限制。\n\n建议解决方案：\n1. 压缩或清除对话历史后重新开始\n2. 使用较小的图片或简化输入内容\n3. 使用支持更大上下文的模型";

impl AcpError {
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::InvalidParams(message.into())
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// Build a turn error from a runtime error, capturing its typed
    /// classification so the ACP surface never has to re-derive behavior by
    /// string-matching the message.
    #[must_use]
    pub fn from_turn_error(error: &runtime::RuntimeError) -> Self {
        Self::Turn {
            message: error.to_string(),
            context_window: error.is_context_window_blocked(),
        }
    }

    /// Generate a user-friendly error message with actionable suggestions.
    #[must_use]
    pub fn user_friendly_message(&self) -> String {
        // A turn error carries its classification typed — decide from the
        // taxonomy bucket, not by string-matching the rendered message.
        let raw_message = match self {
            Self::Turn {
                message,
                context_window,
            } => {
                if *context_window {
                    return CONTEXT_WINDOW_EXCEEDED_MESSAGE.to_string();
                }
                message
            }
            Self::InvalidParams(msg) | Self::Internal(msg) => msg,
        };

        if raw_message.contains(runtime::COMPACTION_FAILED) {
            return COMPACTION_FAILED_MESSAGE.to_string();
        }

        if raw_message.contains("[context_window_exceeded]") {
            return raw_message.clone();
        }

        // Check for specific error types and provide friendly messages.
        // A context-window rejection that reaches here was not classified by
        // the prompt path (which knows whether history was compactable), so
        // do not guess a sub-class: a long history and a single oversized
        // message are different problems with different fixes.
        if raw_message.contains("context_window_blocked")
            || raw_message.contains("Context window blocked")
        {
            return CONTEXT_WINDOW_EXCEEDED_MESSAGE.to_string();
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
            Self::InvalidParams(message) | Self::Internal(message) | Self::Turn { message, .. } => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for AcpError {}

/// Configuration for the SDK-based ACP server — everything needed to build a
/// [`SessionEngine`] per ACP session, plus the CLI build metadata `/doctor`
/// surfaces. Handed in by the composition root (`scode acp`), which owns the
/// resolved model / allowed-tools / auth / build constants.
#[derive(Debug, Clone)]
pub struct SdkAcpConfig {
    pub agent_version: String,
    pub model: String,
    pub model_flag_raw: Option<String>,
    pub permission_mode_override: Option<PermissionMode>,
    pub reasoning_effort: Option<String>,
    /// `--allowed-tools` restriction, threaded into every session's engine.
    pub allowed_tools: Option<AllowedToolSet>,
    /// `--auth` override (`None` = auto-resolve from model + config).
    pub auth_mode: Option<AuthMode>,
    /// `GIT_SHA` build constant, for the `/doctor` System check.
    pub git_sha: Option<String>,
    /// `TARGET` build constant, for the `/doctor` System check.
    pub build_target: Option<String>,
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

// ---------------------------------------------------------------------------
// Custom extension: _sudocode/agent/cancel (sub-agent events contract)
// ---------------------------------------------------------------------------

/// Stop one running sub-agent of a session that opted into sub-agent events.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/agent/cancel", response = CancelSubagentResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelSubagentRequest {
    pub session_id: String,
    pub agent_id: String,
}

/// `cancelled: false` means the agent had already finished.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
pub(crate) struct CancelSubagentResponse {
    pub cancelled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/session/pause_tools", response = PauseToolsResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PauseToolsRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PauseToolsResponse {
    pub pause_epoch: u64,
    pub is_quiescent: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/session/resume_tools", response = ResumeToolsResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResumeToolsRequest {
    pub session_id: String,
    pub pause_epoch: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
pub(crate) struct ResumeToolsResponse {}

// ---------------------------------------------------------------------------
// Custom extension: _sudocode/a2a/* (direct A2A requests)
// ---------------------------------------------------------------------------

// Served from the catch-all dispatch rather than as typed handlers: every typed
// handler nests the server's type one level deeper, and the chain is already at
// the depth the compiler lays out by default in every crate that runs it.
const DIRECT_SEND_METHOD: &str = "_sudocode/a2a/send";
const DIRECT_AGENT_LIST_METHOD: &str = "_sudocode/a2a/agent_list";

/// Run the session's own `send` tool for the client, without a model turn.
/// Plain text only, and no `sender`: the session's agent is the sender. The
/// `send` tool requires a summary with a text message, so a request without
/// one is refused here rather than admitted to fail.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectSendRequest {
    session_id: String,
    to: String,
    message: String,
    summary: String,
    /// ACP lets any request carry `_meta`; it is accepted and ignored.
    #[serde(rename = "_meta", default)]
    _meta: Option<serde::de::IgnoredAny>,
}

/// Run the session's own `agent_list` tool for the client.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectAgentListRequest {
    session_id: String,
    #[serde(default)]
    active_only: Option<bool>,
    #[serde(rename = "_meta", default)]
    _meta: Option<serde::de::IgnoredAny>,
}

/// The tool's own result, verbatim. `isError` reports a failed call; it does
/// not prove that nothing was sent.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectA2aResponse {
    content: String,
    is_error: bool,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    meta: Option<serde_json::Value>,
}

/// `_meta.sudocode` key a client sets in `clientCapabilities` (and scode
/// echoes in the `initialize` result) to opt into sub-agent events.
pub const SUBAGENT_EVENTS_META_KEY: &str = "subagentEvents";
/// The contract version this build speaks.
const SUBAGENT_EVENTS_VERSION: u64 = 1;

/// The sub-agent events version a client asked for in
/// `clientCapabilities._meta.sudocode.subagentEvents`, capped at ours.
/// Anything malformed is "not asked": an extension field never fails
/// `initialize`.
fn subagent_events_version(meta: Option<&agent_client_protocol_schema::Meta>) -> Option<u64> {
    let version = meta?
        .get("sudocode")?
        .get(SUBAGENT_EVENTS_META_KEY)?
        .get("version")?
        .as_u64()?;
    (version >= 1).then_some(version.min(SUBAGENT_EVENTS_VERSION))
}

/// ACP presentation subscriptions and durable wire delivery. All lifecycle
/// observations originate from the session's shared engine event stream.
pub struct SessionSubagents {
    completion: Option<Arc<Delivery>>,
}

impl SessionSubagents {
    /// Must run inside the ACP server's Tokio runtime (it spawns the task that
    /// writes background events to the connection).
    fn start(
        _session_id: String,
        connection: ConnectionTo<Client>,
        completion: Option<Arc<Delivery>>,
    ) -> Arc<Self> {
        if let Some(delivery) = &completion {
            delivery.bind(connection);
        }
        Arc::new(Self { completion })
    }

    pub(crate) fn completion_delivery(&self) -> Option<Arc<Delivery>> {
        self.completion.clone()
    }

    fn set_connection(&self, connection: ConnectionTo<Client>) {
        if let Some(delivery) = &self.completion {
            delivery.bind(connection);
        }
    }
}

impl Drop for SessionSubagents {
    fn drop(&mut self) {
        if let Some(delivery) = &self.completion {
            delivery.unbind();
        }
    }
}

/// Convert the ACP `mcp_servers` carried by `session/new` / `session/load`
/// into scode-internal scoped MCP configs, keyed by server name. The session
/// `cwd` becomes each stdio server's `current_dir` so relative `command`
/// paths resolve against the session working directory.
///
/// Remote servers use the same runtime transports as configured CLI servers.
/// This lets a managed session connect without spawning a host subprocess or
/// requiring its virtual workspace to exist in the host filesystem.
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
            McpServer::Http(http) => {
                out.insert(
                    http.name.clone(),
                    ScopedMcpServerConfig {
                        scope: ConfigSource::Local,
                        config: McpServerConfig::Http(McpRemoteServerConfig {
                            url: http.url.clone(),
                            headers: http
                                .headers
                                .iter()
                                .map(|header| (header.name.clone(), header.value.clone()))
                                .collect(),
                            headers_helper: None,
                            oauth: None,
                        }),
                    },
                );
            }
            McpServer::Sse(sse) => {
                out.insert(
                    sse.name.clone(),
                    ScopedMcpServerConfig {
                        scope: ConfigSource::Local,
                        config: McpServerConfig::Sse(McpRemoteServerConfig {
                            url: sse.url.clone(),
                            headers: sse
                                .headers
                                .iter()
                                .map(|header| (header.name.clone(), header.value.clone()))
                                .collect(),
                            headers_helper: None,
                            oauth: None,
                        }),
                    },
                );
            }
            _ => {}
        }
    }
    out
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
    pub token_breakdown_complete: Option<bool>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub context_window_tokens: Option<u64>,
    pub estimated_session_tokens: Option<u64>,
    pub cost_units: Option<u64>,
    pub cost_currency: Option<UsageCostCurrency>,
    /// Cumulative usage for the entire session, exposed via _meta.sudocode.cumulativeUsage
    pub cumulative_usage: Option<CumulativeUsage>,
    /// `true` when the transcript was compacted automatically during this
    /// turn (pre-turn overflow protection or the in-turn threshold path),
    /// exposed via `_meta.sudocode.autoCompacted`.
    pub auto_compacted: bool,
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

/// Translate a slash-command spec into the ACP `AvailableCommand` advertised in
/// `available_commands_update`. A free function (not a `From` impl) because
/// `AcpSlashCommandSpec` lives in `commands` and `AvailableCommand` in the ACP
/// SDK — neither is local to this crate, so the orphan rule forbids the impl.
fn available_command_from_spec(spec: &AcpSlashCommandSpec) -> AvailableCommand {
    AvailableCommand::new(spec.name, spec.description).input(
        spec.input_hint
            .map(|hint| AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(hint))),
    )
}

/// One turn driver for controller requests and A2A messages. All notifications,
/// permission requests, questions and cancellation use the bound ACP connection.
async fn drive_prompt(
    req: PromptRequest,
    registry: SharedSessionRegistry,
    config: SdkAcpConfig,
    cx: ConnectionTo<Client>,
) -> Result<(PromptResponse, String), AcpError> {
    let (prompt_text, images) = extract_content_from_blocks(&req.prompt)?;
    let trace_id = req
        .meta
        .as_ref()
        .and_then(|m| m.get("traceId").and_then(|v| v.as_str().map(String::from)));
    let sid = req.session_id.to_string();
    let admitted_engine = registry
        .engine(&sid)
        .ok_or_else(|| AcpError::invalid_params(format!("unknown sessionId: {sid}")))?;
    let (admitted_epoch, is_paused) = admitted_engine.tool_control();
    if is_paused {
        return Err(AcpError::invalid_params(
            "Tool execution is paused by the user",
        ));
    }
    let cx_inner = cx.clone();
    let cx_perm = cx;
    // Same-session ordering: wait (asynchronously) for this
    // session's lane; other sessions are unaffected.
    let _lane = registry.enter_lane(&sid).await;
    let Some(engine) = registry.engine(&sid) else {
        return Err(AcpError::invalid_params(format!(
            "unknown sessionId: {sid}"
        )));
    };
    let (current_epoch, is_paused) = engine.tool_control();
    if is_paused || current_epoch != admitted_epoch || !Arc::ptr_eq(&engine, &admitted_engine) {
        return Err(AcpError::invalid_params(
            "Session control changed while the prompt was queued",
        ));
    }
    let session_cwd = registry.cwd(&sid);
    let cwd_lease = registry.cwd_lease();
    let subagents = registry.subagents(&sid);
    if let Some(subagents) = &subagents {
        subagents.set_connection(cx_inner.clone());
    }
    let expected = req
        .meta
        .as_ref()
        .and_then(|m| m.get("sudocode"))
        .and_then(|m| m.get("expectedContext"))
        .map(|value| {
            serde_json::from_value::<engine_core::engine_events::ContextRef>(value.clone())
        })
        .transpose()
        .map_err(|e| AcpError::invalid_params(e.to_string()))?;
    let is_slash_command = prompt_text.starts_with('/');
    let holds_cwd_lease =
        is_slash_command && session_ops::slash_command_holds_cwd_lease(&prompt_text);
    // `!<cmd>` bash mode: runs in the session workspace, no
    // model turn — same contract as the REPL. Hosts may
    // prepend `<system-reminder>` notes to the user's text;
    // those are skipped before looking for the `!`.
    let bang_command = commands::bash_mode::parse_bang_prompt(&prompt_text).map(str::to_string);

    let is_management = is_slash_command || bang_command.is_some();
    let driver = registry
        .driver(&sid)
        .ok_or_else(|| AcpError::internal("missing engine session driver"))?;
    let (_listener, mut events) = driver.listen(cx_inner.clone());
    let mut assistant_text = String::new();
    let mut committed_context = None;
    let result: Result<(StopReason, Option<PromptUsage>), AcpError>;
    let _cwd_guard = if holds_cwd_lease {
        let cwd = session_cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        Some(
            tokio::task::spawn_blocking(move || cwd_lease.acquire(&cwd))
                .await
                .map_err(|e| AcpError::internal(e.to_string()))?
                .map_err(|e| AcpError::internal(e.to_string()))?,
        )
    } else {
        None
    };
    {
        let mut blocks: Vec<_> = images
            .into_iter()
            .map(
                |(data, mime_type)| engine_core::engine_events::ContentBlock::Image {
                    data,
                    mime_type,
                },
            )
            .collect();
        let prompt_text_for_management = prompt_text.clone();
        blocks.push(engine_core::engine_events::ContentBlock::Text { text: prompt_text });
        let context = Some(engine_core::PromptContext {
            trace_id: trace_id.clone().unwrap_or_default(),
            expected_context: expected,
            image_policy: engine_core::engine_events::ImageInputPolicy::DescribeUnsupported,
        });
        let command = if is_management {
            engine_core::EngineCommand::Management {
                line: prompt_text_for_management,
                context,
                build: engine_core::engine_events::ManagementContext {
                    agent_version: config.agent_version.clone(),
                    git_sha: config.git_sha.clone(),
                    build_target: config.build_target.clone(),
                },
            }
        } else {
            engine_core::EngineCommand::Prompt { blocks, context }
        };
        driver.send(command).map_err(AcpError::internal)?;
        let mut replies =
            futures::stream::FuturesUnordered::<futures::future::BoxFuture<'_, ()>>::new();
        result = loop {
            tokio::select! {
                biased;
                _ = replies.next(), if !replies.is_empty() => {}
                event = events.recv() => match event {
                    Some(EngineEvent::TextDelta { text }) => assistant_text.push_str(&text),
                    Some(EngineEvent::ContextCommitted(context)) => committed_context = Some(context),
                    Some(EngineEvent::PermissionRequest { id, request }) => {
                        let abort = registry.abort_signal(&sid).expect("registered session abort signal");
                        replies.push(answer_permission(&sid, id, &request, &driver, abort, cx_perm.clone()));
                    }
                    Some(EngineEvent::QuestionRequest { id, request: question_req }) => {
                    let payload = AcpAskUserQuestionRequestPayload {
                        session_id: sid.clone(),
                        tool_call_id: format!("ask-{}", uuid_v4()),
                        title: question_req.title.clone(),
                        description: question_req.description.as_ref().map(|text| text.as_str().to_owned()),
                                            description_format: question_req.description.as_ref().map(|text| text.format_name().to_owned()),
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
                    let cx_reply = cx_perm.clone();
                    let abort = registry.abort_signal(&sid).expect("registered session abort signal");
                    let driver = Arc::clone(&driver);
                    replies.push(async move {
                        let outcome = match serde_json::value::to_raw_value(&payload) {
                            Ok(raw) => {
                                let answer = tokio::select! {
                                    biased;
                                    () = abort.cancelled() => None,
                                    answer = cx_reply.send_request(ClientRequest::ExtMethodRequest(
                                        ExtRequest::new(ACP_ASK_USER_QUESTION_METHOD, StdArc::from(raw)),
                                    )).block_task() => Some(answer),
                                };
                                match answer {
                                    None => Err("turn cancelled".into()),
                                    Some(Ok(resp)) => {
                                        serde_json::from_value::<AcpAskUserQuestionResponsePayload>(resp)
                                            .map_err(|error| format!("deserialize: {error}"))
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
                                    Some(Err(error)) => Err(error.to_string()),
                                }
                            }
                            Err(error) => Err(error.to_string()),
                        };
                        let _ = driver.send(engine_core::EngineCommand::QuestionAnswer { id, answers: outcome });
                    }.boxed());
                    }
                    Some(EngineEvent::TurnComplete(complete)) => {
                        let auto_compacted = complete.auto_compaction.as_ref().is_some_and(|event| event.removed_message_count > 0 || event.report.as_ref().is_none_or(|report| report.outcome == runtime::CompactionOutcome::TargetMet));
                        if !is_management { session_ops::record_turn_usage(&engine, &complete); }
                        let usage = if is_management { None } else { session_ops::build_prompt_usage(&engine, &complete, auto_compacted) };
                        break Ok((if complete.cancelled { StopReason::Cancelled } else { StopReason::EndTurn }, usage));
                    }
                    Some(EngineEvent::PromptRejected { message }) => break Err(AcpError::invalid_params(message)),
                    Some(EngineEvent::TurnFailed { message, context_window }) => break Err(AcpError::Turn { message, context_window }),
                    Some(EngineEvent::Error { message }) => break Err(AcpError::internal(message)),
                    Some(_) => {}
                    None => break Err(AcpError::internal("engine session closed before turn completion")),
                }
            }
        };
        drop(replies);
    }

    let context = committed_context;
    match result {
        Ok((stop_reason, prompt_usage)) => {
            let mut response = PromptResponse::new(stop_reason);
            if let Some(u) = prompt_usage {
                let sudocode_meta = sudocode_meta_from_prompt_usage(&u);
                let mut meta = Map::new();
                meta.insert("sudocode".to_string(), json!(sudocode_meta));
                response = response
                    .usage(
                        Usage::new(u.total_tokens, u.input_tokens, u.output_tokens)
                            .cached_read_tokens(u.cache_read_tokens)
                            .cached_write_tokens(u.cache_write_tokens),
                    )
                    .meta(Some(meta));
            }
            if let Some(context) = context {
                let meta = response.meta.get_or_insert_with(Map::new);
                let ns = meta
                    .entry("sudocode".to_string())
                    .or_insert_with(|| json!({}));
                ns["contextRef"] = json!(context);
            }
            Ok((response, assistant_text))
        }
        Err(error) => {
            if matches!(error, AcpError::InvalidParams(_)) {
                return Err(error);
            }
            let user_message = error.user_friendly_message();
            let error_notification = SessionNotification::new(
                sid.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(&user_message),
                ))),
            );
            let _ = cx_inner.send_notification(error_notification);
            Err(error)
        }
    }
}

/// Normalize peer text into the same driver used by `session/prompt`.
/// The durable conversation cursor advances after the turn returns. The bound
/// controller receives streaming output, approval requests and questions.
///
/// One receiver per agent this process serves, not one per process: sessions
/// in different workspaces are different agents, and each hears only its own
/// peers. A message goes to a session of the agent it was sent to, never to a
/// session that merely happens to share the process.
async fn ensure_a2a_receiver(
    registry: &SharedSessionRegistry,
    cx: &ConnectionTo<Client>,
    config: &SdkAcpConfig,
    session_id: &str,
) {
    let Some(agent) = registry.agent_of(session_id) else {
        return;
    };
    // The mailbox the session's tools send through: the hosted agent's, else
    // the peer namespace the session build attached this agent to.
    let mailbox = match &registry.hosted_factory {
        Some(factory) => factory.mailbox(),
        None => engine_host::peer::attach(&agent)
            .ok()
            .map(|peer| peer.mailbox()),
    };
    let Some(mailbox) = mailbox else {
        return;
    };
    let (receiver, mut rx) = loop {
        let admission = {
            // Closing the last session and admitting its replacement use this
            // same lock order. A stopped receiver stays registered until its
            // real worker has dropped its locks and finished its remote
            // unlock attempts. A failed unlock still relies on backend TTL.
            let sessions = registry.lock_sessions();
            if !sessions.contains_key(session_id) {
                return;
            }
            let mut receivers = registry.lock_peer_receivers();
            if let Some(previous) = receivers.get(&agent) {
                if !previous.stop.is_aborted() {
                    return;
                }
                Err(Arc::clone(previous))
            } else {
                let stop = HookAbortSignal::new();
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(
                    runtime::agent_mailbox::MailboxEnvelope,
                    std::sync::mpsc::SyncSender<bool>,
                )>();
                let callback_stop = stop.clone();
                let worker = runtime::mailbox::spawn_inbox_poller(
                    Arc::clone(&mailbox),
                    200,
                    "acp-peer",
                    stop.clone(),
                    move |message| {
                        if message.body.trim().is_empty() {
                            return true;
                        }
                        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
                        if tx.send((message.clone(), ack_tx)).is_err() {
                            return false;
                        }
                        while !callback_stop.is_aborted() {
                            match ack_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                                Ok(handled) => return handled,
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                    return false
                                }
                            }
                        }
                        false
                    },
                );
                let receiver = Arc::new(PeerReceiver {
                    stop,
                    worker: Mutex::new(PeerReceiverWorker::Running(worker)),
                });
                receivers.insert(agent.clone(), Arc::clone(&receiver));
                Ok((receiver, rx))
            }
        };
        match admission {
            Ok(started) => break started,
            Err(previous) => {
                let joining = Arc::clone(&previous);
                match tokio::task::spawn_blocking(move || joining.stop_and_join()).await {
                    Ok(Ok(())) => {}
                    error => {
                        eprintln!("[acp-peer] previous receiver teardown failed: {error:?}");
                        return;
                    }
                }
                let mut receivers = registry.lock_peer_receivers();
                if receivers
                    .get(&agent)
                    .is_some_and(|entry| Arc::ptr_eq(entry, &previous))
                {
                    receivers.remove(&agent);
                }
            }
        }
    };
    let stop = receiver.stop.clone();
    let registry = Arc::clone(registry);
    let cx = cx.clone();
    let config = config.clone();
    let mut bound = session_id.to_string();
    tokio::spawn(async move {
        loop {
            let inbound = tokio::select! {
                () = stop.cancelled() => break,
                inbound = rx.recv() => inbound,
            };
            let Some((message, ack)) = inbound else {
                break;
            };
            // The session this agent's messages go to: the one bound so far
            // while it lives, else another session of the same agent.
            let Some(session_id) = registry.session_of_agent(&agent, &bound) else {
                let _ = ack.send(false);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            };
            bound.clone_from(&session_id);
            if registry
                .engine(&session_id)
                .is_some_and(|engine| engine.tool_control().1)
            {
                let _ = ack.send(false);
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            let body = runtime::agent_mailbox::neutralize_untrusted_markup(&message.body);
            let text = format!("[message from {}]\n\n{body}", message.from);
            let mut meta = Map::new();
            meta.insert(
                "sudocode".into(),
                json!({"a2a":{"from":message.from,"kind":message.kind}}),
            );
            let notification = SessionNotification::new(
                session_id.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(&text),
                ))),
            )
            .meta(meta);
            if cx.send_notification(notification).is_err() {
                break;
            }
            let sends_before = mailbox.sends_so_far();
            let request = PromptRequest::new(
                session_id.clone(),
                vec![ContentBlock::Text(TextContent::new(text))],
            );
            let result = tokio::select! {
                biased;
                () = stop.cancelled() => {
                    let _ = ack.send(false);
                    break;
                }
                result = drive_prompt(request, Arc::clone(&registry), config.clone(), cx.clone()) => result,
            };
            if let Ok((response, text)) = &result {
                if response.stop_reason != StopReason::Cancelled {
                    if let Some(body) = runtime::spawn_task::auto_reply_body(
                        &message.kind,
                        text,
                        sends_before,
                        mailbox.sends_so_far(),
                    ) {
                        let reply = runtime::agent_mailbox::MailboxEnvelope {
                            from: mailbox.self_id().to_string(),
                            to: message.from.clone(),
                            body,
                            summary: None,
                            timestamp: 0,
                            color: None,
                            kind: runtime::agent_mailbox::kinds::AUTO_REPLY.to_string(),
                            request_id: message.request_id.clone(),
                        };
                        let sender = Arc::clone(&mailbox);
                        if let Ok(Err(error)) =
                            tokio::task::spawn_blocking(move || sender.send(reply)).await
                        {
                            eprintln!("[acp-peer] deliver reply: {error}");
                        }
                    }
                }
            }
            // Only a completed, persisted turn (including explicit cancellation)
            // acknowledges delivery. Provider errors and shutdown stay unread.
            let _ = ack.send(result.is_ok() && !stop.is_aborted());
        }
        stop.abort();
        match tokio::task::spawn_blocking(move || receiver.stop_and_join()).await {
            Ok(Ok(())) => {}
            error => eprintln!("[acp-peer] receiver teardown failed: {error:?}"),
        }
    });
}

/// The `session/update` notification advertising `specs` to the client
/// (`sessionUpdate: "available_commands_update"`, `availableCommands: [...]`).
fn available_commands_notification(
    session_id: &str,
    specs: &[AcpSlashCommandSpec],
) -> SessionNotification {
    SessionNotification::new(
        session_id.to_string(),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
            specs.iter().map(available_command_from_spec).collect(),
        )),
    )
}

/// Build the `_meta` map for the `initialize` response. Currently advertises
/// sudocode's image-handling capability under `_meta.sudocode.imageCapability`
/// so ACP clients (sudowork) can downsample / route around oversized + wrong-
/// model image cases without surfacing a user-visible error.
///
/// See [`runtime::image_registry::capability`] for the source of truth; design
/// rationale in `docs/design/image-handling-non-user-facing.html`.
fn initialize_meta(subagent_events: Option<u64>) -> Map<String, serde_json::Value> {
    let cap = runtime::image_registry::capability();
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
    // Feature flags for clients: `session/new` / `session/load` accept
    // `_meta.sudocode.systemPrompt` and `_meta.sudocode.appendSystemPrompt`
    // (see `system_prompt_overrides_from_meta`).
    sudocode_ns.insert("systemPromptOverride".to_string(), json!(true));
    sudocode_ns.insert("systemPromptAppend".to_string(), json!(true));
    sudocode_ns.insert("toolPause".to_string(), json!({ "version": 1 }));
    // `_sudocode/a2a/send` and `_sudocode/a2a/agent_list`.
    sudocode_ns.insert(
        "directA2a".to_string(),
        json!({
            "version": 1, "errorOutcomes": 1, "hostAudit": 1,
        }),
    );
    // `session/new` accepts `_meta.sudocode.forkFrom` (see
    // `fork_source_from_meta`): a client that forks a conversation into a
    // new directory gates on this instead of probing.
    sudocode_ns.insert("sessionFork".to_string(), json!(true));
    // `session/new` / `session/load` accept `_meta.sudocode.memory`
    // ("enabled" | "disabled"), the per-session memory switch (see
    // `memory_mode_from_meta`).
    sudocode_ns.insert("sessionMemory".to_string(), json!(true));
    // Only echoed to a client that asked: advertising it unasked would
    // change the `initialize` result every other client sees.
    if let Some(version) = subagent_events {
        sudocode_ns.insert(
            SUBAGENT_EVENTS_META_KEY.to_string(),
            json!({ "version": version, "cancel": true }),
        );
    }
    let mut meta = Map::new();
    meta.insert("sudocode".to_string(), json!(sudocode_ns));
    meta
}

/// The running host owns both path spellings. Clients must not derive its
/// native directory from a control-plane or procfs path.
fn session_response_meta(
    registry: &SessionRegistry,
    session_id: &str,
    mut sudocode: Map<String, serde_json::Value>,
) -> Option<Map<String, serde_json::Value>> {
    if let Some(engine) = registry.engine(session_id) {
        if let (Ok(file_root), Some(shell_root)) =
            (engine.workspace_root(), registry.cwd(session_id))
        {
            sudocode.insert(
                "workspace".into(),
                json!({ "fileRoot": file_root, "shellRoot": shell_root }),
            );
        }
        // A hosted agent's mailbox is injected by its daemon, not attached.
        if let (None, Some(agent)) = (&registry.hosted_factory, registry.agent_of(session_id)) {
            sudocode.insert("peer".into(), json!(engine_host::peer::status(&agent)));
        }
    }
    (!sudocode.is_empty())
        .then(|| Map::from_iter([("sudocode".into(), serde_json::Value::Object(sudocode))]))
}

/// `_meta.sudocode` key: replace the built-in static system-prompt blocks.
pub const SYSTEM_PROMPT_META_KEY: &str = "systemPrompt";
/// `_meta.sudocode` key: append a trailing dynamic system-prompt block.
pub const APPEND_SYSTEM_PROMPT_META_KEY: &str = "appendSystemPrompt";

/// Read `_meta.sudocode.systemPrompt` / `_meta.sudocode.appendSystemPrompt`
/// from a `session/new` or `session/load` request.
///
/// Each key is optional and they compose. A present key must be a non-empty
/// string, otherwise `invalid_params` — a client that mistypes a value
/// learns about it instead of silently getting the default prompt. The
/// text is passed through verbatim: no truncation, no escaping, no size
/// cap (the model's context window is the real limit, and the API reports
/// that explicitly).
fn system_prompt_overrides_from_meta(
    meta: Option<&agent_client_protocol_schema::Meta>,
) -> Result<SystemPromptOverrides, AcpError> {
    let ns = meta.and_then(|m| m.get("sudocode"));
    Ok(SystemPromptOverrides {
        system_prompt: non_empty_string_meta(ns, SYSTEM_PROMPT_META_KEY)?,
        append_system_prompt: non_empty_string_meta(ns, APPEND_SYSTEM_PROMPT_META_KEY)?,
    })
}

/// `_meta.sudocode` key: whether this session uses memory.
pub const MEMORY_META_KEY: &str = "memory";
/// Accepted values of [`MEMORY_META_KEY`], in the order they are reported
/// back in an `invalid_params` message.
const MEMORY_META_VALUES: [(&str, runtime::memory::MemoryMode); 2] = [
    ("enabled", runtime::memory::MemoryMode::Enabled),
    ("disabled", runtime::memory::MemoryMode::Disabled),
];

/// Read `_meta.sudocode.memory` from a `session/new` / `session/load`
/// request.
///
/// Absent → [`runtime::memory::MemoryMode::Enabled`], which is exactly
/// today's behaviour: a client that never sends the key sees no change at
/// all. `"disabled"` stands memory down for **this session only** — the
/// process serves other sessions with their own modes, and nothing under the
/// memory directory is read, written or removed, so a later session that
/// omits the key finds the same entries.
///
/// A string outside the accepted set, or a non-string value, is
/// `invalid_params` rather than a silent default — same rule as the
/// system-prompt keys, and the one that matters most here, since silently
/// ignoring a mistyped `"disable"` would leave memory on while the caller
/// believed it off.
fn memory_mode_from_meta(
    meta: Option<&agent_client_protocol_schema::Meta>,
) -> Result<runtime::memory::MemoryMode, AcpError> {
    let ns = meta.and_then(|m| m.get("sudocode"));
    let Some(value) = ns.and_then(|ns| ns.get(MEMORY_META_KEY)) else {
        return Ok(runtime::memory::MemoryMode::Enabled);
    };
    let accepted = MEMORY_META_VALUES
        .iter()
        .map(|(name, _)| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(" or ");
    let Some(text) = value.as_str() else {
        return Err(AcpError::invalid_params(format!(
            "_meta.sudocode.{MEMORY_META_KEY} must be a string ({accepted})"
        )));
    };
    MEMORY_META_VALUES
        .iter()
        .find(|(name, _)| *name == text.trim())
        .map(|(_, mode)| *mode)
        .ok_or_else(|| {
            AcpError::invalid_params(format!(
                "_meta.sudocode.{MEMORY_META_KEY} must be {accepted} (got {text:?})"
            ))
        })
}

/// `_meta.sudocode` key on `session/new`: start the new session from a copy
/// of another session's transcript.
pub const FORK_FROM_META_KEY: &str = "forkFrom";

/// Where a forked session's transcript comes from
/// (`_meta.sudocode.forkFrom` on `session/new`).
///
/// At least one of the two fields is present:
///
/// * `session_id` alone — the source must be open in this process;
/// * `cwd` alone — the most recently updated session persisted under that
///   directory's session store;
/// * both — the persisted session `session_id` under `cwd`'s store (or the
///   open session of that id, which must live in `cwd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionForkSource {
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
}

/// Parse `_meta.sudocode.forkFrom` from a `session/new` request. Absent →
/// `None`; present but malformed → `invalid_params` (never silently ignored,
/// like the system-prompt keys).
fn fork_source_from_meta(
    meta: Option<&agent_client_protocol_schema::Meta>,
) -> Result<Option<SessionForkSource>, AcpError> {
    let ns = meta.and_then(|m| m.get("sudocode"));
    let Some(value) = ns.and_then(|ns| ns.get(FORK_FROM_META_KEY)) else {
        return Ok(None);
    };
    let Some(object) = value.as_object() else {
        return Err(AcpError::invalid_params(format!(
            "_meta.sudocode.{FORK_FROM_META_KEY} must be an object"
        )));
    };
    let field = |key: &str| -> Result<Option<String>, AcpError> {
        match object.get(key) {
            None => Ok(None),
            Some(v) => match v.as_str() {
                Some(text) if !text.trim().is_empty() => Ok(Some(text.to_string())),
                Some(_) => Err(AcpError::invalid_params(format!(
                    "_meta.sudocode.{FORK_FROM_META_KEY}.{key} must not be empty"
                ))),
                None => Err(AcpError::invalid_params(format!(
                    "_meta.sudocode.{FORK_FROM_META_KEY}.{key} must be a string"
                ))),
            },
        }
    };
    let session_id = field("sessionId")?;
    let cwd = field("cwd")?.map(PathBuf::from);
    if session_id.is_none() && cwd.is_none() {
        return Err(AcpError::invalid_params(format!(
            "_meta.sudocode.{FORK_FROM_META_KEY} needs sessionId and/or cwd"
        )));
    }
    Ok(Some(SessionForkSource { session_id, cwd }))
}

fn non_empty_string_meta(
    sudocode_ns: Option<&serde_json::Value>,
    key: &str,
) -> Result<Option<String>, AcpError> {
    let Some(value) = sudocode_ns.and_then(|ns| ns.get(key)) else {
        return Ok(None);
    };
    match value.as_str() {
        Some(text) if !text.trim().is_empty() => Ok(Some(text.to_string())),
        Some(_) => Err(AcpError::invalid_params(format!(
            "_meta.sudocode.{key} must not be empty"
        ))),
        None => Err(AcpError::invalid_params(format!(
            "_meta.sudocode.{key} must be a string"
        ))),
    }
}

fn sudocode_meta_from_prompt_usage(u: &PromptUsage) -> Map<String, serde_json::Value> {
    let mut sudocode_meta = Map::new();
    if let Some(is_complete) = u.token_breakdown_complete {
        sudocode_meta.insert("tokenBreakdownComplete".to_string(), json!(is_complete));
    }
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
    if u.auto_compacted {
        sudocode_meta.insert("autoCompacted".to_string(), json!(true));
    }
    sudocode_meta
}

#[cfg(test)]
mod tests {
    use super::{
        acp_mcp_servers_to_scoped, sudocode_meta_from_prompt_usage, AcpError, CumulativeUsage,
        PromptUsage, CONTEXT_WINDOW_EXCEEDED_MESSAGE,
    };
    use agent_client_protocol_schema::{
        EnvVariable, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
    };
    use runtime::config::{ConfigSource, McpServerConfig};
    use runtime::UsageCostCurrency;
    use std::path::{Path, PathBuf};

    #[test]
    fn turn_error_classification_comes_from_typed_bits_not_strings() {
        // A context-window turn failure is recognized by the runtime's typed
        // bit — even when the rendered message contains none of the legacy
        // keyword strings — and maps to the context-window guidance.
        let cw = AcpError::from_turn_error(&runtime::RuntimeError::context_window_blocked(
            "the provider said no",
        ));
        assert!(matches!(
            cw,
            AcpError::Turn {
                context_window: true,
                ..
            }
        ));
        assert_eq!(cw.user_friendly_message(), CONTEXT_WINDOW_EXCEEDED_MESSAGE);

        // A generic (non-context-window) turn failure keeps its own message and
        // is NOT misclassified as a context-window error.
        let generic = AcpError::from_turn_error(&runtime::RuntimeError::new("upstream 503"));
        assert!(matches!(
            generic,
            AcpError::Turn {
                context_window: false,
                ..
            }
        ));
        assert_ne!(
            generic.user_friendly_message(),
            CONTEXT_WINDOW_EXCEEDED_MESSAGE
        );
        assert!(generic.user_friendly_message().contains("upstream 503"));
    }

    #[test]
    fn prompt_usage_meta_includes_cost_without_standard_usage_tokens() {
        let meta = sudocode_meta_from_prompt_usage(&PromptUsage {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14,
            token_breakdown_complete: None,
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
            auto_compacted: false,
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
    fn acp_mcp_servers_preserves_http_sse() {
        let http = McpServer::Http(McpServerHttp::new("h", "https://e"));
        let sse = McpServer::Sse(McpServerSse::new("s", "https://e"));
        let out = acp_mcp_servers_to_scoped(&[http, sse], Path::new("/tmp"));
        assert!(
            matches!(&out["h"].config, McpServerConfig::Http(remote) if remote.url == "https://e")
        );
        assert!(
            matches!(&out["s"].config, McpServerConfig::Sse(remote) if remote.url == "https://e")
        );
    }

    #[test]
    fn acp_mcp_servers_mixed() {
        let stdio = McpServer::Stdio(McpServerStdio::new("keep", PathBuf::from("/bin/k")));
        let http = McpServer::Http(McpServerHttp::new("remote", "https://e"));
        let out = acp_mcp_servers_to_scoped(&[stdio, http], Path::new("/tmp"));
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("keep"));
        assert!(out.contains_key("remote"));
    }
}

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
/// * the working directory of each session: every turn runs with it as the
///   thread's [`runtime::workspace_root`] scope, and the runtime-construction
///   paths additionally take the [`WorkspaceCwdLease`] for it (see there).
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    completion_routes: Mutex<HashMap<String, CompletionRoute>>,
    cwd_lease: Arc<WorkspaceCwdLease>,
    hosted_factory: Option<Arc<dyn HostedSessionFactory>>,
    /// Own the worker until it has stopped; an abort signal alone does not
    /// release its remote receiver seat.
    peer_receivers: Mutex<HashMap<String, Arc<PeerReceiver>>>,
}

struct PeerReceiver {
    stop: HookAbortSignal,
    worker: Mutex<PeerReceiverWorker>,
}

enum PeerReceiverWorker {
    Running(std::thread::JoinHandle<()>),
    Joined(Result<(), String>),
}

impl PeerReceiver {
    /// Called on a blocking worker, never on the async executor. Keep the
    /// mutex until join completes so concurrent close/replacement callers
    /// cannot mistake a taken handle for a completed retirement.
    fn stop_and_join(&self) -> Result<(), String> {
        self.stop.abort();
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match std::mem::replace(&mut *worker, PeerReceiverWorker::Joined(Ok(()))) {
            PeerReceiverWorker::Running(handle) => handle
                .join()
                .map_err(|_| "peer receiver worker panicked".to_string()),
            PeerReceiverWorker::Joined(result) => result,
        };
        *worker = PeerReceiverWorker::Joined(result.clone());
        result
    }
}

/// The host chooses where a session lives; the ACP handlers and turn driver are
/// shared with stdio and WebSocket. A hosted run owns one durable transcript.
pub trait HostedSessionFactory: Send + Sync {
    fn mailbox(&self) -> Option<Arc<runtime::mailbox::Mailbox>> {
        None
    }
    fn close(&self) {}
    fn state_changed(&self, _state: engine_core::EngineState) {}
    fn open(
        &self,
        requested_id: Option<&str>,
        mcp_servers: BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
        memory: runtime::memory::MemoryMode,
    ) -> Result<(Arc<SessionEngine>, PathBuf), AcpError>;
}

struct SessionEntry {
    /// The single-session engine this ACP session wraps — `Arc` so a turn in
    /// flight keeps it alive across a concurrent `session/close`.
    engine: Arc<SessionEngine>,
    /// The agent this session answers as, read once at registration: the
    /// engine's own lock is held for a whole turn, so asking it later from
    /// under the registry lock would stall every session behind one turn.
    agent: String,
    driver: Arc<crate::session_driver::SessionDriver>,
    /// A clone of the engine's abort signal, held here so `session/cancel` fires
    /// without taking any session lock.
    abort: HookAbortSignal,
    lane: Arc<tokio::sync::Mutex<()>>,
    cwd: PathBuf,
    /// When the session was registered — for the `session/close` duration metric.
    started_at: std::time::Instant,
    /// Present when the client opted into sub-agent events.
    subagents: Option<Arc<SessionSubagents>>,
}

/// Async guard for a session lane; drop it to let the next request on the
/// same session proceed.
pub type SessionLaneGuard = tokio::sync::OwnedMutexGuard<()>;

struct CompletionRoute {
    driver: std::sync::Weak<Delivery>,
    retirement: engine_core::CompletionRetirement,
}

struct CompletionRouteSlot<'a> {
    routes: std::sync::MutexGuard<'a, HashMap<String, CompletionRoute>>,
    driver: Option<Arc<Delivery>>,
}

impl CompletionRoute {
    fn new(driver: &Arc<Delivery>) -> Self {
        Self {
            driver: Arc::downgrade(driver),
            retirement: driver.completion_retirement(),
        }
    }

    fn retained(&self) -> bool {
        self.driver.strong_count() > 0 || !self.retirement.wait(std::time::Duration::ZERO)
    }
}

impl SessionRegistry {
    fn completion_route_slot(&self, session_id: &str) -> Result<CompletionRouteSlot<'_>, AcpError> {
        loop {
            let mut routes = self
                .completion_routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            routes.retain(|_, route| route.retained());
            let Some(route) = routes.get(session_id) else {
                return Ok(CompletionRouteSlot {
                    routes,
                    driver: None,
                });
            };
            if let Some(driver) = route.driver.upgrade() {
                return Ok(CompletionRouteSlot {
                    routes,
                    driver: Some(driver),
                });
            }
            // Weak::upgrade can fail before the detached engine drops its
            // journal/native writer. Await that exact lifetime outside the
            // registry lock, so other sessions can still be admitted.
            let retirement = route.retirement.clone();
            drop(routes);
            if !retirement.wait(std::time::Duration::from_secs(30)) {
                return Err(AcpError::internal(
                    "completion engine teardown is not finished",
                ));
            }
            // Another caller may have installed the successor while we waited.
            // Recheck under the same lock rather than replace its ownership.
        }
    }

    pub fn with_hosted_factory(factory: Arc<dyn HostedSessionFactory>) -> Self {
        Self {
            hosted_factory: Some(factory),
            ..Self::default()
        }
    }

    fn completion_route(
        &self,
        session: &runtime::Session,
        cwd: &std::path::Path,
        create: bool,
    ) -> Result<Arc<Delivery>, AcpError> {
        let CompletionRouteSlot { mut routes, driver } =
            self.completion_route_slot(&session.session_id)?;
        if let Some(route) = driver {
            route
                .attach_session(session, cwd)
                .map_err(AcpError::internal)?;
            return Ok(route);
        }
        let route = Delivery::open(session, cwd, create).map_err(AcpError::internal)?;
        routes.insert(session.session_id.clone(), CompletionRoute::new(&route));
        Ok(route)
    }

    pub(crate) fn attach_completion_writer(
        &self,
        session: &runtime::Session,
        cwd: &std::path::Path,
    ) -> Result<(), AcpError> {
        let CompletionRouteSlot {
            routes: _routes,
            driver,
        } = self.completion_route_slot(&session.session_id)?;
        if let Some(route) = driver {
            route
                .attach_session(session, cwd)
                .map_err(AcpError::internal)?;
        }
        Ok(())
    }

    fn stored_completion_route(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> Result<Arc<Delivery>, AcpError> {
        let CompletionRouteSlot { mut routes, driver } = self.completion_route_slot(session_id)?;
        if let Some(route) = driver {
            route
                .control(CompletionAction::CheckBinding {
                    session_id: session_id.into(),
                    cwd: cwd.into(),
                })
                .map_err(AcpError::invalid_params)?;
            return Ok(route);
        }
        let route = Delivery::stored(session_id, cwd).map_err(AcpError::invalid_params)?;
        routes.insert(session_id.into(), CompletionRoute::new(&route));
        Ok(route)
    }

    fn task_control(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        storage_ref: Option<&str>,
        operation: TaskOperation,
    ) -> Result<(Option<Arc<Delivery>>, TaskResponse), AcpError> {
        let route = self.stored_completion_route(session_id, cwd)?;
        route.set_task_protocol(true);
        let is_probe = matches!(operation, TaskOperation::Replay(_)) && storage_ref.is_none();
        let is_purge = matches!(operation, TaskOperation::Purge(_));
        let required = || {
            storage_ref
                .map(str::to_owned)
                .ok_or_else(|| AcpError::invalid_params("storageRef required"))
        };
        let action = match operation {
            TaskOperation::Replay(after) => CompletionAction::Replay {
                storage_ref: storage_ref.map(str::to_owned),
                after,
                legacy: false,
            },
            TaskOperation::Suppress(trace) => CompletionAction::Suppress {
                storage_ref: required()?,
                trace,
            },
            TaskOperation::Purge(cleanup_id) => CompletionAction::Purge {
                storage_ref: required()?,
                cleanup_id,
            },
        };
        let value = route.control(action).map_err(AcpError::internal)?;
        let response =
            serde_json::from_value(value).map_err(|e| AcpError::internal(e.to_string()))?;
        Ok((
            if is_probe || is_purge {
                None
            } else {
                Some(route)
            },
            response,
        ))
    }

    pub fn cancel_all(&self) {
        self.stop_peer_receivers();
        for entry in self.lock_sessions().values() {
            let _ = entry.driver.send(engine_core::EngineCommand::Cancel);
            entry.abort.abort();
        }
    }

    pub fn close_all(&self) {
        self.stop_peer_receivers();
        let entries: Vec<_> = self
            .lock_sessions()
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        for entry in entries {
            let _ = entry.driver.send(engine_core::EngineCommand::Cancel);
            entry.abort.abort();
            drop(entry.engine);
            if !entry.driver.shutdown() {
                eprintln!("ACP session teardown timed out");
            }
        }
        let receivers: Vec<_> = self
            .lock_peer_receivers()
            .drain()
            .map(|(_, receiver)| receiver)
            .collect();
        for receiver in receivers {
            if let Err(error) = receiver.stop_and_join() {
                eprintln!("ACP peer receiver teardown failed: {error}");
            }
        }
        if let Some(factory) = &self.hosted_factory {
            factory.close();
        }
    }

    fn lock_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionEntry>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_peer_receivers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<PeerReceiver>>> {
        self.peer_receivers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn stop_peer_receivers(&self) {
        for receiver in self.lock_peer_receivers().values() {
            receiver.stop.abort();
        }
    }

    /// Stop `agent`'s receiver once no session of that agent is left, so its
    /// receiver seat passes to another instance instead of being held by a
    /// process that has nowhere to deliver.
    fn stop_unused_peer_receiver(&self, agent: &str) -> Option<Arc<PeerReceiver>> {
        let sessions = self.lock_sessions();
        let in_use = sessions.values().any(|entry| entry.agent == agent);
        if !in_use {
            if let Some(receiver) = self.lock_peer_receivers().get(agent) {
                receiver.stop.abort();
                return Some(Arc::clone(receiver));
            }
        }
        None
    }

    /// The agent `session_id` answers as.
    fn agent_of(&self, session_id: &str) -> Option<String> {
        self.lock_sessions()
            .get(session_id)
            .map(|entry| entry.agent.clone())
    }

    /// The session `agent`'s peer messages go to: `preferred` while it is still
    /// a session of that agent, else the longest-running other one.
    fn session_of_agent(&self, agent: &str, preferred: &str) -> Option<String> {
        let sessions = self.lock_sessions();
        if sessions
            .get(preferred)
            .is_some_and(|entry| entry.agent == agent)
        {
            return Some(preferred.to_string());
        }
        sessions
            .iter()
            .filter(|(_, entry)| entry.agent == agent)
            .min_by_key(|(_, entry)| entry.started_at)
            .map(|(id, _)| id.clone())
    }

    /// Register (or re-register) a session's engine after `session/new` /
    /// `session/load`. The abort signal is taken from the engine so
    /// `session/cancel` needs no session lock.
    pub fn register(
        &self,
        session_id: String,
        engine: Arc<SessionEngine>,
        cwd: PathBuf,
        subagents: Option<Arc<SessionSubagents>>,
    ) -> Result<(), AcpError> {
        let (lane, existing) = {
            let sessions = self.lock_sessions();
            if sessions
                .get(&session_id)
                .is_some_and(|entry| entry.engine.tool_control().1)
            {
                return Err(AcpError::invalid_params(
                    "Return tool control before reloading the session",
                ));
            }
            let lane = sessions.get(&session_id).map_or_else(
                || Arc::new(tokio::sync::Mutex::new(())),
                |entry| Arc::clone(&entry.lane),
            );
            (
                lane,
                sessions
                    .get(&session_id)
                    .map(|entry| Arc::clone(&entry.driver)),
            )
        };
        let abort = engine.abort_signal();
        let agent = engine.agent_name();
        let driver = crate::session_driver::SessionDriver::start(
            Arc::clone(&engine),
            subagents.as_ref(),
            session_id.clone(),
            existing,
            self.hosted_factory.clone(),
        )
        .map_err(AcpError::internal)?;
        self.lock_sessions().insert(
            session_id,
            SessionEntry {
                engine,
                agent,
                driver,
                abort,
                lane,
                cwd,
                started_at: std::time::Instant::now(),
                subagents,
            },
        );
        Ok(())
    }

    fn pause_tools(&self, session_id: &str) -> Result<(Arc<SessionEngine>, u64), AcpError> {
        let sessions = self.lock_sessions();
        let entry = sessions
            .get(session_id)
            .ok_or_else(|| AcpError::invalid_params("Unknown session"))?;
        let epoch = entry
            .engine
            .begin_tool_pause()
            .map_err(AcpError::internal)?;
        let _ = entry.driver.send(engine_core::EngineCommand::Cancel);
        entry.abort.abort();
        Ok((Arc::clone(&entry.engine), epoch))
    }

    /// The session's sub-agent event route, if its client opted in.
    #[must_use]
    pub fn subagents(&self, session_id: &str) -> Option<Arc<SessionSubagents>> {
        self.lock_sessions()
            .get(session_id)
            .and_then(|e| e.subagents.clone())
    }

    fn driver(&self, session_id: &str) -> Option<Arc<crate::session_driver::SessionDriver>> {
        self.lock_sessions()
            .get(session_id)
            .map(|entry| Arc::clone(&entry.driver))
    }

    /// The engine for a session; `None` for an unknown sessionId.
    #[must_use]
    pub fn engine(&self, session_id: &str) -> Option<Arc<SessionEngine>> {
        self.lock_sessions()
            .get(session_id)
            .map(|e| Arc::clone(&e.engine))
    }

    /// Remove a session after `session/close`, returning its engine + the time
    /// it was registered so the caller can record the close telemetry before the
    /// last `Arc` drops.
    fn take(
        &self,
        session_id: &str,
    ) -> Option<(
        Arc<SessionEngine>,
        std::time::Instant,
        Arc<crate::session_driver::SessionDriver>,
        Option<Arc<PeerReceiver>>,
    )> {
        let taken = self.lock_sessions().remove(session_id)?;
        let receiver = self.stop_unused_peer_receiver(&taken.agent);
        Some((taken.engine, taken.started_at, taken.driver, receiver))
    }

    /// All live sessions as `(id, cwd)` — the `session/list` response.
    #[must_use]
    pub fn list(&self) -> Vec<(String, PathBuf)> {
        self.lock_sessions()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.cwd.clone()))
            .collect()
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

    /// The session's lane if nothing holds it now. `None` for unknown sessions
    /// and for sessions busy with a turn or another request.
    #[must_use]
    pub fn try_enter_lane(&self, session_id: &str) -> Option<SessionLaneGuard> {
        let lane = self
            .lock_sessions()
            .get(session_id)
            .map(|e| Arc::clone(&e.lane))?;
        lane.try_lock_owned().ok()
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
/// Turns no longer touch the process cwd at all: everything a turn does —
/// the tool loop, hooks, config, the session store — resolves paths against
/// the session's [`runtime::workspace_root`] scope, so turns of sessions in
/// different directories run fully concurrently. What still resolves against
/// the process cwd is *runtime construction*: `session/new`, `session/load`,
/// `session/setModel` and the `/model` slash command rebuild a session's
/// runtime, which spawns config-declared MCP servers and plugin processes
/// that inherit the process cwd. Those paths are rare, short and never park
/// on user input, so they simply run under this lease:
///
/// * holders whose sessions share a cwd hold the lease **together** (it is
///   reference-counted) and run concurrently;
/// * a holder for a *different* cwd waits until the current holders are gone.
///
/// The lease only ever *sets* the cwd on acquisition; when the last holder
/// leaves, the cwd is left as is (nothing outside a lease may depend on it —
/// in particular no turn does).
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
    pub fn acquire(self: &Arc<Self>, cwd: &std::path::Path) -> std::io::Result<CwdLeaseGuard> {
        self.enter(cwd)?;
        Ok(CwdLeaseGuard {
            lease: Arc::clone(self),
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

/// RAII holder of a [`WorkspaceCwdLease`] acquisition.
pub struct CwdLeaseGuard {
    lease: Arc<WorkspaceCwdLease>,
}

impl Drop for CwdLeaseGuard {
    fn drop(&mut self) {
        self.lease.leave();
    }
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
    #[serde(skip_serializing_if = "Option::is_none")]
    description_format: Option<String>,
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

/// Ask the client to decide one engine permission request, then hand the
/// decision back to the engine. Cancelling the session denies it.
fn answer_permission(
    session_id: &str,
    id: engine_core::RequestId,
    request: &PermissionRequest,
    driver: &Arc<crate::session_driver::SessionDriver>,
    abort: HookAbortSignal,
    cx: ConnectionTo<Client>,
) -> futures::future::BoxFuture<'static, ()> {
    let request = build_acp_permission_request(session_id.to_string(), request);
    let driver = Arc::clone(driver);
    async move {
        let answer = tokio::select! {
            biased;
            () = abort.cancelled() => None,
            answer = cx.send_request(request).block_task() => Some(answer),
        };
        let decision = match answer {
            Some(Ok(response)) => map_permission_response(response),
            _ => PermissionPromptDecision::Deny {
                reason: "ACP permission request cancelled or failed".into(),
            },
        };
        let _ = driver.send(engine_core::EngineCommand::PermissionAnswer { id, decision });
    }
    .boxed()
}

/// The session, tool and tool input a direct A2A request asks for.
fn direct_a2a_call(
    message: agent_client_protocol::UntypedMessage,
) -> Result<(String, &'static str, serde_json::Value), DirectToolError> {
    let (method, params) = message.into_parts();
    let invalid = |error: serde_json::Error| DirectToolError::Refused(error.to_string());
    if method == DIRECT_SEND_METHOD {
        let request: DirectSendRequest = serde_json::from_value(params).map_err(invalid)?;
        let input = json!({
            "to": request.to,
            "message": request.message,
            "summary": request.summary,
        });
        Ok((request.session_id, "send", input))
    } else {
        let request: DirectAgentListRequest = serde_json::from_value(params).map_err(invalid)?;
        let input = request.active_only.map_or_else(
            || json!({}),
            |active_only| json!({ "active_only": active_only }),
        );
        Ok((request.session_id, "agent_list", input))
    }
}

/// A direct A2A failure on the wire, typed as `initialize` advertises with
/// `errorOutcomes: 1`. A refusal is invalid params whose outcome is
/// `not_submitted`: nothing ran, so the client may correct the request and
/// send it again. Anything after admission is an internal error whose outcome
/// is `unknown`: the message may have been sent, so the client must not resend
/// it blindly.
fn direct_a2a_error(error: &DirectToolError) -> Error {
    let typed = |outcome: &str| json!({ "directA2a": { "version": 1, "outcome": outcome } });
    match error {
        DirectToolError::Refused(reason) => Error::invalid_params().data(json!({
            "detail": reason,
            "sudocode": typed("not_submitted"),
        })),
        DirectToolError::Unknown(detail) => {
            let mut error = Error::internal_error().data(json!({
                "outcome": "unknown",
                "detail": detail,
                "sudocode": typed("unknown"),
            }));
            error.message = "The call may have run; its outcome is unknown".into();
            error
        }
    }
}

/// Run one of the session's own A2A tools for the client, outside any turn,
/// through the hooks and permission policy a model's call passes.
///
/// A direct call never waits behind a turn: a busy session is a definite
/// refusal before anything runs, so the caller never has to guess whether a
/// queued send went out after it stopped waiting.
async fn drive_direct_tool(
    registry: SharedSessionRegistry,
    session_id: String,
    name: &'static str,
    input: serde_json::Value,
    cx: ConnectionTo<Client>,
) -> Result<DirectA2aResponse, DirectToolError> {
    let refuse = |reason: &str| DirectToolError::Refused(reason.to_string());
    let admitted = registry
        .engine(&session_id)
        .ok_or_else(|| DirectToolError::Refused(format!("unknown sessionId: {session_id}")))?;
    let (admitted_epoch, is_paused) = admitted.tool_control();
    if is_paused {
        return Err(refuse("Tool execution is paused by the user"));
    }
    let Some(_lane) = registry.try_enter_lane(&session_id) else {
        return Err(refuse("The session is busy; nothing was sent"));
    };
    let unchanged = registry.engine(&session_id).is_some_and(|engine| {
        let (epoch, is_paused) = engine.tool_control();
        !is_paused && epoch == admitted_epoch && Arc::ptr_eq(&engine, &admitted)
    });
    if !unchanged {
        return Err(refuse(
            "Session control changed while the request was admitted",
        ));
    }
    let (Some(driver), Some(abort)) = (
        registry.driver(&session_id),
        registry.abort_signal(&session_id),
    ) else {
        return Err(refuse("Unknown session"));
    };
    let (_listener, mut events) = driver.listen(cx.clone());
    let reply = driver.direct_tool(name, input.to_string());
    tokio::pin!(reply);
    let mut replies =
        futures::stream::FuturesUnordered::<futures::future::BoxFuture<'static, ()>>::new();
    let value = loop {
        tokio::select! {
            biased;
            _ = replies.next(), if !replies.is_empty() => {}
            value = &mut reply => break value,
            event = events.recv() => match event {
                Some(EngineEvent::PermissionRequest { id, request }) => replies.push(
                    answer_permission(&session_id, id, &request, &driver, abort.clone(), cx.clone()),
                ),
                Some(_) => {}
                None => break Err(DirectToolError::Unknown(
                    "the engine session closed before replying".into(),
                )),
            },
        }
    };
    let value = value?;
    if let Some(reason) = value.get("refused").and_then(serde_json::Value::as_str) {
        return Err(refuse(reason));
    }
    serde_json::from_value(value).map_err(|e| DirectToolError::Unknown(e.to_string()))
}

/// Build an ACP `RequestPermissionRequest` from a runtime `PermissionRequest`.
fn build_acp_permission_request(
    session_id: String,
    request: &PermissionRequest,
) -> RequestPermissionRequest {
    let tool_call = ToolCallUpdate::new(
        format!("perm-{}", uuid_v4()),
        ToolCallUpdateFields::new()
            .title(request.tool_name.clone())
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
/// `Lines`). Each ACP session wraps one [`SessionEngine`] (the seam) held in the
/// [`SessionRegistry`]. Each session owns one shared `EngineSession`; handlers
/// send commands and translate its events onto the ACP connection.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_acp_on_transport(
    config: &SdkAcpConfig,
    registry: SharedSessionRegistry,
    transport: impl ConnectTo<Agent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent_version = config.agent_version.clone();
    let hosted = registry.hosted_factory.is_some();
    // Per connection: whether this client opted into sub-agent events at
    // `initialize`. Sessions it creates or loads get a sub-agent route.
    let subagent_events = Arc::new(AtomicBool::new(false));
    let subagent_completions = Arc::new(AtomicBool::new(false));
    let task_completions = Arc::new(AtomicBool::new(false));

    Agent
        .builder()
        .name("scode")
        // --- initialize ---
        .on_receive_request(
            {
                let version = agent_version.clone();
                let subagent_events = Arc::clone(&subagent_events);
                let subagent_completions = Arc::clone(&subagent_completions);
                let task_completions = Arc::clone(&task_completions);
                async move |req: InitializeRequest,
                            responder: Responder<InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    let subagent_version =
                        subagent_events_version(req.client_capabilities.meta.as_ref());
                    subagent_events.store(subagent_version.is_some(), Ordering::SeqCst);
                    let tasks = !hosted
                        && subagent_completions::negotiated(
                            req.client_capabilities.meta.as_ref(),
                            subagent_completions::TASK_META_KEY,
                        );
                    task_completions.store(tasks, Ordering::SeqCst);
                    let completion_version = tasks
                        || (!hosted
                            && subagent_completions::negotiated(
                                req.client_capabilities.meta.as_ref(),
                                subagent_completions::META_KEY,
                            ));
                    subagent_completions.store(completion_version, Ordering::SeqCst);
                    let mut meta = initialize_meta(subagent_version);
                    if completion_version {
                        meta.get_mut("sudocode").unwrap()[if tasks {
                            subagent_completions::TASK_META_KEY
                        } else {
                            subagent_completions::META_KEY
                        }] = json!({"version": 1});
                    }
                    if hosted {
                        meta.get_mut("sudocode").unwrap()["sessionFork"] = json!(false);
                    }
                    let resp = InitializeResponse::new(req.protocol_version)
                        .agent_info(Implementation::new("scode", &version))
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
                                .prompt_capabilities(PromptCapabilities::new().image(true))
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .close(SessionCloseCapabilities::new())
                                        .list(SessionListCapabilities::new()),
                                ),
                        )
                        .meta(meta);
                    responder.respond(resp)?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/new ---
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                let config = config.clone();
                let subagent_events = Arc::clone(&subagent_events);
                let subagent_completions = Arc::clone(&subagent_completions);
                let task_completions = Arc::clone(&task_completions);
                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let prompt_overrides =
                        match system_prompt_overrides_from_meta(req.meta.as_ref()) {
                            Ok(v) => v,
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                                return Ok(());
                            }
                        };
                    let memory = match memory_mode_from_meta(req.meta.as_ref()) {
                        Ok(v) => v,
                        Err(e) => {
                            responder.respond_with_error(acp_error_to_sdk(&e))?;
                            return Ok(());
                        }
                    };
                    let fork_source = match fork_source_from_meta(req.meta.as_ref()) {
                        Ok(v) => v,
                        Err(e) => {
                            responder.respond_with_error(acp_error_to_sdk(&e))?;
                            return Ok(());
                        }
                    };
                    let registry = Arc::clone(&registry);
                    let config = config.clone();
                    let commands = session_ops::available_commands();
                    let peer_config = config.clone();
                    let cx_notify = cx.clone();
                    let completions_enabled = subagent_completions.load(Ordering::SeqCst);
                    let tasks_enabled = task_completions.load(Ordering::SeqCst);
                    let subagent_events =
                        subagent_events.load(Ordering::SeqCst) || completions_enabled;
                    cx.spawn(async move {
                        let lease = registry.cwd_lease();
                        let build_registry = Arc::clone(&registry);
                        let result = tokio::task::spawn_blocking(move || {
                            if let Some(factory) = &build_registry.hosted_factory {
                                if fork_source.is_some() {
                                    return Err(AcpError::invalid_params(
                                        "fork requires a separate managed session",
                                    ));
                                }
                                let mcp_servers =
                                    acp_mcp_servers_to_scoped(&req.mcp_servers, &req.cwd);
                                let (engine, cwd) =
                                    factory.open(None, mcp_servers, prompt_overrides, memory)?;
                                let session_id = engine.session_handle().id;
                                return Ok((engine, session_id, cwd));
                            }
                            // Runtime construction: the engine resolves config /
                            // model / permission mode against the workspace-root
                            // scope, but the MCP servers + plugins it spawns
                            // inherit the process cwd, so it runs under the lease.
                            let lease_cwd = session_lease_cwd(&req.cwd);
                            let _cwd = lease.acquire(&lease_cwd).map_err(|e| {
                                AcpError::internal(format!("failed to enter cwd: {e}"))
                            })?;
                            let _scope = WorkspaceRootScope::enter(lease_cwd);
                            let mcp_servers = acp_mcp_servers_to_scoped(&req.mcp_servers, &req.cwd);
                            let (engine, cwd) = match fork_source {
                                Some(source) => session_ops::open_forked_session(
                                    &config,
                                    &source,
                                    &build_registry,
                                    req.cwd,
                                    mcp_servers,
                                    prompt_overrides,
                                    memory,
                                )?,
                                None => session_ops::build_new_session(
                                    &config,
                                    req.cwd,
                                    mcp_servers,
                                    prompt_overrides,
                                    memory,
                                )?,
                            };
                            let session_id = engine.session_handle().id;
                            Ok::<_, AcpError>((engine, session_id, cwd))
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok((engine, session_id, cwd)) => {
                                let completion = if completions_enabled {
                                    match registry.completion_route(
                                        &engine.session_snapshot(),
                                        &cwd,
                                        true,
                                    ) {
                                        Ok(route) => {
                                            route.set_task_protocol(tasks_enabled);
                                            Some(route)
                                        }
                                        Err(error) => {
                                            responder
                                                .respond_with_error(acp_error_to_sdk(&error))?;
                                            return Ok(());
                                        }
                                    }
                                } else {
                                    None
                                };
                                let storage_ref = match &completion {
                                    Some(route) => route
                                        .control_async(CompletionAction::Status)
                                        .await
                                        .ok()
                                        .and_then(|status| status.get("storageRef").cloned()),
                                    None => None,
                                };
                                let subagents = subagent_events.then(|| {
                                    SessionSubagents::start(
                                        session_id.clone(),
                                        cx_notify.clone(),
                                        completion,
                                    )
                                });
                                let register = Arc::clone(&registry);
                                let registered_id = session_id.clone();
                                if let Err(error) = tokio::task::spawn_blocking(move || {
                                    register.register(registered_id, engine, cwd, subagents)
                                })
                                .await
                                .unwrap_or_else(|error| Err(AcpError::internal(error.to_string())))
                                {
                                    responder.respond_with_error(acp_error_to_sdk(&error))?;
                                    return Ok(());
                                }
                                let mut sudocode = Map::new();
                                if tasks_enabled {
                                    sudocode.insert("storageRef".into(), json!(storage_ref));
                                }
                                responder.respond(
                                    NewSessionResponse::new(session_id.clone()).meta(
                                        session_response_meta(&registry, &session_id, sudocode),
                                    ),
                                )?;
                                let _ = cx_notify.send_notification(
                                    available_commands_notification(&session_id, commands),
                                );
                                ensure_a2a_receiver(
                                    &registry,
                                    &cx_notify,
                                    &peer_config,
                                    &session_id,
                                )
                                .await;
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
                let registry = Arc::clone(&registry);
                let config = config.clone();
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    let registry = Arc::clone(&registry);
                    let config = config.clone();
                    let connection = cx.clone();
                    cx.spawn(async move {
                        match drive_prompt(req, registry, config, connection).await {
                            Ok((response, _)) => responder.respond(response)?,
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(&error))?,
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
                    if let Some(driver) = registry.driver(&notif.session_id.to_string()) {
                        let _ = driver.send(engine_core::EngineCommand::Cancel);
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        // --- session/close ---
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                async move |req: CloseSessionRequest,
                            responder: Responder<CloseSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let registry = Arc::clone(&registry);
                    let sid = req.session_id.to_string();
                    cx.spawn(async move {
                        // Queue behind any in-flight turn on this session.
                        let _lane = registry.enter_lane(&sid).await;
                        let taken = registry.take(&sid);
                        let closed = tokio::task::spawn_blocking(move || {
                            if let Some((engine, started_at, driver, receiver)) = taken {
                                // Session-ended telemetry, then persist + drop.
                                if let Some(tracer) = engine.session_tracer() {
                                    let usage = engine.usage_snapshot();
                                    let cumulative = usage.cumulative_usage();
                                    let duration_ms = started_at.elapsed().as_millis() as u64;
                                    tracer.record_usage(
                                        "session_summary".to_string(),
                                        cumulative.input_tokens,
                                        cumulative.output_tokens,
                                        cumulative.cache_creation_input_tokens,
                                        cumulative.cache_read_input_tokens,
                                    );
                                    tracer.record_session_ended(
                                        usage.turns(),
                                        u64::from(cumulative.input_tokens),
                                        u64::from(cumulative.output_tokens),
                                        duration_ms,
                                    );
                                }
                                drop(engine);
                                let driver_stopped = driver.shutdown();
                                let receiver_stopped =
                                    receiver.map_or(Ok(()), |receiver| receiver.stop_and_join());
                                if !driver_stopped {
                                    return Err(AcpError::internal("session teardown timed out"));
                                }
                                receiver_stopped.map_err(AcpError::internal)?;
                            }
                            Ok(())
                        })
                        .await;
                        match closed {
                            Ok(Ok(())) => responder.respond(CloseSessionResponse::new())?,
                            Ok(Err(error)) => {
                                responder.respond_with_error(acp_error_to_sdk(&error))?;
                            }
                            Err(error) => responder.respond_with_error(
                                Error::internal_error().data(error.to_string()),
                            )?,
                        }
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
                let registry = Arc::clone(&registry);
                async move |_req: ListSessionsRequest,
                            responder: Responder<ListSessionsResponse>,
                            _cx: ConnectionTo<Client>| {
                    let infos = registry
                        .list()
                        .into_iter()
                        .map(|(id, cwd)| SessionInfo::new(id, cwd))
                        .collect::<Vec<_>>();
                    responder.respond(ListSessionsResponse::new(infos))?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/setModel (unstable) ---
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                async move |req: SetSessionModelRequest,
                            responder: Responder<SetSessionModelResponse>,
                            cx: ConnectionTo<Client>| {
                    let registry = Arc::clone(&registry);
                    let sid = req.session_id.to_string();
                    let model_id: String = req.model_id.0.to_string();
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&sid).await;
                        let session_cwd = registry.cwd(&sid);
                        let lease = registry.cwd_lease();
                        let Some(engine) = registry.engine(&sid) else {
                            responder.respond_with_error(acp_error_to_sdk(
                                &AcpError::invalid_params(format!("unknown sessionId: {sid}")),
                            ))?;
                            return Ok(());
                        };
                        let result = tokio::task::spawn_blocking(move || {
                            // The model switch rebuilds the session runtime
                            // (runtime construction — under the cwd lease).
                            let _scope = session_cwd.clone().map(WorkspaceRootScope::enter);
                            let _cwd = match session_cwd {
                                Some(cwd) => Some(lease.acquire(&cwd).map_err(|e| {
                                    AcpError::internal(format!("failed to enter session cwd: {e}"))
                                })?),
                                None => None,
                            };
                            EngineDelegate::set_model(&*engine, &model_id)
                                .map_err(AcpError::internal)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok(_) => responder.respond(SetSessionModelResponse::new())?,
                            Err(e) => responder.respond_with_error(acp_error_to_sdk(&e))?,
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
                let registry = Arc::clone(&registry);
                let config = config.clone();
                let subagent_events = Arc::clone(&subagent_events);
                let subagent_completions = Arc::clone(&subagent_completions);
                let task_completions = Arc::clone(&task_completions);
                async move |req: LoadSessionRequest,
                            responder: Responder<LoadSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let prompt_overrides =
                        match system_prompt_overrides_from_meta(req.meta.as_ref()) {
                            Ok(v) => v,
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                                return Ok(());
                            }
                        };
                    let memory = match memory_mode_from_meta(req.meta.as_ref()) {
                        Ok(v) => v,
                        Err(e) => {
                            responder.respond_with_error(acp_error_to_sdk(&e))?;
                            return Ok(());
                        }
                    };
                    let registry = Arc::clone(&registry);
                    let config = config.clone();
                    let commands = session_ops::available_commands();
                    let peer_config = config.clone();
                    let cx_notify = cx.clone();
                    let sid = req.session_id.to_string();
                    let cwd = req.cwd;
                    let completions_enabled = subagent_completions.load(Ordering::SeqCst);
                    let tasks_enabled = task_completions.load(Ordering::SeqCst);
                    let subagent_events =
                        subagent_events.load(Ordering::SeqCst) || completions_enabled;
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&sid).await;
                        let lease = registry.cwd_lease();
                        let factory = registry.hosted_factory.clone();
                        let live = if completions_enabled {
                            registry.engine(&sid).zip(registry.cwd(&sid))
                        } else {
                            None
                        };
                        let load_registry = Arc::clone(&registry);
                        let result = tokio::task::spawn_blocking(move || {
                            if let Some((engine, live_cwd)) = live {
                                let cwd = engine_host::session::canonical_session_cwd(&cwd)
                                    .map_err(AcpError::invalid_params)?;
                                if live_cwd != cwd {
                                    return Err(AcpError::invalid_params("session cwd mismatch"));
                                }
                                // A live engine retains its prompt/memory snapshot and
                                // children. New settings apply on a true cold rebuild.
                                return Ok((engine, sid, cwd));
                            }
                            if let Some(factory) = factory {
                                let mcp_servers = acp_mcp_servers_to_scoped(&req.mcp_servers, &cwd);
                                let (engine, cwd) = factory.open(
                                    Some(&sid),
                                    mcp_servers,
                                    prompt_overrides,
                                    memory,
                                )?;
                                let session_id = engine.session_handle().id;
                                return Ok((engine, session_id, cwd));
                            }
                            let lease_cwd = session_lease_cwd(&cwd);
                            let _cwd = lease.acquire(&lease_cwd).map_err(|e| {
                                AcpError::internal(format!("failed to enter cwd: {e}"))
                            })?;
                            let _scope = WorkspaceRootScope::enter(lease_cwd);
                            let mcp_servers = acp_mcp_servers_to_scoped(&req.mcp_servers, &cwd);
                            let (engine, cwd) = session_ops::open_loaded_session(
                                &config,
                                &load_registry,
                                &sid,
                                cwd,
                                mcp_servers,
                                prompt_overrides,
                                memory,
                            )?;
                            let session_id = engine.session_handle().id;
                            Ok::<_, AcpError>((engine, session_id, cwd))
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok((engine, session_id, cwd)) => {
                                // A reload of a live session keeps its route, so
                                // background agents it already spawned still
                                // reach the client.
                                let completion = if completions_enabled {
                                    match registry.completion_route(
                                        &engine.session_snapshot(),
                                        &cwd,
                                        false,
                                    ) {
                                        Ok(route) => {
                                            route.set_task_protocol(tasks_enabled);
                                            Some(route)
                                        }
                                        Err(error) => {
                                            responder
                                                .respond_with_error(acp_error_to_sdk(&error))?;
                                            return Ok(());
                                        }
                                    }
                                } else {
                                    None
                                };
                                let (context, storage_ref) = match &completion {
                                    Some(route) => {
                                        match route.control_async(CompletionAction::Status).await {
                                            Ok(value) => (
                                                value
                                                    .get("contextRef")
                                                    .cloned()
                                                    .filter(|value| !value.is_null()),
                                                value.get("storageRef").cloned(),
                                            ),
                                            Err(error) => {
                                                responder.respond_with_error(acp_error_to_sdk(
                                                    &AcpError::internal(error),
                                                ))?;
                                                return Ok(());
                                            }
                                        }
                                    }
                                    None => (None, None),
                                };
                                let subagents = subagent_events.then(|| {
                                    registry.subagents(&session_id).unwrap_or_else(|| {
                                        SessionSubagents::start(
                                            session_id.clone(),
                                            cx_notify.clone(),
                                            completion,
                                        )
                                    })
                                });
                                if let Some(route) = &subagents {
                                    route.set_connection(cx_notify.clone());
                                }
                                let register = Arc::clone(&registry);
                                let registered_id = session_id.clone();
                                if let Err(error) = tokio::task::spawn_blocking(move || {
                                    register.register(registered_id, engine, cwd, subagents)
                                })
                                .await
                                .unwrap_or_else(|error| Err(AcpError::internal(error.to_string())))
                                {
                                    responder.respond_with_error(acp_error_to_sdk(&error))?;
                                    return Ok(());
                                }
                                let mut sudocode = Map::new();
                                if completions_enabled {
                                    sudocode.insert("contextRef".into(), json!(context));
                                    sudocode.insert("storageRef".into(), json!(storage_ref));
                                }
                                responder.respond(LoadSessionResponse::new().meta(
                                    session_response_meta(&registry, &session_id, sudocode),
                                ))?;
                                let _ = cx_notify.send_notification(
                                    available_commands_notification(&session_id, commands),
                                );
                                ensure_a2a_receiver(
                                    &registry,
                                    &cx_notify,
                                    &peer_config,
                                    &session_id,
                                )
                                .await;
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
        // --- durable sub-agent journal replay (no engine construction) ---
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                let enabled = Arc::clone(&subagent_completions);
                let task_enabled = Arc::clone(&task_completions);
                async move |req: ReplayRequest,
                            responder: Responder<ReplayResponse>,
                            cx: ConnectionTo<Client>| {
                    if !enabled.load(Ordering::SeqCst) || task_enabled.load(Ordering::SeqCst) {
                        responder.respond_with_error(Error::method_not_found())?;
                        return Ok(());
                    }
                    let registry = Arc::clone(&registry);
                    let sender = cx.clone();
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let route =
                                registry.stored_completion_route(&req.session_id, &req.cwd)?;
                            let value = route
                                .control(CompletionAction::Replay {
                                    storage_ref: None,
                                    after: None,
                                    legacy: true,
                                })
                                .map_err(AcpError::internal)?;
                            let status: ReplayResponse = serde_json::from_value(value)
                                .map_err(|e| AcpError::internal(e.to_string()))?;
                            Ok((route, status))
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok((route, status)) => {
                                route.bind(sender);
                                responder.respond(status)?;
                            }
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(&error))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                let enabled = Arc::clone(&task_completions);
                async move |req: TaskReplayRequest,
                            responder: Responder<TaskResponse>,
                            cx: ConnectionTo<Client>| {
                    if !enabled.load(Ordering::SeqCst) {
                        responder.respond_with_error(Error::method_not_found())?;
                        return Ok(());
                    }
                    let registry = Arc::clone(&registry);
                    let sender = cx.clone();
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            registry.task_control(
                                &req.session_id,
                                &req.cwd,
                                req.storage_ref.as_deref(),
                                TaskOperation::Replay(req.after_task_id),
                            )
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok((route, response)) => {
                                if let Some(route) = route {
                                    route.bind(sender);
                                }
                                responder.respond(response)?;
                            }
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(&error))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                let enabled = Arc::clone(&task_completions);
                async move |req: TaskSuppressRequest,
                            responder: Responder<TaskResponse>,
                            cx: ConnectionTo<Client>| {
                    if !enabled.load(Ordering::SeqCst) {
                        responder.respond_with_error(Error::method_not_found())?;
                        return Ok(());
                    }
                    let registry = Arc::clone(&registry);
                    let sender = cx.clone();
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            registry.task_control(
                                &req.session_id,
                                &req.cwd,
                                req.storage_ref.as_deref(),
                                TaskOperation::Suppress(req.origin_trace_id),
                            )
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok((route, response)) => {
                                if let Some(route) = route {
                                    route.bind(sender);
                                }
                                responder.respond(response)?;
                            }
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(&error))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                let enabled = Arc::clone(&task_completions);
                async move |req: TaskPurgeRequest,
                            responder: Responder<TaskResponse>,
                            cx: ConnectionTo<Client>| {
                    if !enabled.load(Ordering::SeqCst) {
                        responder.respond_with_error(Error::method_not_found())?;
                        return Ok(());
                    }
                    let registry = Arc::clone(&registry);
                    let sender = cx.clone();
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            registry.task_control(
                                &req.session_id,
                                &req.cwd,
                                req.storage_ref.as_deref(),
                                TaskOperation::Purge(req.cleanup_id),
                            )
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok((route, response)) => {
                                if let Some(route) = route {
                                    route.bind(sender);
                                }
                                responder.respond(response)?;
                            }
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(&error))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/setPermissionMode (custom extension) ---
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                async move |req: SetPermissionModeRequest,
                            responder: Responder<SetPermissionModeResponse>,
                            cx: ConnectionTo<Client>| {
                    let registry = Arc::clone(&registry);
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&req.session_id).await;
                        let Some(engine) = registry.engine(&req.session_id) else {
                            responder.respond_with_error(acp_error_to_sdk(
                                &AcpError::invalid_params(format!(
                                    "unknown sessionId: {}",
                                    req.session_id
                                )),
                            ))?;
                            return Ok(());
                        };
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
                                Ok(m) => EngineDelegate::set_permission_mode(&*engine, m)
                                    .map_err(AcpError::internal),
                                Err(e) => Err(e),
                            }
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok(()) => responder.respond(SetPermissionModeResponse {})?,
                            Err(e) => responder.respond_with_error(acp_error_to_sdk(&e))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // Pause acknowledges only after the main lane and actual child/tool work drain.
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                async move |req: PauseToolsRequest,
                            responder: Responder<PauseToolsResponse>,
                            cx: ConnectionTo<Client>| {
                    let registry = Arc::clone(&registry);
                    cx.spawn(async move {
                        let (engine, epoch) = match registry.pause_tools(&req.session_id) {
                            Ok(paused) => paused,
                            Err(error) => {
                                responder.respond_with_error(acp_error_to_sdk(&error))?;
                                return Ok(());
                            }
                        };
                        let duration = std::time::Duration::from_secs(30);
                        let drained = tokio::time::timeout(duration, async {
                            let _lane = registry.enter_lane(&req.session_id).await;
                            if !registry
                                .engine(&req.session_id)
                                .is_some_and(|current| Arc::ptr_eq(&current, &engine))
                            {
                                return Err(
                                    "Session closed or changed while tool operations were stopping"
                                        .to_string(),
                                );
                            }
                            let engine = Arc::clone(&engine);
                            tokio::task::spawn_blocking(move || {
                                engine.wait_tool_pause(epoch, duration)
                            })
                            .await
                            .map_err(|error| error.to_string())?
                        })
                        .await;
                        match drained {
                            Ok(Ok(())) => responder.respond(PauseToolsResponse {
                                pause_epoch: epoch,
                                is_quiescent: true,
                            })?,
                            Ok(Err(error)) => responder
                                .respond_with_error(acp_error_to_sdk(&AcpError::internal(error)))?,
                            Err(_) => {
                                responder
                                    .respond_with_error(acp_error_to_sdk(&AcpError::internal(
                                    "Tool operations are still stopping; control remains fenced",
                                )))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                async move |req: ResumeToolsRequest,
                            responder: Responder<ResumeToolsResponse>,
                            cx: ConnectionTo<Client>| {
                    let registry = Arc::clone(&registry);
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&req.session_id).await;
                        let Some(engine) = registry.engine(&req.session_id) else {
                            responder.respond_with_error(acp_error_to_sdk(
                                &AcpError::invalid_params("Unknown session"),
                            ))?;
                            return Ok(());
                        };
                        match engine.resume_tool_operations(req.pause_epoch) {
                            Ok(()) => {
                                if let Some(abort) = registry.abort_signal(&req.session_id) {
                                    abort.reset();
                                }
                                responder.respond(ResumeToolsResponse {})?;
                            }
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(
                                &AcpError::invalid_params(error),
                            ))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- _sudocode/agent/cancel (sub-agent events extension) ---
        .on_receive_request(
            {
                let registry = Arc::clone(&registry);
                async move |req: CancelSubagentRequest,
                            responder: Responder<CancelSubagentResponse>,
                            _cx: ConnectionTo<Client>| {
                    let Some(_subagents) = registry.subagents(&req.session_id) else {
                        responder.respond_with_error(acp_error_to_sdk(
                            &AcpError::invalid_params(format!(
                                "session {} is unknown or did not opt into sub-agent events",
                                req.session_id
                            )),
                        ))?;
                        return Ok(());
                    };
                    let Some(driver) = registry.driver(&req.session_id) else {
                        responder.respond_with_error(acp_error_to_sdk(
                            &AcpError::invalid_params("Unknown session"),
                        ))?;
                        return Ok(());
                    };
                    _cx.spawn(async move {
                        match driver.cancel_subagent(req.agent_id).await {
                            Ok(cancelled) => {
                                responder.respond(CancelSubagentResponse { cancelled })?
                            }
                            Err(error) => responder.respond_with_error(acp_error_to_sdk(
                                &AcpError::invalid_params(error),
                            ))?,
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- catch-all: direct A2A requests, then unhandled methods ---
        .on_receive_dispatch(
            {
                let registry = Arc::clone(&registry);
                async move |dispatch: Dispatch, cx: ConnectionTo<Client>| match dispatch {
                    Dispatch::Request(message, responder)
                        if matches!(
                            message.method(),
                            DIRECT_SEND_METHOD | DIRECT_AGENT_LIST_METHOD
                        ) =>
                    {
                        let registry = Arc::clone(&registry);
                        let connection = cx.clone();
                        cx.spawn(async move {
                            let result = match direct_a2a_call(message) {
                                Ok((session_id, name, input)) => {
                                    drive_direct_tool(registry, session_id, name, input, connection)
                                        .await
                                }
                                Err(error) => Err(error),
                            };
                            responder.respond_with_result(
                                result
                                    .and_then(|response| {
                                        serde_json::to_value(response)
                                            .map_err(|e| DirectToolError::Unknown(e.to_string()))
                                    })
                                    .map_err(|error| direct_a2a_error(&error)),
                            )?;
                            Ok(())
                        })?;
                        Ok(Handled::Yes)
                    }
                    message @ (Dispatch::Request(_, _) | Dispatch::Notification(_)) => {
                        message.respond_with_error(Error::method_not_found(), cx)?;
                        Ok(Handled::Yes)
                    }
                    Dispatch::Response(result, router) => {
                        // A cancelled turn drops its wire waiter. The SDK's default
                        // router treats a late reply to that waiter as a connection
                        // error; consume it without closing unrelated live turns.
                        let _ = router.respond_with_result(result);
                        Ok(Handled::Yes)
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
        AcpError::Internal(message) | AcpError::Turn { message, .. } => {
            let mut error =
                Error::internal_error().data(serde_json::Value::String(message.clone()));
            // Clients display the RPC message as the terminal banner. Both
            // manual (Internal) and automatic (Turn) compaction failures need
            // the same guidance. Preserve the typed context-window decision;
            // other failures retain generic RPC text and full diagnostics.
            if message.contains(runtime::COMPACTION_FAILED)
                || matches!(
                    e,
                    AcpError::Turn {
                        context_window: true,
                        ..
                    }
                )
            {
                error.message = e.user_friendly_message();
            }
            error
        }
    }
}
