//! MCP transport abstraction layer.
//!
//! Defines the `McpTransportProcess` trait that all transport types must implement,
//! and provides a factory function `connect_mcp_process` that dispatches to the
//! appropriate transport implementation based on the bootstrap configuration.

use std::fmt;

use crate::mcp_client::{McpClientBootstrap, McpClientTransport};

// Re-export types used in trait signatures from mcp_stdio.
pub use crate::mcp_stdio::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
    McpListResourcesParams, McpListResourcesResult, McpListToolsParams, McpListToolsResult,
    McpReadResourceParams, McpReadResourceResult, McpToolCallParams, McpToolCallResult,
};

/// Unified RPC contract for all MCP transport types.
///
/// Each transport (Stdio, SSE, HTTP, WebSocket, ManagedProxy) implements this trait.
/// The `McpServerManager` interacts with transports exclusively through this interface,
/// making it transport-agnostic.
#[async_trait::async_trait]
pub trait McpTransportProcess: Send + fmt::Debug {
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> std::io::Result<JsonRpcResponse<McpInitializeResult>>;

    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> std::io::Result<JsonRpcResponse<McpListToolsResult>>;

    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> std::io::Result<JsonRpcResponse<McpToolCallResult>>;

    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> std::io::Result<JsonRpcResponse<McpListResourcesResult>>;

    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> std::io::Result<JsonRpcResponse<McpReadResourceResult>>;

    fn has_exited(&mut self) -> std::io::Result<bool>;

    async fn shutdown(&mut self) -> std::io::Result<()>;

    fn resolved_tool_call_timeout_ms(&self) -> u64;
}

/// Error type for transport connection failures.
#[derive(Debug)]
pub enum McpTransportConnectError {
    /// The transport type is not supported (e.g., XAA not implemented).
    Unsupported { reason: String },
    /// I/O error during connection.
    Io(std::io::Error),
    /// OAuth is required but cannot be completed interactively.
    OAuthRequired { server_name: String },
    /// Generic transport error.
    Transport {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl fmt::Display for McpTransportConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported { reason } => write!(f, "unsupported transport: {reason}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::OAuthRequired { server_name } => {
                write!(f, "OAuth required for server '{server_name}' but cannot be completed interactively")
            }
            Self::Transport { source } => write!(f, "transport error: {source}"),
        }
    }
}

impl std::error::Error for McpTransportConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Transport { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<std::io::Error> for McpTransportConnectError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Factory function: connect to an MCP server using the appropriate transport.
///
/// Dispatches to the transport-specific `connect()` function based on
/// `bootstrap.transport` variant. The caller is responsible for ensuring
/// XAA servers are filtered out before calling this function (done in
/// `McpServerManager::from_servers`).
pub async fn connect_mcp_process(
    bootstrap: &McpClientBootstrap,
) -> Result<Box<dyn McpTransportProcess + Send>, McpTransportConnectError> {
    match &bootstrap.transport {
        McpClientTransport::Stdio(transport) => {
            let process = crate::mcp_stdio::McpStdioProcess::spawn(transport)?;
            Ok(Box::new(process))
        }
        McpClientTransport::Sse(transport) => {
            let process = crate::mcp_sse::McpSseProcess::connect(
                &bootstrap.server_name,
                transport,
                bootstrap.scope,
                bootstrap.workspace_is_trusted,
            )
            .await?;
            Ok(Box::new(process))
        }
        McpClientTransport::Http(transport) => {
            let process = crate::mcp_http::McpHttpProcess::connect(
                &bootstrap.server_name,
                transport,
                bootstrap.scope,
                bootstrap.workspace_is_trusted,
            )
            .await?;
            Ok(Box::new(process))
        }
        McpClientTransport::WebSocket(transport) => {
            let process = crate::mcp_websocket::McpWebSocketProcess::connect(
                &bootstrap.server_name,
                transport,
                bootstrap.scope,
                bootstrap.workspace_is_trusted,
            )
            .await?;
            Ok(Box::new(process))
        }
        McpClientTransport::ManagedProxy(transport) => {
            let process = crate::mcp_managed_proxy::McpManagedProxyProcess::connect(
                &bootstrap.server_name,
                transport,
            )
            .await?;
            Ok(Box::new(process))
        }
        McpClientTransport::Sdk(_) => Err(McpTransportConnectError::Unsupported {
            reason: "SDK transport is not supported".to_string(),
        }),
    }
}
