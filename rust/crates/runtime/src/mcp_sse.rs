//! MCP SSE (legacy) transport implementation.
//!
//! Connects to an MCP server using Server-Sent Events for the server-to-client
//! direction and HTTP POST for the client-to-server direction, as specified by
//! the legacy MCP SSE transport protocol.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

use crate::mcp_client::{McpRemoteTransport, DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS};
use crate::mcp_headers_helper;
use crate::mcp_oauth;
use crate::mcp_stdio::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
    McpListResourcesParams, McpListResourcesResult, McpListToolsParams, McpListToolsResult,
    McpReadResourceParams, McpReadResourceResult, McpToolCallParams, McpToolCallResult,
};
use crate::mcp_transport::McpTransportProcess;
use crate::sse::IncrementalSseParser;

/// Maximum time to wait for the SSE stream to deliver the `endpoint` event
/// before reporting a connection failure.
const ENDPOINT_TIMEOUT_SECS: u64 = 30;

pub struct McpSseProcess {
    server_name: String,
    config: McpRemoteTransport,
    http_client: reqwest::Client,
    base_headers: HeaderMap,
    sse_stream_task: Option<tokio::task::JoinHandle<()>>,
    inbound_rx: mpsc::UnboundedReceiver<JsonRpcResponse<JsonValue>>,
    endpoint_url: watch::Receiver<Option<url::Url>>,
    pending: HashMap<JsonRpcId, oneshot::Sender<JsonRpcResponse<JsonValue>>>,
    tool_call_timeout_ms: u64,
}

impl fmt::Debug for McpSseProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpSseProcess")
            .field("server_name", &self.server_name)
            .field("config", &self.config)
            .field("tool_call_timeout_ms", &self.tool_call_timeout_ms)
            .field("pending_count", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl McpSseProcess {
    /// Connect to the SSE endpoint and start receiving events.
    ///
    /// This initiates the SSE long-lived GET request and waits for the
    /// `endpoint` event before returning.
    pub async fn connect(
        server_name: &str,
        config: &McpRemoteTransport,
        scope: crate::config::ConfigSource,
        workspace_is_trusted: bool,
    ) -> io::Result<Self> {
        let base_url: url::Url = config
            .url
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let http_client = reqwest::Client::new();

        // Build request headers from static config + optional headers_helper.
        let mut base_headers = mcp_headers_helper::build_request_headers(
            server_name,
            config,
            scope,
            workspace_is_trusted,
        )
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // Obtain a fresh access token via the full OAuth flow if configured.
        // The same token is used for both base_headers (POST requests) and
        // the SSE GET stream task.
        let auth_token = match mcp_oauth::ensure_access_token(server_name, config).await {
            Ok(token) => token,
            Err(mcp_oauth::McpOAuthError::NeedsInteractiveAuth { .. }) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "MCP server '{server_name}' requires OAuth authorization. \
                         Please run the authorization flow interactively."
                    ),
                ));
            }
            Err(e) => return Err(io::Error::new(io::ErrorKind::Other, e)),
        };

        // Apply OAuth token to base headers if present.
        if let Some(ref token) = auth_token {
            let val = HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            base_headers.insert(AUTHORIZATION, val);
        }

        let (endpoint_tx, endpoint_rx) = watch::channel(None);
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();

        let sse_stream_task = spawn_sse_stream_task(
            server_name.to_string(),
            base_url,
            auth_token,
            base_headers.clone(),
            http_client.clone(),
            endpoint_tx,
            inbound_tx,
        );

        let mut process = Self {
            server_name: server_name.to_string(),
            config: config.clone(),
            http_client,
            base_headers,
            sse_stream_task: Some(sse_stream_task),
            inbound_rx,
            endpoint_url: endpoint_rx,
            pending: HashMap::new(),
            tool_call_timeout_ms: DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS,
        };

        // Wait for the endpoint URL to arrive via the SSE stream.
        process.wait_for_endpoint().await?;

        Ok(process)
    }

    /// Wait until the `endpoint_url` watch channel contains a value, or
    /// report an error if the SSE task finishes without delivering one.
    async fn wait_for_endpoint(&mut self) -> io::Result<()> {
        let deadline = Duration::from_secs(ENDPOINT_TIMEOUT_SECS);
        let result = timeout(deadline, async {
            loop {
                if self.endpoint_url.borrow().is_some() {
                    return Ok(());
                }
                if self.endpoint_url.changed().await.is_err() {
                    // The sender was dropped — the SSE task terminated.
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "SSE stream closed before delivering the endpoint event",
                    ));
                }
            }
        })
        .await;

        match result {
            Ok(inner) => inner,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for SSE endpoint event from server `{}` after {ENDPOINT_TIMEOUT_SECS}s",
                    self.server_name
                ),
            )),
        }
    }

    /// Send a JSON-RPC request via POST to the SSE endpoint URL and wait for
    /// the response to arrive through the SSE stream.
    async fn rpc<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        id: JsonRpcId,
        method: impl Into<String>,
        params: Option<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let method = method.into();
        let request = JsonRpcRequest::new(id.clone(), method.clone(), params);

        let endpoint = self.endpoint_url.borrow().clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "SSE endpoint URL not available",
            )
        })?;

        // Register a oneshot channel so the dispatch loop can deliver the
        // response back to us.
        let (response_tx, mut response_rx) = oneshot::channel();
        self.pending.insert(id.clone(), response_tx);

        // Drain any buffered inbound messages first.
        self.drain_inbound();

        // Post the JSON-RPC request.
        let post_result = self.post_jsonrpc(&endpoint, &request).await;
        if let Err(e) = post_result {
            self.pending.remove(&id);
            return Err(e);
        }

        // Drain again immediately after POST — the server may have responded
        // before we enter the wait loop.
        self.drain_inbound();

        // Wait for the response to arrive through the oneshot channel.
        let deadline = Duration::from_millis(self.tool_call_timeout_ms);
        let response = timeout(deadline, async {
            loop {
                // Check if the oneshot was already fulfilled.
                if let Ok(response) = response_rx.try_recv() {
                    return Ok(response);
                }

                // Pull the next message from the SSE inbound stream.
                match self.inbound_rx.recv().await {
                    Some(message) => {
                        if let Some(sender) = self.pending.remove(&message.id) {
                            let _ = sender.send(message);
                        }
                        // After dispatching, check if our response arrived.
                        if let Ok(response) = response_rx.try_recv() {
                            return Ok(response);
                        }
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "SSE inbound stream closed",
                        ));
                    }
                }
            }
        })
        .await
        .map_err(|_| {
            self.pending.remove(&id);
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for response to `{method}` from server `{}`",
                    self.server_name
                ),
            )
        })??;

        // Validate the response envelope.
        if response.jsonrpc != "2.0" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "MCP response for {method} used unsupported jsonrpc version `{}`",
                    response.jsonrpc
                ),
            ));
        }

        if response.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "MCP response for {method} used mismatched id: expected {id:?}, got {:?}",
                    response.id
                ),
            ));
        }

        // Deserialize the generic JsonValue result into the expected typed
        // result.
        let typed_response = JsonRpcResponse {
            jsonrpc: response.jsonrpc,
            id: response.id,
            result: response
                .result
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            error: response.error,
        };

        Ok(typed_response)
    }

    /// Drain any buffered inbound messages into their respective oneshot
    /// channels.
    fn drain_inbound(&mut self) {
        while let Ok(response) = self.inbound_rx.try_recv() {
            if let Some(sender) = self.pending.remove(&response.id) {
                let _ = sender.send(response);
            }
        }
    }

    /// POST a JSON-RPC request to the endpoint, handling 401 with an OAuth
    /// retry.
    async fn post_jsonrpc<T: Serialize>(
        &mut self,
        endpoint: &url::Url,
        request: &JsonRpcRequest<T>,
    ) -> io::Result<()> {
        let response = self
            .build_post(endpoint, request)
            .send()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // Attempt OAuth recovery and retry once.
            let new_token = mcp_oauth::on_unauthorized(&self.server_name, &self.config)
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

            // Update base_headers with the new token so the retry and
            // subsequent requests use the refreshed credentials.
            let val = HeaderValue::from_str(&format!("Bearer {new_token}"))
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            self.base_headers.insert(AUTHORIZATION, val);

            let retry_response = self
                .build_post(endpoint, request)
                .send()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e))?;

            if !retry_response.status().is_success() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!(
                        "POST to SSE endpoint returned status {} after retry",
                        retry_response.status()
                    ),
                ));
            }
        } else if !response.status().is_success() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("POST to SSE endpoint returned status {}", response.status()),
            ));
        }

        Ok(())
    }

    /// Build an authenticated POST request with JSON-RPC body.
    fn build_post<T: Serialize>(
        &self,
        endpoint: &url::Url,
        request: &JsonRpcRequest<T>,
    ) -> reqwest::RequestBuilder {
        let mut headers = self.base_headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        self.http_client
            .post(endpoint.clone())
            .headers(headers)
            .json(request)
    }
}

#[async_trait::async_trait]
impl McpTransportProcess for McpSseProcess {
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        self.rpc(id, "initialize", Some(params)).await
    }

    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>> {
        self.rpc(id, "tools/list", params).await
    }

    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>> {
        self.rpc(id, "tools/call", Some(params)).await
    }

    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> io::Result<JsonRpcResponse<McpListResourcesResult>> {
        self.rpc(id, "resources/list", params).await
    }

    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> io::Result<JsonRpcResponse<McpReadResourceResult>> {
        self.rpc(id, "resources/read", Some(params)).await
    }

    fn has_exited(&mut self) -> io::Result<bool> {
        match &self.sse_stream_task {
            Some(handle) => Ok(handle.is_finished()),
            None => Ok(true),
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        if let Some(handle) = self.sse_stream_task.take() {
            handle.abort();
        }
        self.pending.clear();
        Ok(())
    }

    fn resolved_tool_call_timeout_ms(&self) -> u64 {
        self.tool_call_timeout_ms
    }
}

/// Spawn the background task that reads the SSE long-lived stream.
fn spawn_sse_stream_task(
    server_name: String,
    base_url: url::Url,
    auth_token: Option<String>,
    base_headers: HeaderMap,
    http_client: reqwest::Client,
    endpoint_tx: watch::Sender<Option<url::Url>>,
    inbound_tx: mpsc::UnboundedSender<JsonRpcResponse<JsonValue>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut request = http_client
            .get(base_url.clone())
            .header("Accept", "text/event-stream");

        if let Some(token) = &auth_token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }

        // Use the fully-resolved base_headers (includes headersHelper dynamic
        // headers + OAuth token) instead of raw config.headers.
        for (key, value) in &base_headers {
            request = request.header(key, value);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(_e) => {
                return;
            }
        };

        if !response.status().is_success() {
            let _ = server_name;
            return;
        }

        let mut parser = IncrementalSseParser::new();
        let mut stream = response.bytes_stream();

        use futures::StreamExt;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(bytes) => bytes,
                Err(_e) => {
                    break;
                }
            };

            let text = match std::str::from_utf8(&chunk) {
                Ok(s) => s,
                Err(_e) => {
                    break;
                }
            };

            let events = parser.push_chunk(text);

            for event in events {
                match event.event.as_deref() {
                    Some("endpoint") => {
                        let resolved = resolve_endpoint_url(&base_url, &event.data);
                        let _ = endpoint_tx.send(Some(resolved));
                    }
                    Some("message") => {
                        match serde_json::from_str::<JsonRpcResponse<JsonValue>>(&event.data) {
                            Ok(message) => {
                                if inbound_tx.send(message).is_err() {
                                    // Receiver dropped — the process has been
                                    // shut down.
                                    return;
                                }
                            }
                            Err(_e) => {
                                // Malformed JSON-RPC message; skip.
                            }
                        }
                    }
                    _ => {
                        // Ignore other event types per spec.
                    }
                }
            }
        }

        // Flush any trailing partial event from the parser.
        let remaining = parser.finish();
        for event in remaining {
            if event.event.as_deref() == Some("endpoint") {
                let resolved = resolve_endpoint_url(&base_url, &event.data);
                let _ = endpoint_tx.send(Some(resolved));
            } else if event.event.as_deref() == Some("message") {
                if let Ok(message) = serde_json::from_str::<JsonRpcResponse<JsonValue>>(&event.data)
                {
                    let _ = inbound_tx.send(message);
                }
            }
        }

        let _ = server_name;
    })
}

/// Resolve a possibly-relative endpoint URL against the SSE base URL.
fn resolve_endpoint_url(base: &url::Url, raw: &str) -> url::Url {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.parse().unwrap_or_else(|_| base.clone())
    } else {
        base.join(trimmed).unwrap_or_else(|_| base.clone())
    }
}
