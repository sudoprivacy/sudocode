//! MCP Streamable HTTP transport implementation.
//!
//! Provides bidirectional communication over HTTP POST + SSE response parsing,
//! optional GET long-polling for server-initiated messages, and `Mcp-Session-Id`
//! management per the MCP Streamable HTTP specification.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::{timeout, Duration};

use crate::mcp_client::{McpRemoteTransport, DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS};
use crate::mcp_headers_helper;
use crate::mcp_transport::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
    McpListResourcesParams, McpListResourcesResult, McpListToolsParams, McpListToolsResult,
    McpReadResourceParams, McpReadResourceResult, McpToolCallParams, McpToolCallResult,
    McpTransportProcess,
};
use crate::sse::IncrementalSseParser;

/// The `Mcp-Session-Id` header name used by the MCP Streamable HTTP spec.
const MCP_SESSION_ID_HEADER: &str = "Mcp-Session-Id";

// ---------------------------------------------------------------------------
// HttpResponseBody
// ---------------------------------------------------------------------------

/// Parsed body of an HTTP response from the MCP server.
pub enum HttpResponseBody {
    /// The server returned a single JSON-RPC response.
    SingleJson(JsonRpcResponse<JsonValue>),
    /// The server returned an SSE stream of JSON-RPC responses.
    SseStream(Pin<Box<dyn Stream<Item = io::Result<JsonRpcResponse<JsonValue>>> + Send>>),
}

// ---------------------------------------------------------------------------
// SharedSseState
// ---------------------------------------------------------------------------

/// Shared state between the `McpStreamableHttpClient` methods and the spawned
/// GET stream task. Wrapped in `Arc` so the task can own a clone.
#[derive(Debug)]
struct SharedSseState {
    session_id: RwLock<Option<String>>,
    /// Wrapped in `RwLock` so the authorization header can be updated in-place
    /// when a 401 triggers a token refresh (retry-once pattern).
    base_headers: RwLock<HeaderMap>,
}

// ---------------------------------------------------------------------------
// McpStreamableHttpClient
// ---------------------------------------------------------------------------

/// Low-level HTTP client for the MCP Streamable HTTP transport.
///
/// Exposed as `pub` so that `mcp_managed_proxy` can reuse it for single-direction
/// HTTP POST without pulling in the full `McpHttpProcess` lifecycle.
pub struct McpStreamableHttpClient {
    http: reqwest::Client,
    url: url::Url,
    shared: Arc<SharedSseState>,
}

impl McpStreamableHttpClient {
    /// POST a JSON-RPC request body to the server.
    ///
    /// Sets `Accept: application/json, text/event-stream` and
    /// `Content-Type: application/json`. If a `Mcp-Session-Id` has been captured
    /// from a previous response, it is included.
    ///
    /// Returns `HttpResponseBody::SingleJson` when the response Content-Type is
    /// `application/json`, or `HttpResponseBody::SseStream` when it is
    /// `text/event-stream`.
    pub async fn post_request(&self, body: &JsonValue) -> io::Result<HttpResponseBody> {
        let session_id_guard = self.shared.session_id.read().await;
        let base_headers_guard = self.shared.base_headers.read().await;
        let mut request = self
            .http
            .post(self.url.as_str())
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json")
            .json(body);

        for (name, value) in &*base_headers_guard {
            request = request.header(name, value);
        }

        if let Some(sid) = session_id_guard.as_deref() {
            request = request.header(MCP_SESSION_ID_HEADER, sid);
        }

        drop(session_id_guard);
        drop(base_headers_guard);

        let response = request
            .send()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "MCP HTTP server returned 401 Unauthorized",
            ));
        }
        if !status.is_success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("MCP HTTP server returned status {status}"),
            ));
        }

        // Capture session id from response headers.
        if let Some(sid) = response.headers().get(MCP_SESSION_ID_HEADER) {
            let sid_str = sid
                .to_str()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
                .to_string();
            let mut guard = self.shared.session_id.write().await;
            *guard = Some(sid_str);
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if content_type.contains("text/event-stream") {
            let stream = Self::parse_sse_response(response);
            Ok(HttpResponseBody::SseStream(stream))
        } else {
            // Default: treat as application/json.
            let json: JsonRpcResponse<JsonValue> = response
                .json()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(HttpResponseBody::SingleJson(json))
        }
    }

    /// Send an HTTP GET to establish a long-lived SSE listener for
    /// server-initiated messages.
    pub async fn get_stream(
        &self,
    ) -> io::Result<Pin<Box<dyn Stream<Item = io::Result<JsonRpcResponse<JsonValue>>> + Send>>>
    {
        let session_id_guard = self.shared.session_id.read().await;
        let base_headers_guard = self.shared.base_headers.read().await;

        let mut request = self
            .http
            .get(self.url.as_str())
            .header(ACCEPT, "text/event-stream");

        for (name, value) in &*base_headers_guard {
            request = request.header(name, value);
        }

        if let Some(sid) = session_id_guard.as_deref() {
            request = request.header(MCP_SESSION_ID_HEADER, sid);
        }

        drop(session_id_guard);
        drop(base_headers_guard);

        let response = request
            .send()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("MCP HTTP GET stream returned status {status}"),
            ));
        }

        Ok(Self::parse_sse_response(response))
    }

    /// Send an HTTP DELETE to terminate the session (best-effort).
    pub async fn delete_session(&self) {
        let session_id_guard = self.shared.session_id.read().await;
        let base_headers_guard = self.shared.base_headers.read().await;

        let mut request = self.http.delete(self.url.as_str());

        for (name, value) in &*base_headers_guard {
            request = request.header(name, value);
        }

        if let Some(sid) = session_id_guard.as_deref() {
            request = request.header(MCP_SESSION_ID_HEADER, sid);
        }

        drop(session_id_guard);
        drop(base_headers_guard);

        let _ = request.send().await;
    }

    /// Read the current session id (if any).
    pub async fn session_id(&self) -> Option<String> {
        self.shared.session_id.read().await.clone()
    }

    /// Update the `Authorization` header in the shared base headers.
    ///
    /// Called after a successful token refresh so that subsequent requests
    /// (POST, GET stream, DELETE) automatically carry the new credentials.
    pub async fn update_auth_token(&self, token: &str) {
        let val = HeaderValue::from_str(&format!("Bearer {token}"));
        if let Ok(val) = val {
            let mut guard = self.shared.base_headers.write().await;
            guard.insert(AUTHORIZATION, val);
        }
    }

    /// Convert an HTTP response with `text/event-stream` into a boxed stream
    /// of parsed JSON-RPC responses using `IncrementalSseParser`.
    fn parse_sse_response(
        response: reqwest::Response,
    ) -> Pin<Box<dyn Stream<Item = io::Result<JsonRpcResponse<JsonValue>>> + Send>> {
        // The parser must live across chunks, so we wrap it in an unfold
        // stream that carries state.
        let byte_stream = response.bytes_stream();

        let stream = futures::stream::unfold(
            (byte_stream, IncrementalSseParser::new()),
            |(mut byte_stream, mut parser)| async move {
                loop {
                    match byte_stream.next().await {
                        Some(Ok(chunk)) => {
                            let text = String::from_utf8_lossy(&chunk).into_owned();
                            let events = parser.push_chunk(&text);
                            if events.is_empty() {
                                // No complete events yet; keep reading.
                                continue;
                            }
                            let mut results = Vec::new();
                            for event in events {
                                if event.data.trim().is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<JsonRpcResponse<JsonValue>>(
                                    &event.data,
                                ) {
                                    Ok(response) => results.push(Ok(response)),
                                    Err(e) => {
                                        results.push(Err(io::Error::new(
                                            io::ErrorKind::InvalidData,
                                            e,
                                        )));
                                    }
                                }
                            }
                            if !results.is_empty() {
                                return Some((results, (byte_stream, parser)));
                            }
                            // All events were empty; keep reading.
                        }
                        Some(Err(e)) => {
                            let error = io::Error::new(io::ErrorKind::Other, e);
                            return Some((vec![Err(error)], (byte_stream, parser)));
                        }
                        None => {
                            // Stream ended. Flush trailing partial event.
                            let remaining = parser.finish();
                            let mut results = Vec::new();
                            for event in remaining {
                                if event.data.trim().is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<JsonRpcResponse<JsonValue>>(
                                    &event.data,
                                ) {
                                    Ok(response) => results.push(Ok(response)),
                                    Err(e) => {
                                        results.push(Err(io::Error::new(
                                            io::ErrorKind::InvalidData,
                                            e,
                                        )));
                                    }
                                }
                            }
                            if results.is_empty() {
                                return None;
                            }
                            return Some((results, (byte_stream, parser)));
                        }
                    }
                }
            },
        )
        .map(futures::stream::iter)
        .flatten();

        Box::pin(stream)
    }
}

impl fmt::Debug for McpStreamableHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpStreamableHttpClient")
            .field("url", &self.url)
            .field("base_headers", &self.shared.base_headers)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// McpHttpProcess
// ---------------------------------------------------------------------------

/// MCP Streamable HTTP transport process.
///
/// Manages a bidirectional session with an MCP server over HTTP:
/// - POST requests carry JSON-RPC messages.
/// - Responses come back as either single JSON or SSE streams.
/// - An optional GET long-poll connection receives server-initiated messages.
/// - `Mcp-Session-Id` header is tracked across requests.
pub struct McpHttpProcess {
    server_name: String,
    config: McpRemoteTransport,
    client: McpStreamableHttpClient,
    inbound_rx: mpsc::UnboundedReceiver<JsonRpcResponse<JsonValue>>,
    get_stream_task: Option<tokio::task::JoinHandle<()>>,
    pending: HashMap<JsonRpcId, oneshot::Sender<JsonRpcResponse<JsonValue>>>,
    tool_call_timeout_ms: u64,
}

impl McpHttpProcess {
    /// Connect to an MCP server using Streamable HTTP transport.
    ///
    /// This creates the HTTP client and prepares the channel for inbound
    /// messages. The `initialize` handshake is *not* performed here; the
    /// manager layer calls `initialize()` on the returned process.
    pub async fn connect(
        server_name: &str,
        config: &McpRemoteTransport,
        scope: crate::config::ConfigSource,
        workspace_is_trusted: bool,
    ) -> io::Result<Self> {
        let url: url::Url = config
            .url
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

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
        let auth_token = match crate::mcp_oauth::ensure_access_token(server_name, config).await {
            Ok(token) => token,
            Err(crate::mcp_oauth::McpOAuthError::NeedsInteractiveAuth { .. }) => {
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

        // Set User-Agent: sudocode/{version}.
        let ua = format!("sudocode/{}", env!("CARGO_PKG_VERSION"));
        base_headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_str(&ua)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
        );

        let http_client = reqwest::Client::builder()
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let shared = Arc::new(SharedSseState {
            session_id: RwLock::new(None),
            base_headers: RwLock::new(base_headers),
        });

        let client = McpStreamableHttpClient {
            http: http_client,
            url,
            shared: shared.clone(),
        };

        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();

        // Spawn the GET long-poll task. It will reconnect as needed.
        let get_stream_task = Self::spawn_get_stream_task(&client, inbound_tx, shared);

        Ok(Self {
            server_name: server_name.to_string(),
            config: config.clone(),
            client,
            inbound_rx,
            get_stream_task: Some(get_stream_task),
            pending: HashMap::new(),
            tool_call_timeout_ms: DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS,
        })
    }

    /// Send a JSON-RPC request and wait for the matching response.
    ///
    /// The response may arrive either from the POST response body (single JSON
    /// or first SSE event) or from the GET long-poll stream.
    async fn send_and_wait<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        id: JsonRpcId,
        method: impl Into<String>,
        params: Option<TParams>,
        timeout_ms: u64,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let method = method.into();
        let request = JsonRpcRequest::new(id.clone(), method.clone(), params);
        let body = serde_json::to_value(&request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Set up oneshot channel so we can match the response by id.
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id.clone(), tx);

        // POST the request, handling 401 with an OAuth retry-once.
        let response_body = match self.client.post_request(&body).await {
            Ok(body) => body,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // 401 Unauthorized — attempt OAuth recovery and retry once.
                // Only applicable when the server is configured with OAuth.
                if matches!(self.config.auth, crate::mcp_client::McpClientAuth::OAuth(_)) {
                    let new_token =
                        crate::mcp_oauth::on_unauthorized(&self.server_name, &self.config)
                            .await
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                    // Update the shared auth header for this and future requests.
                    self.client.update_auth_token(&new_token).await;

                    // Retry the POST once.
                    self.client.post_request(&body).await?
                } else {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        };

        match response_body {
            HttpResponseBody::SingleJson(json_response) => {
                // If the response id matches a pending entry, complete it.
                if let Some(sender) = self.pending.remove(&json_response.id) {
                    let _ = sender.send(json_response);
                }
            }
            HttpResponseBody::SseStream(mut stream) => {
                // Read events from the SSE stream with a timeout and route
                // them to the appropriate pending entry or the inbound channel.
                let deadline = Duration::from_millis(timeout_ms);
                let result = timeout(deadline, async {
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(json_response) => {
                                let response_id = json_response.id.clone();
                                if let Some(sender) = self.pending.remove(&response_id) {
                                    let _ = sender.send(json_response);
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                })
                .await;

                if result.is_err() {
                    // Timeout: remove the pending entry.
                    self.pending.remove(&id);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("MCP HTTP request for {method} timed out after {timeout_ms} ms"),
                    ));
                }
            }
        }

        // Wait for the matched response from either the POST or GET stream.
        // The oneshot might already be fulfilled (from SSE stream above or from
        // a concurrent GET stream event). Poll the inbound channel to drain
        // any messages that arrived in the meantime.
        self.drain_inbound();

        let deadline = Duration::from_millis(timeout_ms);
        match timeout(deadline, rx).await {
            Ok(Ok(response)) => {
                if response.jsonrpc != "2.0" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "MCP response for {method} used unsupported jsonrpc version `{}`",
                            response.jsonrpc
                        ),
                    ));
                }

                let typed: JsonRpcResponse<TResult> = JsonRpcResponse {
                    jsonrpc: response.jsonrpc,
                    id: response.id,
                    result: response
                        .result
                        .map(|v| {
                            serde_json::from_value(v)
                                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                        })
                        .transpose()?,
                    error: response.error,
                };
                Ok(typed)
            }
            Ok(Err(_)) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("MCP HTTP response channel closed for {method}"),
            )),
            Err(_) => {
                self.pending.remove(&id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("MCP HTTP request for {method} timed out after {timeout_ms} ms"),
                ))
            }
        }
    }

    /// Drain any pending messages from the GET stream into their respective
    /// oneshot channels.
    fn drain_inbound(&mut self) {
        while let Ok(response) = self.inbound_rx.try_recv() {
            if let Some(sender) = self.pending.remove(&response.id) {
                let _ = sender.send(response);
            }
            // Discard responses that don't match any pending request (e.g.
            // server-initiated notifications).
        }
    }

    /// Spawn a background task that maintains a GET SSE connection and routes
    /// incoming responses to the inbound channel.
    fn spawn_get_stream_task(
        client: &McpStreamableHttpClient,
        inbound_tx: mpsc::UnboundedSender<JsonRpcResponse<JsonValue>>,
        shared: Arc<SharedSseState>,
    ) -> tokio::task::JoinHandle<()> {
        let http_client = client.http.clone();
        let url = client.url.clone();

        let get_client = McpStreamableHttpClient {
            http: http_client,
            url,
            shared,
        };

        tokio::spawn(async move {
            // Best-effort GET stream loop. If the connection drops, we simply
            // exit -- the GET stream is optional per the MCP spec.
            match get_client.get_stream().await {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(response) => {
                                // Route to the inbound channel. If the receiver
                                // is dropped (process shut down), exit quietly.
                                if inbound_tx.send(response).is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                Err(_) => {
                    // GET stream is optional; failure to connect is not fatal.
                }
            }
        })
    }

    /// Update the `Authorization` header in the underlying HTTP client.
    ///
    /// Used by `McpManagedProxyProcess` after reloading credentials on a 401.
    pub async fn update_auth_token(&self, token: &str) {
        self.client.update_auth_token(token).await;
    }
}

impl fmt::Debug for McpHttpProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpHttpProcess")
            .field("server_name", &self.server_name)
            .field("client", &self.client)
            .field("tool_call_timeout_ms", &self.tool_call_timeout_ms)
            .field("pending_count", &self.pending.len())
            .field("get_stream_running", &self.get_stream_task.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl McpTransportProcess for McpHttpProcess {
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        let timeout_ms = self.tool_call_timeout_ms;
        self.send_and_wait(id, "initialize", Some(params), timeout_ms)
            .await
    }

    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>> {
        let timeout_ms = self.tool_call_timeout_ms;
        self.send_and_wait(id, "tools/list", params, timeout_ms)
            .await
    }

    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>> {
        let timeout_ms = self.tool_call_timeout_ms;
        self.send_and_wait(id, "tools/call", Some(params), timeout_ms)
            .await
    }

    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> io::Result<JsonRpcResponse<McpListResourcesResult>> {
        let timeout_ms = self.tool_call_timeout_ms;
        self.send_and_wait(id, "resources/list", params, timeout_ms)
            .await
    }

    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> io::Result<JsonRpcResponse<McpReadResourceResult>> {
        let timeout_ms = self.tool_call_timeout_ms;
        self.send_and_wait(id, "resources/read", Some(params), timeout_ms)
            .await
    }

    fn has_exited(&mut self) -> io::Result<bool> {
        match &self.get_stream_task {
            Some(handle) => Ok(handle.is_finished()),
            None => Ok(true),
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        // Abort the GET stream task.
        if let Some(handle) = self.get_stream_task.take() {
            handle.abort();
        }
        // Best-effort DELETE to close the session.
        self.client.delete_session().await;
        // Clear pending entries.
        self.pending.clear();
        Ok(())
    }

    fn resolved_tool_call_timeout_ms(&self) -> u64 {
        self.tool_call_timeout_ms
    }
}
