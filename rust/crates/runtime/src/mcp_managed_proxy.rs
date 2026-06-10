//! MCP ManagedProxy transport.
//!
//! Wraps `McpHttpProcess` (Streamable HTTP) with ManagedProxy-specific behavior:
//! bearer token from `oauth::load_oauth_credentials()`, no custom headers from headersHelper,
//! and 401 retry via re-reading credentials.

use std::fmt;
use std::io;

use crate::mcp_client::{McpClientAuth, McpManagedProxyTransport, McpRemoteTransport};
use crate::mcp_transport::{
    JsonRpcId, JsonRpcResponse, McpInitializeParams, McpInitializeResult, McpListResourcesParams,
    McpListResourcesResult, McpListToolsParams, McpListToolsResult, McpReadResourceParams,
    McpReadResourceResult, McpToolCallParams, McpToolCallResult, McpTransportProcess,
};

/// ManagedProxy transport — delegates all RPC to an inner `McpHttpProcess`.
///
/// The only differences from plain HTTP are in the `connect()` phase:
/// - Bearer token comes from `oauth::load_oauth_credentials()` (sudocode's own token)
/// - No headersHelper execution (ManagedProxy config has no headers_helper field)
/// - 401 triggers a re-read of credentials (not a full OAuth flow)
pub struct McpManagedProxyProcess {
    inner: crate::mcp_http::McpHttpProcess,
}

impl fmt::Debug for McpManagedProxyProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpManagedProxyProcess")
            .field("inner", &self.inner)
            .finish()
    }
}

impl McpManagedProxyProcess {
    pub async fn connect(
        server_name: &str,
        transport: &McpManagedProxyTransport,
    ) -> io::Result<McpManagedProxyProcess> {
        // Load sudocode's own OAuth credentials for the bearer token.
        let credentials = crate::oauth::load_oauth_credentials().map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("ManagedProxy requires auth but credentials are unavailable: {e}"),
            )
        })?;

        let access_token = credentials
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "ManagedProxy requires auth but no OAuth credentials found. Please login first.",
                )
            })?
            .access_token;

        // Build static headers with the bearer token.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );

        // Construct a McpRemoteTransport to pass to McpHttpProcess::connect.
        // ManagedProxy uses Streamable HTTP under the hood.
        let remote_transport = McpRemoteTransport {
            url: transport.url.clone(),
            headers,
            headers_helper: None,      // ManagedProxy has no headers_helper
            auth: McpClientAuth::None, // We already injected the token in headers
        };

        let inner = crate::mcp_http::McpHttpProcess::connect(
            server_name,
            &remote_transport,
            crate::config::ConfigSource::User,
            true,
        )
        .await?;

        Ok(McpManagedProxyProcess { inner })
    }

    /// Reload credentials from keyring/file and update the HTTP client's
    /// Authorization header.
    async fn reload_credentials(&self) -> io::Result<()> {
        let credentials = crate::oauth::load_oauth_credentials().map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to reload ManagedProxy credentials: {e}"),
            )
        })?;

        let token = credentials
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "ManagedProxy credentials no longer available. Please login again.",
                )
            })?
            .access_token;

        self.inner.update_auth_token(&token).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl McpTransportProcess for McpManagedProxyProcess {
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        match self.inner.initialize(id.clone(), params.clone()).await {
            Ok(result) => Ok(result),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                self.reload_credentials().await?;
                self.inner.initialize(id, params).await
            }
            Err(e) => Err(e),
        }
    }

    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>> {
        match self.inner.list_tools(id.clone(), params.clone()).await {
            Ok(result) => Ok(result),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                self.reload_credentials().await?;
                self.inner.list_tools(id, params).await
            }
            Err(e) => Err(e),
        }
    }

    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>> {
        match self.inner.call_tool(id.clone(), params.clone()).await {
            Ok(result) => Ok(result),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                self.reload_credentials().await?;
                self.inner.call_tool(id, params).await
            }
            Err(e) => Err(e),
        }
    }

    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> io::Result<JsonRpcResponse<McpListResourcesResult>> {
        match self.inner.list_resources(id.clone(), params.clone()).await {
            Ok(result) => Ok(result),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                self.reload_credentials().await?;
                self.inner.list_resources(id, params).await
            }
            Err(e) => Err(e),
        }
    }

    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> io::Result<JsonRpcResponse<McpReadResourceResult>> {
        match self.inner.read_resource(id.clone(), params.clone()).await {
            Ok(result) => Ok(result),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                self.reload_credentials().await?;
                self.inner.read_resource(id, params).await
            }
            Err(e) => Err(e),
        }
    }

    fn has_exited(&mut self) -> io::Result<bool> {
        self.inner.has_exited()
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }

    fn resolved_tool_call_timeout_ms(&self) -> u64 {
        self.inner.resolved_tool_call_timeout_ms()
    }
}
