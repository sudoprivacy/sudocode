//! Transport-agnostic MCP client manager.
//!
//! Extracted verbatim from `mcp_stdio.rs` (structural refactor, no logic
//! change): the JSON-RPC message shapes, the `McpServerManager` that drives
//! `initialize` / `tools/list` / `tools/call` / `resources/*`, the error
//! type, and the spawn-attempt / reset retry policy. Transport-specific
//! connection types (`McpStdioProcess`, the future `McpSseConnection`) live
//! in their own modules and are attached through `McpStdioProcess` directly
//! today (to be generalized via a connection trait in a follow-up).

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tokio::time::timeout;

use crate::config::{McpTransport, RuntimeConfig, ScopedMcpServerConfig};
use crate::mcp::mcp_tool_name;
use crate::mcp_client::{McpClientBootstrap, McpClientTransport, DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS};
use crate::mcp_connection::McpConnection;
use crate::mcp_http::McpHttpConnection;
use crate::mcp_lifecycle_hardened::{
    McpDegradedReport, McpErrorSurface, McpFailedServer, McpLifecyclePhase,
};
use crate::mcp_sse::McpSseConnection;
use crate::mcp_stdio::spawn_mcp_stdio_process;

// Test timeouts must still comfortably cover spawning a fresh Python child and
// completing the JSON-RPC handshake on a loaded CI runner (macOS is the slowest).
// They were originally 200/300 ms, which flaked when the *second* (legitimate)
// spawn in the retry tests raced the timeout. They only bound how long a
// deliberately-hung child is waited on, so 2 s keeps tests fast while removing
// the race. Production values are unchanged.
#[cfg(test)]
const MCP_INITIALIZE_TIMEOUT_MS: u64 = 2_000;
#[cfg(not(test))]
const MCP_INITIALIZE_TIMEOUT_MS: u64 = 10_000;

#[cfg(test)]
const MCP_LIST_TOOLS_TIMEOUT_MS: u64 = 2_000;
#[cfg(not(test))]
const MCP_LIST_TOOLS_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(u64),
    String(String),
    Null,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcRequest<T = JsonValue> {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<T>,
}

impl<T> JsonRpcRequest<T> {
    #[must_use]
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<T>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse<T = JsonValue> {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeParams {
    pub protocol_version: String,
    pub capabilities: JsonValue,
    pub client_info: McpInitializeClientInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeResult {
    pub protocol_version: String,
    pub capabilities: JsonValue,
    pub server_info: McpInitializeServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpListToolsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<JsonValue>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpListToolsResult {
    pub tools: Vec<McpTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonValue>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolCallContent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub data: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpToolCallResult {
    #[serde(default)]
    pub content: Vec<McpToolCallContent>,
    #[serde(default)]
    pub structured_content: Option<JsonValue>,
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpListResourcesParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpResource {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<JsonValue>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpListResourcesResult {
    pub resources: Vec<McpResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpReadResourceParams {
    pub uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpResourceContents {
    pub uri: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpReadResourceResult {
    pub contents: Vec<McpResourceContents>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManagedMcpTool {
    pub server_name: String,
    pub qualified_name: String,
    pub raw_name: String,
    pub tool: McpTool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedMcpServer {
    pub server_name: String,
    pub transport: McpTransport,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDiscoveryFailure {
    pub server_name: String,
    pub phase: McpLifecyclePhase,
    pub error: String,
    pub recoverable: bool,
    pub context: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDiscoveryReport {
    pub tools: Vec<ManagedMcpTool>,
    pub failed_servers: Vec<McpDiscoveryFailure>,
    pub unsupported_servers: Vec<UnsupportedMcpServer>,
    pub degraded_startup: Option<McpDegradedReport>,
}

#[derive(Debug)]
pub enum McpServerManagerError {
    Io(io::Error),
    Transport {
        server_name: String,
        method: &'static str,
        source: io::Error,
    },
    JsonRpc {
        server_name: String,
        method: &'static str,
        error: JsonRpcError,
    },
    InvalidResponse {
        server_name: String,
        method: &'static str,
        details: String,
    },
    Timeout {
        server_name: String,
        method: &'static str,
        timeout_ms: u64,
    },
    UnknownTool {
        qualified_name: String,
    },
    UnknownServer {
        server_name: String,
    },
    /// Sticky terminal failure after exceeding the spawn-attempt limit.
    /// Prevents a broken plugin MCP server from being re-forked indefinitely.
    PermanentlyFailed {
        server_name: String,
        reason: String,
    },
}

impl std::fmt::Display for McpServerManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Transport {
                server_name,
                method,
                source,
            } => write!(
                f,
                "MCP server `{server_name}` transport failed during {method}: {source}"
            ),
            Self::JsonRpc {
                server_name,
                method,
                error,
            } => write!(
                f,
                "MCP server `{server_name}` returned JSON-RPC error for {method}: {} ({})",
                error.message, error.code
            ),
            Self::InvalidResponse {
                server_name,
                method,
                details,
            } => write!(
                f,
                "MCP server `{server_name}` returned invalid response for {method}: {details}"
            ),
            Self::Timeout {
                server_name,
                method,
                timeout_ms,
            } => write!(
                f,
                "MCP server `{server_name}` timed out after {timeout_ms} ms while handling {method}"
            ),
            Self::UnknownTool { qualified_name } => {
                write!(f, "unknown MCP tool `{qualified_name}`")
            }
            Self::UnknownServer { server_name } => write!(f, "unknown MCP server `{server_name}`"),
            Self::PermanentlyFailed {
                server_name,
                reason,
            } => write!(
                f,
                "MCP server `{server_name}` permanently disabled: {reason}"
            ),
        }
    }
}

impl std::error::Error for McpServerManagerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Transport { source, .. } => Some(source),
            Self::JsonRpc { .. }
            | Self::InvalidResponse { .. }
            | Self::Timeout { .. }
            | Self::UnknownTool { .. }
            | Self::UnknownServer { .. }
            | Self::PermanentlyFailed { .. } => None,
        }
    }
}

impl From<io::Error> for McpServerManagerError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl McpServerManagerError {
    fn lifecycle_phase(&self) -> McpLifecyclePhase {
        match self {
            Self::Io(_) => McpLifecyclePhase::SpawnConnect,
            Self::Transport { method, .. }
            | Self::JsonRpc { method, .. }
            | Self::InvalidResponse { method, .. }
            | Self::Timeout { method, .. } => lifecycle_phase_for_method(method),
            Self::UnknownTool { .. } => McpLifecyclePhase::ToolDiscovery,
            Self::UnknownServer { .. } => McpLifecyclePhase::ServerRegistration,
            // Permanent failure is the sticky version of "couldn't make it
            // past initialize" — preserve InitializeHandshake so downstream
            // surfaces (degraded report, doctor) don't misclassify it.
            Self::PermanentlyFailed { .. } => McpLifecyclePhase::InitializeHandshake,
        }
    }

    fn recoverable(&self) -> bool {
        !matches!(
            self.lifecycle_phase(),
            McpLifecyclePhase::InitializeHandshake
        ) && matches!(self, Self::Transport { .. } | Self::Timeout { .. })
    }

    fn discovery_failure(&self, server_name: &str) -> McpDiscoveryFailure {
        let phase = self.lifecycle_phase();
        let recoverable = self.recoverable();
        let context = self.error_context();

        McpDiscoveryFailure {
            server_name: server_name.to_string(),
            phase,
            error: self.to_string(),
            recoverable,
            context,
        }
    }

    fn error_context(&self) -> BTreeMap<String, String> {
        match self {
            Self::Io(error) => BTreeMap::from([("kind".to_string(), error.kind().to_string())]),
            Self::Transport {
                server_name,
                method,
                source,
            } => BTreeMap::from([
                ("server".to_string(), server_name.clone()),
                ("method".to_string(), (*method).to_string()),
                ("io_kind".to_string(), source.kind().to_string()),
            ]),
            Self::JsonRpc {
                server_name,
                method,
                error,
            } => BTreeMap::from([
                ("server".to_string(), server_name.clone()),
                ("method".to_string(), (*method).to_string()),
                ("jsonrpc_code".to_string(), error.code.to_string()),
            ]),
            Self::InvalidResponse {
                server_name,
                method,
                details,
            } => BTreeMap::from([
                ("server".to_string(), server_name.clone()),
                ("method".to_string(), (*method).to_string()),
                ("details".to_string(), details.clone()),
            ]),
            Self::Timeout {
                server_name,
                method,
                timeout_ms,
            } => BTreeMap::from([
                ("server".to_string(), server_name.clone()),
                ("method".to_string(), (*method).to_string()),
                ("timeout_ms".to_string(), timeout_ms.to_string()),
            ]),
            Self::UnknownTool { qualified_name } => {
                BTreeMap::from([("qualified_tool".to_string(), qualified_name.clone())])
            }
            Self::UnknownServer { server_name } => {
                BTreeMap::from([("server".to_string(), server_name.clone())])
            }
            Self::PermanentlyFailed {
                server_name,
                reason,
            } => BTreeMap::from([
                ("server".to_string(), server_name.clone()),
                ("reason".to_string(), reason.clone()),
                // Track the lifecycle method this failure aborted, matching
                // other variants. PermanentlyFailed always trips in the
                // spawn → initialize handshake window.
                ("method".to_string(), "initialize".to_string()),
            ]),
        }
    }
}

fn lifecycle_phase_for_method(method: &str) -> McpLifecyclePhase {
    match method {
        "initialize" => McpLifecyclePhase::InitializeHandshake,
        "tools/list" => McpLifecyclePhase::ToolDiscovery,
        "resources/list" => McpLifecyclePhase::ResourceDiscovery,
        "resources/read" | "tools/call" => McpLifecyclePhase::Invocation,
        _ => McpLifecyclePhase::ErrorSurfacing,
    }
}

pub(crate) fn unsupported_server_failed_server(server: &UnsupportedMcpServer) -> McpFailedServer {
    McpFailedServer {
        server_name: server.server_name.clone(),
        phase: McpLifecyclePhase::ServerRegistration,
        error: McpErrorSurface::new(
            McpLifecyclePhase::ServerRegistration,
            Some(server.server_name.clone()),
            server.reason.clone(),
            BTreeMap::from([("transport".to_string(), format!("{:?}", server.transport))]),
            false,
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolRoute {
    server_name: String,
    raw_name: String,
}

/// Maximum number of spawn+initialize attempts to make against a single MCP
/// server within one McpServerManager lifetime. Exceeding this trips a sticky
/// `permanent_failure` so a broken plugin MCP server cannot loop-fork.
pub(crate) const MCP_SPAWN_ATTEMPT_LIMIT: u32 = 2;

#[derive(Debug)]
struct ManagedMcpServer {
    bootstrap: McpClientBootstrap,
    process: Option<Box<dyn McpConnection>>,
    initialized: bool,
    /// Total spawn attempts (including retries) made against this server.
    /// Capped at MCP_SPAWN_ATTEMPT_LIMIT to short-circuit the spawn loop.
    spawn_attempts: u32,
    /// Sticky terminal failure that disables further spawn attempts and is
    /// returned verbatim on every future request. Set once spawn_attempts
    /// reaches the cap.
    permanent_failure: Option<String>,
}

impl ManagedMcpServer {
    fn new(bootstrap: McpClientBootstrap) -> Self {
        Self {
            bootstrap,
            process: None,
            initialized: false,
            spawn_attempts: 0,
            permanent_failure: None,
        }
    }
}

#[derive(Debug)]
pub struct McpServerManager {
    servers: BTreeMap<String, ManagedMcpServer>,
    unsupported_servers: Vec<UnsupportedMcpServer>,
    tool_index: BTreeMap<String, ToolRoute>,
    next_request_id: u64,
}

impl McpServerManager {
    #[must_use]
    pub fn from_runtime_config(config: &RuntimeConfig) -> Self {
        Self::from_servers(config.mcp().servers())
    }

    #[must_use]
    pub fn from_servers(servers: &BTreeMap<String, ScopedMcpServerConfig>) -> Self {
        let mut managed_servers = BTreeMap::new();
        let mut unsupported_servers = Vec::new();

        for (server_name, server_config) in servers {
            let transport = server_config.transport();
            if matches!(
                transport,
                McpTransport::Stdio | McpTransport::Sse | McpTransport::Http
            ) {
                let bootstrap = McpClientBootstrap::from_scoped_config(server_name, server_config);
                managed_servers.insert(server_name.clone(), ManagedMcpServer::new(bootstrap));
            } else {
                unsupported_servers.push(UnsupportedMcpServer {
                    server_name: server_name.clone(),
                    transport,
                    reason: format!("transport {transport:?} is not supported by McpServerManager"),
                });
            }
        }

        Self {
            servers: managed_servers,
            unsupported_servers,
            tool_index: BTreeMap::new(),
            next_request_id: 1,
        }
    }

    #[must_use]
    pub fn unsupported_servers(&self) -> &[UnsupportedMcpServer] {
        &self.unsupported_servers
    }

    #[must_use]
    pub fn server_names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// All registered tools as `(qualified_name, server_name)` pairs, where
    /// `server_name` is the original (pre-normalization) server name. Lets
    /// callers attribute a tool to its server exactly, without reverse-
    /// engineering the normalized `mcp__<server>__` prefix (which collides for
    /// names like `foo.bar` vs `foo_bar`).
    #[must_use]
    pub fn tools_with_server(&self) -> Vec<(String, String)> {
        self.tool_index
            .iter()
            .map(|(qualified, route)| (qualified.clone(), route.server_name.clone()))
            .collect()
    }

    pub async fn discover_tools(&mut self) -> Result<Vec<ManagedMcpTool>, McpServerManagerError> {
        let server_names = self.servers.keys().cloned().collect::<Vec<_>>();
        let mut discovered_tools = Vec::new();

        for server_name in server_names {
            let server_tools = self.discover_tools_for_server(&server_name).await?;
            self.clear_routes_for_server(&server_name);

            for tool in server_tools {
                self.tool_index.insert(
                    tool.qualified_name.clone(),
                    ToolRoute {
                        server_name: tool.server_name.clone(),
                        raw_name: tool.raw_name.clone(),
                    },
                );
                discovered_tools.push(tool);
            }
        }

        Ok(discovered_tools)
    }

    pub async fn discover_tools_best_effort(&mut self) -> McpToolDiscoveryReport {
        let server_names = self.server_names();
        let mut discovered_tools = Vec::new();
        let mut working_servers = Vec::new();
        let mut failed_servers = Vec::new();

        for server_name in server_names {
            match self.discover_tools_for_server(&server_name).await {
                Ok(server_tools) => {
                    working_servers.push(server_name.clone());
                    self.clear_routes_for_server(&server_name);
                    for tool in server_tools {
                        self.tool_index.insert(
                            tool.qualified_name.clone(),
                            ToolRoute {
                                server_name: tool.server_name.clone(),
                                raw_name: tool.raw_name.clone(),
                            },
                        );
                        discovered_tools.push(tool);
                    }
                }
                Err(error) => {
                    self.clear_routes_for_server(&server_name);
                    failed_servers.push(error.discovery_failure(&server_name));
                }
            }
        }

        let degraded_failed_servers = failed_servers
            .iter()
            .map(|failure| McpFailedServer {
                server_name: failure.server_name.clone(),
                phase: failure.phase,
                error: McpErrorSurface::new(
                    failure.phase,
                    Some(failure.server_name.clone()),
                    failure.error.clone(),
                    failure.context.clone(),
                    failure.recoverable,
                ),
            })
            .chain(
                self.unsupported_servers
                    .iter()
                    .map(unsupported_server_failed_server),
            )
            .collect::<Vec<_>>();
        let degraded_startup = (!working_servers.is_empty() && !degraded_failed_servers.is_empty())
            .then(|| {
                McpDegradedReport::new(
                    working_servers,
                    degraded_failed_servers,
                    discovered_tools
                        .iter()
                        .map(|tool| tool.qualified_name.clone())
                        .collect(),
                    Vec::new(),
                )
            });

        McpToolDiscoveryReport {
            tools: discovered_tools,
            failed_servers,
            unsupported_servers: self.unsupported_servers.clone(),
            degraded_startup,
        }
    }

    pub async fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<JsonValue>,
    ) -> Result<JsonRpcResponse<McpToolCallResult>, McpServerManagerError> {
        let route = self
            .tool_index
            .get(qualified_tool_name)
            .cloned()
            .ok_or_else(|| McpServerManagerError::UnknownTool {
                qualified_name: qualified_tool_name.to_string(),
            })?;

        let timeout_ms = self.tool_call_timeout_ms(&route.server_name)?;

        self.ensure_server_ready(&route.server_name).await?;
        let request_id = self.take_request_id();
        let response =
            {
                let server = self.server_mut(&route.server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: route.server_name.clone(),
                        method: "tools/call",
                        details: "server process missing after initialization".to_string(),
                    }
                })?;
                Self::run_process_request(
                    &route.server_name,
                    "tools/call",
                    timeout_ms,
                    process.call_tool(
                        request_id,
                        McpToolCallParams {
                            name: route.raw_name,
                            arguments,
                            meta: None,
                        },
                    ),
                )
                .await
            };

        if let Err(error) = &response {
            if Self::should_reset_server(error) {
                self.reset_server(&route.server_name).await?;
            }
        }

        response
    }

    pub async fn list_resources(
        &mut self,
        server_name: &str,
    ) -> Result<McpListResourcesResult, McpServerManagerError> {
        let mut attempts = 0;

        loop {
            match self.list_resources_once(server_name).await {
                Ok(resources) => return Ok(resources),
                Err(error) if attempts == 0 && Self::is_retryable_error(&error) => {
                    self.reset_server(server_name).await?;
                    attempts += 1;
                }
                Err(error) => {
                    if Self::should_reset_server(&error) {
                        self.reset_server(server_name).await?;
                    }
                    return Err(error);
                }
            }
        }
    }

    pub async fn read_resource(
        &mut self,
        server_name: &str,
        uri: &str,
    ) -> Result<McpReadResourceResult, McpServerManagerError> {
        let mut attempts = 0;

        loop {
            match self.read_resource_once(server_name, uri).await {
                Ok(resource) => return Ok(resource),
                Err(error) if attempts == 0 && Self::is_retryable_error(&error) => {
                    self.reset_server(server_name).await?;
                    attempts += 1;
                }
                Err(error) => {
                    if Self::should_reset_server(&error) {
                        self.reset_server(server_name).await?;
                    }
                    return Err(error);
                }
            }
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), McpServerManagerError> {
        let server_names = self.servers.keys().cloned().collect::<Vec<_>>();
        for server_name in server_names {
            let server = self.server_mut(&server_name)?;
            if let Some(process) = server.process.as_mut() {
                process.shutdown().await;
            }
            server.process = None;
            server.initialized = false;
        }
        Ok(())
    }

    fn clear_routes_for_server(&mut self, server_name: &str) {
        self.tool_index
            .retain(|_, route| route.server_name != server_name);
    }

    fn server_mut(
        &mut self,
        server_name: &str,
    ) -> Result<&mut ManagedMcpServer, McpServerManagerError> {
        self.servers
            .get_mut(server_name)
            .ok_or_else(|| McpServerManagerError::UnknownServer {
                server_name: server_name.to_string(),
            })
    }

    fn take_request_id(&mut self) -> JsonRpcId {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        JsonRpcId::Number(id)
    }

    fn tool_call_timeout_ms(&self, server_name: &str) -> Result<u64, McpServerManagerError> {
        let server =
            self.servers
                .get(server_name)
                .ok_or_else(|| McpServerManagerError::UnknownServer {
                    server_name: server_name.to_string(),
                })?;
        match &server.bootstrap.transport {
            McpClientTransport::Stdio(transport) => Ok(transport.resolved_tool_call_timeout_ms()),
            // Remote transports (SSE today; HTTP/WS/SDK remain unimplemented)
            // are not configurable per-server yet — fall back to the default.
            _ => Ok(DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS),
        }
    }

    pub(crate) async fn server_process_exited(
        &mut self,
        server_name: &str,
    ) -> Result<bool, McpServerManagerError> {
        let server = self.server_mut(server_name)?;
        match server.process.as_mut() {
            Some(process) => Ok(process.has_exited().await?),
            None => Ok(false),
        }
    }

    async fn discover_tools_for_server(
        &mut self,
        server_name: &str,
    ) -> Result<Vec<ManagedMcpTool>, McpServerManagerError> {
        let mut attempts = 0;

        loop {
            match self.discover_tools_for_server_once(server_name).await {
                Ok(tools) => return Ok(tools),
                Err(error) if attempts == 0 && Self::is_retryable_error(&error) => {
                    self.reset_server(server_name).await?;
                    attempts += 1;
                }
                Err(error) => {
                    if Self::should_reset_server(&error) {
                        self.reset_server(server_name).await?;
                    }
                    return Err(error);
                }
            }
        }
    }

    async fn discover_tools_for_server_once(
        &mut self,
        server_name: &str,
    ) -> Result<Vec<ManagedMcpTool>, McpServerManagerError> {
        self.ensure_server_ready(server_name).await?;

        let mut discovered_tools = Vec::new();
        let mut cursor = None;
        loop {
            let request_id = self.take_request_id();
            let response = {
                let server = self.server_mut(server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: server_name.to_string(),
                        method: "tools/list",
                        details: "server process missing after initialization".to_string(),
                    }
                })?;
                Self::run_process_request(
                    server_name,
                    "tools/list",
                    MCP_LIST_TOOLS_TIMEOUT_MS,
                    process.list_tools(
                        request_id,
                        Some(McpListToolsParams {
                            cursor: cursor.clone(),
                        }),
                    ),
                )
                .await?
            };

            if let Some(error) = response.error {
                return Err(McpServerManagerError::JsonRpc {
                    server_name: server_name.to_string(),
                    method: "tools/list",
                    error,
                });
            }

            let result = response
                .result
                .ok_or_else(|| McpServerManagerError::InvalidResponse {
                    server_name: server_name.to_string(),
                    method: "tools/list",
                    details: "missing result payload".to_string(),
                })?;

            for tool in result.tools {
                let qualified_name = mcp_tool_name(server_name, &tool.name);
                discovered_tools.push(ManagedMcpTool {
                    server_name: server_name.to_string(),
                    qualified_name,
                    raw_name: tool.name.clone(),
                    tool,
                });
            }

            match result.next_cursor {
                Some(next_cursor) => cursor = Some(next_cursor),
                None => break,
            }
        }

        Ok(discovered_tools)
    }

    async fn list_resources_once(
        &mut self,
        server_name: &str,
    ) -> Result<McpListResourcesResult, McpServerManagerError> {
        self.ensure_server_ready(server_name).await?;

        let mut resources = Vec::new();
        let mut cursor = None;
        loop {
            let request_id = self.take_request_id();
            let response = {
                let server = self.server_mut(server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: server_name.to_string(),
                        method: "resources/list",
                        details: "server process missing after initialization".to_string(),
                    }
                })?;
                Self::run_process_request(
                    server_name,
                    "resources/list",
                    MCP_LIST_TOOLS_TIMEOUT_MS,
                    process.list_resources(
                        request_id,
                        Some(McpListResourcesParams {
                            cursor: cursor.clone(),
                        }),
                    ),
                )
                .await?
            };

            if let Some(error) = response.error {
                return Err(McpServerManagerError::JsonRpc {
                    server_name: server_name.to_string(),
                    method: "resources/list",
                    error,
                });
            }

            let result = response
                .result
                .ok_or_else(|| McpServerManagerError::InvalidResponse {
                    server_name: server_name.to_string(),
                    method: "resources/list",
                    details: "missing result payload".to_string(),
                })?;

            resources.extend(result.resources);

            match result.next_cursor {
                Some(next_cursor) => cursor = Some(next_cursor),
                None => break,
            }
        }

        Ok(McpListResourcesResult {
            resources,
            next_cursor: None,
        })
    }

    async fn read_resource_once(
        &mut self,
        server_name: &str,
        uri: &str,
    ) -> Result<McpReadResourceResult, McpServerManagerError> {
        self.ensure_server_ready(server_name).await?;

        let request_id = self.take_request_id();
        let response =
            {
                let server = self.server_mut(server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: server_name.to_string(),
                        method: "resources/read",
                        details: "server process missing after initialization".to_string(),
                    }
                })?;
                Self::run_process_request(
                    server_name,
                    "resources/read",
                    MCP_LIST_TOOLS_TIMEOUT_MS,
                    process.read_resource(
                        request_id,
                        McpReadResourceParams {
                            uri: uri.to_string(),
                        },
                    ),
                )
                .await?
            };

        if let Some(error) = response.error {
            return Err(McpServerManagerError::JsonRpc {
                server_name: server_name.to_string(),
                method: "resources/read",
                error,
            });
        }

        response
            .result
            .ok_or_else(|| McpServerManagerError::InvalidResponse {
                server_name: server_name.to_string(),
                method: "resources/read",
                details: "missing result payload".to_string(),
            })
    }

    async fn reset_server(&mut self, server_name: &str) -> Result<(), McpServerManagerError> {
        let mut process = {
            let server = self.server_mut(server_name)?;
            server.initialized = false;
            server.process.take()
        };

        if let Some(process) = process.as_mut() {
            let _ = process.shutdown().await;
        }

        Ok(())
    }

    fn is_retryable_error(error: &McpServerManagerError) -> bool {
        matches!(
            error,
            McpServerManagerError::Transport { .. } | McpServerManagerError::Timeout { .. }
        )
    }

    fn should_reset_server(error: &McpServerManagerError) -> bool {
        matches!(
            error,
            McpServerManagerError::Transport { .. }
                | McpServerManagerError::Timeout { .. }
                | McpServerManagerError::InvalidResponse { .. }
        )
    }

    async fn run_process_request<T, F>(
        server_name: &str,
        method: &'static str,
        timeout_ms: u64,
        future: F,
    ) -> Result<T, McpServerManagerError>
    where
        F: Future<Output = io::Result<T>>,
    {
        match timeout(Duration::from_millis(timeout_ms), future).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                Err(McpServerManagerError::InvalidResponse {
                    server_name: server_name.to_string(),
                    method,
                    details: error.to_string(),
                })
            }
            Ok(Err(source)) => Err(McpServerManagerError::Transport {
                server_name: server_name.to_string(),
                method,
                source,
            }),
            Err(_) => Err(McpServerManagerError::Timeout {
                server_name: server_name.to_string(),
                method,
                timeout_ms,
            }),
        }
    }

    async fn ensure_server_ready(
        &mut self,
        server_name: &str,
    ) -> Result<(), McpServerManagerError> {
        // Sticky terminal failure short-circuits before any work — prevents
        // the spawn loop from repeatedly fork()ing a broken plugin server.
        if let Some(reason) = self
            .servers
            .get(server_name)
            .and_then(|server| server.permanent_failure.clone())
        {
            return Err(McpServerManagerError::PermanentlyFailed {
                server_name: server_name.to_string(),
                reason,
            });
        }

        if self.server_process_exited(server_name).await? {
            self.reset_server(server_name).await?;
        }

        let mut attempts = 0;
        loop {
            let needs_spawn = self
                .servers
                .get(server_name)
                .map(|server| server.process.is_none())
                .ok_or_else(|| McpServerManagerError::UnknownServer {
                    server_name: server_name.to_string(),
                })?;

            if needs_spawn {
                let attempt_count = self
                    .servers
                    .get(server_name)
                    .map(|server| server.spawn_attempts)
                    .unwrap_or(0);
                if attempt_count >= MCP_SPAWN_ATTEMPT_LIMIT {
                    let reason = format!(
                        "MCP server `{server_name}` exceeded {MCP_SPAWN_ATTEMPT_LIMIT} initialize attempts; refusing to retry spawn"
                    );
                    if let Some(server) = self.servers.get_mut(server_name) {
                        server.permanent_failure = Some(reason.clone());
                    }
                    return Err(McpServerManagerError::PermanentlyFailed {
                        server_name: server_name.to_string(),
                        reason,
                    });
                }
                let bootstrap = {
                    let server = self.server_mut(server_name)?;
                    server.spawn_attempts = server.spawn_attempts.saturating_add(1);
                    server.bootstrap.clone()
                };
                let process = match timeout(
                    Duration::from_millis(MCP_INITIALIZE_TIMEOUT_MS),
                    spawn_mcp_connection(&bootstrap),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        return Err(McpServerManagerError::Io(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "MCP spawn timed out",
                        )));
                    }
                };
                let server = self.server_mut(server_name)?;
                server.process = Some(process);
                server.initialized = false;
            }

            let needs_initialize = self
                .servers
                .get(server_name)
                .map(|server| !server.initialized)
                .ok_or_else(|| McpServerManagerError::UnknownServer {
                    server_name: server_name.to_string(),
                })?;

            if !needs_initialize {
                return Ok(());
            }

            let request_id = self.take_request_id();
            let response = {
                let server = self.server_mut(server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: server_name.to_string(),
                        method: "initialize",
                        details: "server process missing before initialize".to_string(),
                    }
                })?;
                Self::run_process_request(
                    server_name,
                    "initialize",
                    MCP_INITIALIZE_TIMEOUT_MS,
                    process.initialize(request_id, default_initialize_params()),
                )
                .await
            };

            let response = match response {
                Ok(response) => response,
                Err(error) if attempts == 0 && Self::is_retryable_error(&error) => {
                    self.reset_server(server_name).await?;
                    attempts += 1;
                    continue;
                }
                Err(error) => {
                    if Self::should_reset_server(&error) {
                        self.reset_server(server_name).await?;
                    }
                    return Err(error);
                }
            };

            if let Some(error) = response.error {
                return Err(McpServerManagerError::JsonRpc {
                    server_name: server_name.to_string(),
                    method: "initialize",
                    error,
                });
            }

            if response.result.is_none() {
                let error = McpServerManagerError::InvalidResponse {
                    server_name: server_name.to_string(),
                    method: "initialize",
                    details: "missing result payload".to_string(),
                };
                self.reset_server(server_name).await?;
                return Err(error);
            }

            let server = self.server_mut(server_name)?;
            server.initialized = true;
            // A successful initialize proves the server can start — reset
            // the spawn counter so a later transport-drop + respawn does
            // not inherit stale attempts from a prior lifecycle.
            server.spawn_attempts = 0;
            return Ok(());
        }
    }
}

fn default_initialize_params() -> McpInitializeParams {
    McpInitializeParams {
        protocol_version: "2025-03-26".to_string(),
        capabilities: JsonValue::Object(serde_json::Map::new()),
        client_info: McpInitializeClientInfo {
            name: "runtime".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    }
}

/// Spawn (stdio) or connect (SSE) the transport described by `bootstrap` into
/// a boxed [`McpConnection`]. The JSON-RPC `initialize` handshake itself is
/// driven later by `McpServerManager::ensure_server_ready`; this only
/// establishes the transport-level connection (forking the stdio child, or
/// opening the SSE stream and resolving its POST endpoint).
async fn spawn_mcp_connection(
    bootstrap: &McpClientBootstrap,
) -> io::Result<Box<dyn McpConnection>> {
    match &bootstrap.transport {
        McpClientTransport::Stdio(_) => spawn_mcp_stdio_process(bootstrap)
            .map(|process| Box::new(process) as Box<dyn McpConnection>),
        McpClientTransport::Sse(transport) => {
            let connection = McpSseConnection::connect(transport, &bootstrap.server_name).await?;
            Ok(Box::new(connection))
        }
        McpClientTransport::Http(transport) => {
            let connection = McpHttpConnection::connect(transport, &bootstrap.server_name).await?;
            Ok(Box::new(connection))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "MCP bootstrap transport for {} is not supported: {other:?}",
                bootstrap.server_name
            ),
        )),
    }
}
