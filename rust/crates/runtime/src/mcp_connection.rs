//! Transport-agnostic connection abstraction shared by the stdio and the
//! remote (SSE) MCP transports, so [`McpServerManager`](crate::McpServerManager)
//! can drive either one through a single `Box<dyn McpConnection>`.
//!
//! The trait deliberately mirrors the inherent methods of `McpStdioProcess`
//! (the first implementation), so the stdio path delegates with no behavioral
//! change. Per-transport timeout configuration is intentionally excluded:
//! `McpStdioProcess` does not carry a timeout, so the manager resolves the
//! `tools/call` timeout from the bootstrap transport instead (see
//! `McpServerManager::tool_call_timeout_ms`).

use std::io;

use async_trait::async_trait;

use crate::mcp_server_manager::{
    JsonRpcId, JsonRpcResponse, McpInitializeParams, McpInitializeResult, McpListResourcesParams,
    McpListResourcesResult, McpListToolsParams, McpListToolsResult, McpReadResourceParams,
    McpReadResourceResult, McpToolCallParams, McpToolCallResult,
};

/// A live MCP transport connection over which JSON-RPC requests can be driven.
#[async_trait]
pub trait McpConnection: Send + std::fmt::Debug {
    /// Send `initialize` and read the handshake response.
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>>;

    /// Send `tools/list` (optionally paged via `cursor`) and read the response.
    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>>;

    /// Send `tools/call` and read the response.
    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>>;

    /// Send `resources/list` (optionally paged) and read the response.
    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> io::Result<JsonRpcResponse<McpListResourcesResult>>;

    /// Send `resources/read` and read the response.
    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> io::Result<JsonRpcResponse<McpReadResourceResult>>;

    /// Whether the underlying transport has already terminated and must be
    /// re-established before the next request.
    async fn has_exited(&mut self) -> io::Result<bool>;

    /// Tear down the transport. Best-effort: callers absorb errors, so this
    /// returns unit rather than `io::Result`.
    async fn shutdown(&mut self);
}
