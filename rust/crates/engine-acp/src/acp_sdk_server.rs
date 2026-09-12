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
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, McpServer,
    NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionId,
    PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionCapabilities, SessionCloseCapabilities, SessionInfo, SessionListCapabilities,
    SessionNotification, SessionUpdate, SetSessionModelRequest, SetSessionModelResponse,
    StopReason, TextContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    UnstructuredCommandInput, Usage,
};
use engine_core::{AuthMode, EngineDelegate, EngineEvent, ObserverAdapter};
use engine_host::config::AllowedToolSet;
use engine_host::{SessionEngine, SessionLifecycle};
use runtime::config::{ConfigSource, McpServerConfig, McpStdioServerConfig, ScopedMcpServerConfig};
use runtime::workspace_root::WorkspaceRootScope;
use runtime::HookAbortSignal;
use runtime::SystemPromptOverrides;
use runtime::UsageCostCurrency;
use runtime::{
    PermissionMode, PermissionPromptDecision, PermissionPrompter, PermissionRequest,
    QuestionPromptAnswer, QuestionPromptRequest, QuestionPrompter,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map};
use std::collections::BTreeMap;
use std::sync::mpsc as std_mpsc;

use crate::session_ops;

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

        // Check for specific error types and provide friendly messages.
        // A context-window rejection that reaches here was not classified by
        // the prompt path (which knows whether history was compactable), so
        // do not guess a sub-class: a long history and a single oversized
        // message are different problems with different fixes.
        if raw_message.contains("context_window_blocked")
            || raw_message.contains("Context window blocked")
        {
            return "[context_window_exceeded] 请求超出了模型的上下文限制。\n\n建议解决方案：\n1. 压缩或清除对话历史后重新开始\n2. 使用较小的图片或简化输入内容\n3. 使用支持更大上下文的模型".to_string();
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

/// Deliver nexus-A2A peer messages to ACP clients, once per process.
///
/// The receive half of standalone A2A was only ever wired into the interactive
/// REPL, which printed `📨 A2A from <peer>` to the terminal. An ACP client —
/// including an agent driving `scode acp` programmatically — saw nothing, so
/// the send half worked and the reply never arrived anywhere it could be read.
///
/// Delivered as a `user_message_chunk` because that is what it is: from this
/// session's point of view the peer is the party talking *to* the agent. Using
/// a standard variant means every existing client renders it with no change;
/// `_meta.sudocode.a2a.from` carries the sender for clients that want to tell
/// peer mail apart from a human's typing.
///
/// Broadcast to every registered session, because the inbox belongs to the
/// PROCESS: `NEXUS_A2A_AGENT` names one agent, and every session this server
/// hosts is that agent. There is no per-session mailbox to route to.
///
/// Started on the first `session/new` rather than at boot — before a session
/// exists there is nobody to notify — and only once, since the poller parks on
/// a blocking tail read of a single inbox.
fn ensure_a2a_receiver(registry: &SharedSessionRegistry, cx: &ConnectionTo<Client>) {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let Ok(Some(a2a)) = engine_host::nexus_a2a::session() else {
        return;
    };

    // The poller is a blocking thread with a sync callback; the ACP connection
    // is async. One channel bridges them, the same shape the turn path uses.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
    let _poller = engine_host::nexus_a2a::spawn_poller(a2a, HookAbortSignal::new(), move |msg| {
        // Hand-off only, and weaker than the REPL's ack: this channel is
        // unbounded and the ACP client's receipt is not observable from here,
        // so the cursor advances once the notification is queued for the
        // client. A send error means the forwarding task is gone, which is a
        // refusal and re-delivers.
        tx.send((msg.from.clone(), msg.body.clone())).is_ok()
    });

    let registry = Arc::clone(registry);
    let cx = cx.clone();
    tokio::spawn(async move {
        while let Some((from, body)) = rx.recv().await {
            let mut meta = Map::new();
            meta.insert(
                "sudocode".to_string(),
                serde_json::json!({ "a2a": { "from": from } }),
            );
            for (session_id, _cwd) in registry.list() {
                let notification = SessionNotification::new(
                    session_id,
                    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(&format!("[message from {from}] {body}")),
                    ))),
                )
                .meta(meta.clone());
                let _ = cx.send_notification(notification);
            }
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
fn initialize_meta() -> Map<String, serde_json::Value> {
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
    // `session/new` accepts `_meta.sudocode.forkFrom` (see
    // `fork_source_from_meta`): a client that forks a conversation into a
    // new directory gates on this instead of probing.
    sudocode_ns.insert("sessionFork".to_string(), json!(true));
    // `session/new` / `session/load` accept `_meta.sudocode.memory`
    // ("enabled" | "disabled"), the per-session memory switch (see
    // `memory_mode_from_meta`).
    sudocode_ns.insert("sessionMemory".to_string(), json!(true));
    let mut meta = Map::new();
    meta.insert("sudocode".to_string(), json!(sudocode_ns));
    meta
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
        acp_mcp_servers_to_scoped, sudocode_meta_from_prompt_usage, CumulativeUsage, PromptUsage,
    };
    use agent_client_protocol_schema::{
        EnvVariable, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
    };
    use runtime::config::{ConfigSource, McpServerConfig};
    use runtime::UsageCostCurrency;
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
    cwd_lease: Arc<WorkspaceCwdLease>,
}

struct SessionEntry {
    /// The single-session engine this ACP session wraps — `Arc` so a turn in
    /// flight keeps it alive across a concurrent `session/close`.
    engine: Arc<SessionEngine>,
    /// A clone of the engine's abort signal, held here so `session/cancel` fires
    /// without taking any session lock.
    abort: HookAbortSignal,
    lane: Arc<tokio::sync::Mutex<()>>,
    cwd: PathBuf,
    /// When the session was registered — for the `session/close` duration metric.
    started_at: std::time::Instant,
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

    /// Register (or re-register) a session's engine after `session/new` /
    /// `session/load`. The abort signal is taken from the engine so
    /// `session/cancel` needs no session lock.
    pub fn register(&self, session_id: String, engine: Arc<SessionEngine>, cwd: PathBuf) {
        let mut sessions = self.lock_sessions();
        // A reload of a session that is already live keeps its lane so that
        // requests already queued behind it stay ordered.
        let lane = sessions.get(&session_id).map_or_else(
            || Arc::new(tokio::sync::Mutex::new(())),
            |e| Arc::clone(&e.lane),
        );
        let abort = engine.abort_signal();
        sessions.insert(
            session_id,
            SessionEntry {
                engine,
                abort,
                lane,
                cwd,
                started_at: std::time::Instant::now(),
            },
        );
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
    pub fn take(&self, session_id: &str) -> Option<(Arc<SessionEngine>, std::time::Instant)> {
        self.lock_sessions()
            .remove(session_id)
            .map(|e| (e.engine, e.started_at))
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
        //
        // While this thread is parked the turn holds nothing shared: its
        // workspace root is a thread-scoped value (see
        // `runtime::workspace_root`), so a parked session cannot hold up any
        // other session.
        tokio::task::block_in_place(|| {
            response_rx
                .blocking_recv()
                .unwrap_or(PermissionPromptDecision::Deny {
                    reason: "permission response channel closed".to_string(),
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
        tokio::task::block_in_place(|| {
            response_rx
                .blocking_recv()
                .unwrap_or_else(|_| Err("question response channel closed".to_string()))
        })
    }
}

struct AcpQuestionBridge {
    tx: tokio::sync::mpsc::UnboundedSender<(
        String,
        QuestionPromptRequest,
        tokio::sync::oneshot::Sender<Result<Vec<QuestionPromptAnswer>, String>>,
    )>,
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
/// `Lines`). Each ACP session wraps one [`SessionEngine`] (the seam) held in the
/// [`SessionRegistry`]; the delegate indirection is gone — the handlers drive the
/// engine directly through the seam, one `run_turn` per `session/prompt`.
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_acp_on_transport(
    config: &SdkAcpConfig,
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
                                .load_session(true)
                                .prompt_capabilities(PromptCapabilities::new().image(true))
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .close(SessionCloseCapabilities::new())
                                        .list(SessionListCapabilities::new()),
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
                let registry = Arc::clone(&registry);
                let config = config.clone();
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
                    let cx_notify = cx.clone();
                    cx.spawn(async move {
                        let lease = registry.cwd_lease();
                        let build_registry = Arc::clone(&registry);
                        let result = tokio::task::spawn_blocking(move || {
                            // Runtime construction: the engine resolves config /
                            // model / permission mode against the workspace-root
                            // scope, but the MCP servers + plugins it spawns
                            // inherit the process cwd, so it runs under the lease.
                            let lease_cwd = session_lease_cwd(&req.cwd);
                            let _cwd = lease.acquire(&lease_cwd).map_err(|e| {
                                AcpError::internal(format!("failed to enter cwd: {e}"))
                            })?;
                            let _scope = WorkspaceRootScope::enter(lease_cwd);
                            let mcp_servers =
                                acp_mcp_servers_to_scoped(&req.mcp_servers, &req.cwd);
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
                                registry.register(session_id.clone(), engine, cwd);
                                responder.respond(NewSessionResponse::new(session_id.clone()))?;
                                let _ = cx_notify.send_notification(
                                    available_commands_notification(&session_id, commands),
                                );
                                ensure_a2a_receiver(&registry, &cx_notify);
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
                    let (prompt_text, images) = match extract_content_from_blocks(&req.prompt) {
                        Ok(r) => r,
                        Err(e) => {
                            responder.respond_with_error(acp_error_to_sdk(&e))?;
                            return Ok(());
                        }
                    };
                    if prompt_text.is_empty() {
                        responder.respond_with_error(acp_error_to_sdk(
                            &AcpError::invalid_params(
                                "prompt must include at least one non-empty text content block",
                            ),
                        ))?;
                        return Ok(());
                    }
                    let trace_id = req.meta.as_ref().and_then(|m| {
                        m.get("traceId").and_then(|v| v.as_str().map(String::from))
                    });

                    let registry = Arc::clone(&registry);
                    let config = config.clone();
                    let sid = req.session_id.to_string();
                    let cx_inner = cx.clone();
                    let cx_perm = cx.clone();
                    cx.spawn(async move {
                        // Same-session ordering: wait (asynchronously) for this
                        // session's lane; other sessions are unaffected.
                        let _lane = registry.enter_lane(&sid).await;
                        let Some(engine) = registry.engine(&sid) else {
                            responder.respond_with_error(acp_error_to_sdk(
                                &AcpError::invalid_params(format!("unknown sessionId: {sid}")),
                            ))?;
                            return Ok(());
                        };
                        let session_cwd = registry.cwd(&sid);
                        let cwd_lease = registry.cwd_lease();
                        let is_slash_command = prompt_text.starts_with('/');
                        let holds_cwd_lease = is_slash_command
                            && session_ops::slash_command_holds_cwd_lease(&prompt_text);

                        // Permission-prompt + question bridge channels (ACP
                        // round-trips) and the engine event → notification bridge.
                        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel::<(
                            PermissionRequest,
                            tokio::sync::oneshot::Sender<PermissionPromptDecision>,
                        )>();
                        let (question_tx, mut question_rx) =
                            tokio::sync::mpsc::unbounded_channel::<(
                                String,
                                QuestionPromptRequest,
                                tokio::sync::oneshot::Sender<
                                    Result<Vec<QuestionPromptAnswer>, String>,
                                >,
                            )>();
                        let (notif_tx, mut notif_rx) =
                            tokio::sync::mpsc::unbounded_channel::<SessionNotification>();

                        // The seam speaks `EngineEvent` over a std mpsc (the pump's
                        // shape); a small forwarder thread maps each event onto the
                        // ACP wire and feeds the async `select!` below — the same
                        // pattern the in-process pump uses for its command channel.
                        let (evt_tx, evt_rx) = std_mpsc::channel::<EngineEvent>();
                        let sid_forward = sid.clone();
                        std::thread::Builder::new()
                            .name("acp-engine-events".into())
                            .spawn(move || {
                                while let Ok(event) = evt_rx.recv() {
                                    if let Some(notification) =
                                        session_ops::engine_event_to_session_update(
                                            &sid_forward,
                                            event,
                                        )
                                    {
                                        if notif_tx.send(notification).is_err() {
                                            break;
                                        }
                                    }
                                }
                            })
                            .expect("spawn acp-engine-events thread");

                        let engine_blocking = Arc::clone(&engine);
                        let config_blocking = config.clone();
                        let session_cwd_blocking = session_cwd.clone();
                        let images_blocking = images.clone();
                        let prompt_blocking = prompt_text.clone();
                        let trace_blocking = trace_id.clone();
                        let blocking_handle = tokio::task::spawn_blocking(move || {
                            // The turn resolves paths against the session's
                            // workspace-root scope (thread-scoped), never the
                            // process cwd, so turns of sessions in other dirs run
                            // concurrently. `/model` (holds_cwd_lease) rebuilds the
                            // runtime, so it runs under the process-cwd lease.
                            let _scope = session_cwd_blocking
                                .clone()
                                .map(WorkspaceRootScope::enter);
                            let _cwd_guard = match (holds_cwd_lease, &session_cwd_blocking) {
                                (true, Some(cwd)) => Some(cwd_lease.acquire(cwd).map_err(|e| {
                                    AcpError::internal(format!("failed to enter session cwd: {e}"))
                                })?),
                                _ => None,
                            };
                            let turn_cwd = session_cwd_blocking
                                .clone()
                                .unwrap_or_else(|| std::path::PathBuf::from("."));

                            if is_slash_command {
                                let (text, stop) = session_ops::handle_slash_command(
                                    &engine_blocking,
                                    &config_blocking,
                                    &turn_cwd,
                                    &prompt_blocking,
                                )?;
                                let _ = evt_tx.send(EngineEvent::TextDelta { text });
                                return Ok::<_, AcpError>((stop, None));
                            }

                            if let Some(tid) = &trace_blocking {
                                engine_blocking.set_trace_id(tid);
                            }
                            if !images_blocking.is_empty() {
                                session_ops::prepare_and_push_images(
                                    &engine_blocking,
                                    &images_blocking,
                                    &turn_cwd,
                                )?;
                            }
                            engine_blocking.set_question_prompter(Box::new(AcpQuestionBridge {
                                tx: question_tx,
                            }));
                            let mut observer = ObserverAdapter::new(evt_tx);
                            let mut bridge = AcpPermissionBridge { tx: bridge_tx };
                            let blocks = vec![runtime::ContentBlock::Text {
                                text: prompt_blocking,
                            }];
                            let complete = engine_blocking
                                .run_turn(blocks, &mut observer, &mut bridge)
                                .map_err(AcpError::internal)?;
                            let auto_compacted = complete.auto_compaction.is_some();
                            session_ops::record_turn_usage(&engine_blocking, &complete);
                            let usage = session_ops::build_prompt_usage(
                                &engine_blocking,
                                &complete,
                                auto_compacted,
                            );
                            let stop = if complete.cancelled {
                                StopReason::Cancelled
                            } else {
                                StopReason::EndTurn
                            };
                            Ok((stop, usage))
                        });

                        // Concurrently serve permission/question requests + stream
                        // notifications while the blocking turn runs.
                        let mut blocking_handle = blocking_handle;
                        let mut notif_rx_open = true;
                        let result: Result<(StopReason, Option<PromptUsage>), AcpError> = loop {
                            tokio::select! {
                                biased;
                                notif = notif_rx.recv(), if notif_rx_open => {
                                    if let Some(n) = notif {
                                        let _ = cx_inner.send_notification(n);
                                    } else {
                                        notif_rx_open = false;
                                    }
                                }
                                perm = bridge_rx.recv() => {
                                    if let Some((perm_req, response_tx)) = perm {
                                        let acp_req = build_acp_permission_request(
                                            sid.clone(),
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
                                        break blocking_handle.await
                                            .unwrap_or(Err(AcpError::internal("blocking task failed")));
                                    }
                                }
                                question = question_rx.recv() => {
                                    if let Some((tool_call_id, question_req, response_tx)) = question {
                                        let payload = AcpAskUserQuestionRequestPayload {
                                            session_id: sid.clone(),
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

                        // Flush the rest of the turn's notifications before the
                        // response, which the protocol requires: a client renders
                        // `session/update`s as they arrive and finalises on the
                        // `session/prompt` response, so an update that lands after
                        // it is an update the client has already stopped listening
                        // for.
                        //
                        // Draining with `try_recv` did not guarantee that. The turn
                        // hands events to the forwarder thread, which maps them onto
                        // the wire; when the turn returned with the forwarder still
                        // mid-map, `try_recv` saw an empty channel and the response
                        // overtook them. A slash command — one `TextDelta` sent
                        // immediately before returning — lost that race routinely on
                        // Windows over WebSocket, and the client saw a bare
                        // `end_turn` with no text at all.
                        //
                        // The forwarder owns the only sender, so the channel closes
                        // exactly when it has finished; recv until then and the
                        // ordering is guaranteed rather than raced.
                        while let Some(n) = notif_rx.recv().await {
                            let _ = cx_inner.send_notification(n);
                        }

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
                                responder.respond(response)?;
                            }
                            Err(error) => {
                                let user_message = error.user_friendly_message();
                                let error_notification = SessionNotification::new(
                                    sid.clone(),
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(&user_message)),
                                    )),
                                );
                                let _ = cx_inner.send_notification(error_notification);
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
                        tokio::task::spawn_blocking(move || {
                            if let Some((engine, started_at)) = taken {
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
                                engine.close();
                            }
                        })
                        .await
                        .ok();
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
                    let cx_notify = cx.clone();
                    let sid = req.session_id.to_string();
                    let cwd = req.cwd;
                    cx.spawn(async move {
                        let _lane = registry.enter_lane(&sid).await;
                        let lease = registry.cwd_lease();
                        let result = tokio::task::spawn_blocking(move || {
                            let lease_cwd = session_lease_cwd(&cwd);
                            let _cwd = lease.acquire(&lease_cwd).map_err(|e| {
                                AcpError::internal(format!("failed to enter cwd: {e}"))
                            })?;
                            let _scope = WorkspaceRootScope::enter(lease_cwd);
                            let mcp_servers = acp_mcp_servers_to_scoped(&req.mcp_servers, &cwd);
                            let (engine, cwd) = session_ops::open_loaded_session(
                                &config,
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
                                registry.register(session_id.clone(), engine, cwd);
                                responder.respond(LoadSessionResponse::new())?;
                                let _ = cx_notify.send_notification(
                                    available_commands_notification(&session_id, commands),
                                );
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
        // --- catch-all for unhandled methods ---
        .on_receive_dispatch(
            async move |dispatch: Dispatch, cx: ConnectionTo<Client>| {
                match &dispatch {
                    Dispatch::Request(_, _) | Dispatch::Notification(_) => {
                        dispatch.respond_with_error(Error::method_not_found(), cx)?;
                        Ok(Handled::Yes)
                    }
                    Dispatch::Response(_, _) => Ok(Handled::No {
                        message: dispatch,
                        retry: false,
                    }),
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
