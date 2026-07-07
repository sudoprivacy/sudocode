//! MCP SSE transport — legacy HTTP+SSE transport (pre-2025-06-18 MCP spec).
//!
//! Wire shape: the client opens a long-lived `GET {url}` Server-Sent-Events
//! stream; the server emits an `endpoint` event whose data is the URI the
//! client must POST JSON-RPC requests to; responses travel back on the same
//! SSE stream. This module implements that shape on top of `reqwest` + the
//! existing [`IncrementalSseParser`](crate::IncrementalSseParser) and exposes
//! it through the [`McpConnection`](crate::McpConnection) trait, so
//! [`McpServerManager`](crate::McpServerManager) drives it exactly like the
//! stdio transport.
//!
//! Per the configured short-lived model, a connection lives for a single
//! `McpServerManager` request sequence: `connect` opens the stream and resolves
//! the endpoint, the trait methods POST requests against it, and the connection
//! is dropped (re-established) on the next call.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::mcp_client::McpRemoteTransport;
use crate::mcp_connection::McpConnection;
use crate::mcp_remote::{resolve_headers, MAX_RESPONSE_BYTES};
use crate::mcp_server_manager::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
    McpListResourcesParams, McpListResourcesResult, McpListToolsParams, McpListToolsResult,
    McpReadResourceParams, McpReadResourceResult, McpToolCallParams, McpToolCallResult,
};
use crate::{IncrementalSseParser, SseEvent};

/// A live MCP SSE connection: a long-lived GET stream paired with a POST
/// endpoint, driven over a single [`reqwest::Client`].
#[derive(Debug)]
pub struct McpSseConnection {
    client: Client,
    endpoint: Url,
    headers: BTreeMap<String, String>,
    events: mpsc::Receiver<SseEvent>,
    closed: Arc<AtomicBool>,
}

impl McpSseConnection {
    /// Open the SSE stream at `transport.url`, await the `endpoint` event, and
    /// return a ready-to-use connection. Static `headers` are merged with the
    /// dynamic `headersHelper` output (dynamic overrides static); helper
    /// failures are absorbed so the connection still proceeds with static
    /// headers alone.
    pub(crate) async fn connect(
        transport: &McpRemoteTransport,
        server_name: &str,
    ) -> io::Result<Self> {
        // Redirects disabled: this client carries user auth headers, which
        // reqwest would forward to any redirect target (only standard
        // sensitive headers are stripped cross-host). A legitimate redirect
        // (http→https upgrade, load-balancer 307, trailing slash) surfaces as
        // an error instead. See `mcp_http::connect` for the same rationale.
        // Only headers/headersHelper auth is supported; `transport.auth`
        // (OAuth) is intentionally not consumed by the remote transports.
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(reqwest_error_to_io)?;
        let headers = resolve_headers(transport, server_name).await;

        let base_url = Url::parse(&transport.url).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP SSE url `{}`: {error}", transport.url),
            )
        })?;

        let get_headers = build_header_map(&headers)?;
        let response = client
            .get(base_url.clone())
            .headers(get_headers)
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(reqwest_error_to_io)?;
        let status = response.status();
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "MCP SSE GET `{}` returned HTTP {status}",
                transport.url
            )));
        }

        let (sender, mut receiver) = mpsc::channel::<SseEvent>(64);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_task = closed.clone();
        let mut stream = Box::pin(response.bytes_stream());
        tokio::spawn(async move {
            let mut parser = IncrementalSseParser::new();
            let mut read = 0usize;
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        read += bytes.len();
                        if read > MAX_RESPONSE_BYTES {
                            // Over the cap: stop draining to bound memory. The
                            // receiver sees the stream close (has_exited → reset).
                            closed_task.store(true, Ordering::Relaxed);
                            return;
                        }
                        for event in parser.push_chunk(&bytes[..]) {
                            if sender.send(event).await.is_err() {
                                // Receiver dropped — connection torn down.
                                closed_task.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            for event in parser.finish() {
                if sender.send(event).await.is_err() {
                    break;
                }
            }
            closed_task.store(true, Ordering::Relaxed);
        });

        let endpoint_event = receiver.recv().await.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "MCP SSE stream closed before endpoint event",
            )
        })?;
        if endpoint_event.event.as_deref() != Some("endpoint") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP SSE first event was not an `endpoint` event",
            ));
        }
        let endpoint_path = endpoint_event.data.trim();
        let endpoint = base_url.join(endpoint_path).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("MCP SSE endpoint `{endpoint_path}` is not a valid URL: {error}"),
            )
        })?;

        Ok(Self {
            client,
            endpoint,
            headers,
            events: receiver,
            closed,
        })
    }

    /// POST a JSON-RPC request to the endpoint and read the matching response
    /// off the SSE stream. Events that fail to parse as a response or carry a
    /// different id (notifications, progress) are skipped until the matching
    /// response arrives.
    async fn post_and_read<TParams, TResult>(
        &mut self,
        method: &str,
        id: JsonRpcId,
        params: Option<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>>
    where
        TParams: Serialize,
        TResult: DeserializeOwned,
    {
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let body = serde_json::to_vec(&request)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

        let post_headers = build_header_map(&self.headers)?;
        let response = self
            .client
            .post(self.endpoint.clone())
            .headers(post_headers)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(body)
            .send()
            .await
            .map_err(reqwest_error_to_io)?;
        let status = response.status();
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "MCP SSE POST `{}` returned HTTP {status}",
                self.endpoint
            )));
        }

        loop {
            let event = self.events.recv().await.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "MCP SSE stream closed before response",
                )
            })?;
            let response: JsonRpcResponse<TResult> = match serde_json::from_str(&event.data) {
                Ok(response) => response,
                Err(_) => continue,
            };
            if response.id == id {
                return Ok(response);
            }
        }
    }
}

#[async_trait]
impl McpConnection for McpSseConnection {
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        self.post_and_read("initialize", id, Some(params)).await
    }

    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>> {
        self.post_and_read("tools/list", id, params).await
    }

    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>> {
        self.post_and_read("tools/call", id, Some(params)).await
    }

    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> io::Result<JsonRpcResponse<McpListResourcesResult>> {
        self.post_and_read("resources/list", id, params).await
    }

    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> io::Result<JsonRpcResponse<McpReadResourceResult>> {
        self.post_and_read("resources/read", id, Some(params)).await
    }

    async fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.closed.load(Ordering::Relaxed))
    }

    async fn shutdown(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        self.events.close();
    }
}

/// Build a [`HeaderMap`] from a string→string table. Header names/values that
/// fail to parse abort the connection with a clear error rather than being
/// silently dropped.
fn build_header_map(headers: &BTreeMap<String, String>) -> io::Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP SSE header name `{key}`: {error}"),
            )
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP SSE header value for `{key}`: {error}"),
            )
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

fn reqwest_error_to_io(error: reqwest::Error) -> io::Error {
    io::Error::other(format!("MCP SSE HTTP error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::{McpClientAuth, McpRemoteTransport};
    use crate::mcp_server_manager::{McpInitializeClientInfo, McpInitializeParams};
    use axum::response::sse::{Event, Sse};
    use axum::{routing::get, routing::post, Router};
    use std::sync::Arc;

    /// Spawn a mock MCP-over-SSE server on an ephemeral port.
    ///
    /// `GET /sse` emits an `endpoint` event pointing at `/message`, then the
    /// supplied JSON-RPC responses in order. `POST /message` acknowledges with
    /// 202; the response is already queued on the GET stream, which mirrors
    /// how a real server streams responses back over the open SSE connection.
    async fn spawn_mock_sse(responses: Vec<&'static str>) -> std::net::SocketAddr {
        let mut events: Vec<Event> = vec![Event::default().event("endpoint").data("/message")];
        for response in responses {
            events.push(Event::default().data(response));
        }
        let events = Arc::new(events);

        let app = Router::new()
            .route(
                "/sse",
                get({
                    let events = events.clone();
                    move || {
                        let events = events.clone();
                        async move {
                            Sse::new(futures::stream::iter(
                                (*events).clone().into_iter().map(Ok::<Event, io::Error>),
                            ))
                        }
                    }
                }),
            )
            .route(
                "/message",
                post(|| async { axum::http::StatusCode::ACCEPTED }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    #[tokio::test]
    async fn sse_connects_initializes_lists_and_calls() {
        let addr = spawn_mock_sse(vec![
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock-sse","version":"0.1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}}"#,
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echo:hi"}],"isError":false}}"#,
        ])
        .await;

        let transport = McpRemoteTransport {
            url: format!("http://{addr}/sse"),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let mut connection = McpSseConnection::connect(&transport, "mock-sse")
            .await
            .expect("connect");

        let initialized = connection
            .initialize(
                JsonRpcId::Number(1),
                McpInitializeParams {
                    protocol_version: "2025-03-26".to_string(),
                    capabilities: serde_json::json!({}),
                    client_info: McpInitializeClientInfo {
                        name: "test".to_string(),
                        version: "0.1.0".to_string(),
                    },
                },
            )
            .await
            .expect("initialize");
        assert_eq!(initialized.id, JsonRpcId::Number(1));
        assert_eq!(
            initialized.result.expect("init result").server_info.name,
            "mock-sse"
        );

        let tools = connection
            .list_tools(JsonRpcId::Number(2), None)
            .await
            .expect("list tools");
        assert_eq!(tools.result.expect("list result").tools.len(), 1);

        let call = connection
            .call_tool(
                JsonRpcId::Number(3),
                McpToolCallParams {
                    name: "echo".to_string(),
                    arguments: Some(serde_json::json!({"text": "hi"})),
                    meta: None,
                },
            )
            .await
            .expect("call tool");
        assert!(!call.result.expect("call result").is_error.unwrap_or(true));
    }

    #[tokio::test]
    async fn sse_connect_rejects_non_endpoint_first_event() {
        // First event is a plain message, not `endpoint` → connect must fail.
        let app = Router::new().route(
            "/sse",
            get(|| async {
                Sse::new(futures::stream::iter(vec![Ok::<Event, io::Error>(
                    Event::default().event("message").data("hello"),
                )]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let transport = McpRemoteTransport {
            url: format!("http://{addr}/sse"),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let result = McpSseConnection::connect(&transport, "mock-sse").await;
        assert!(
            result.is_err(),
            "connect should fail without an endpoint event"
        );
    }
}
