//! MCP Streamable HTTP transport (2025-03-26 spec).
//!
//! Wire shape: every JSON-RPC message is a fresh HTTP POST to a single MCP
//! endpoint; the server answers with either `application/json` (one JSON-RPC
//! response) or `text/event-stream` (an SSE stream carrying the response,
//! optionally interleaved with server notifications). A server MAY assign a
//! session id via the `Mcp-Session-Id` response header on `initialize`; the
//! client echoes it on subsequent requests. This module implements that shape
//! on top of `reqwest` and the existing
//! [`IncrementalSseParser`](crate::IncrementalSseParser), and exposes it
//! through the [`McpConnection`](crate::McpConnection) trait so
//! [`McpServerManager`](crate::mcp_server_manager::McpServerManager) drives it
//! exactly like the stdio and SSE transports.
//!
//! Per the configured short-lived model, a connection lives for a single
//! `McpServerManager` request sequence: `connect` resolves the endpoint and
//! headers, the trait methods POST requests against it, and the connection is
//! dropped (re-established) on the next call.

use std::collections::BTreeMap;
use std::io;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::mcp_client::McpRemoteTransport;
use crate::mcp_connection::McpConnection;
use crate::mcp_remote::resolve_headers;
use crate::mcp_server_manager::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
    McpListResourcesParams, McpListResourcesResult, McpListToolsParams, McpListToolsResult,
    McpReadResourceParams, McpReadResourceResult, McpToolCallParams, McpToolCallResult,
};
use crate::IncrementalSseParser;

/// `Accept` header required by the Streamable HTTP spec on every POST: the
/// server may answer with either content type.
const ACCEPTED: &str = "application/json, text/event-stream";

/// Header name a server uses to assign/track a Streamable HTTP session.
const MCP_SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");

/// A live MCP Streamable HTTP connection: a single endpoint driven over one
/// [`reqwest::Client`], optionally tracking a server-assigned session id.
#[derive(Debug)]
pub struct McpHttpConnection {
    client: Client,
    endpoint: Url,
    headers: BTreeMap<String, String>,
    session_id: Option<String>,
}

impl McpHttpConnection {
    /// Resolve headers (static + `headersHelper`) and parse `transport.url`
    /// as the single MCP endpoint. No request is issued here — the Streamable
    /// HTTP transport first talks to the server on `initialize`.
    pub(crate) async fn connect(
        transport: &McpRemoteTransport,
        server_name: &str,
    ) -> io::Result<Self> {
        let client = Client::builder().build().map_err(reqwest_error_to_io)?;
        let headers = resolve_headers(transport, server_name).await;
        let endpoint = Url::parse(&transport.url).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP HTTP url `{}`: {error}", transport.url),
            )
        })?;
        Ok(Self {
            client,
            endpoint,
            headers,
            session_id: None,
        })
    }

    /// POST a JSON-RPC request to the endpoint and read the matching response.
    /// The response body is either a single JSON-RPC object or an SSE stream;
    /// events that fail to parse as a response or carry a different id
    /// (notifications, progress) are skipped until the matching response
    /// arrives. A `Mcp-Session-Id` response header, when present, is recorded
    /// and echoed on subsequent requests.
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

        let mut request_headers = build_header_map(&self.headers)?;
        request_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        request_headers.insert(ACCEPT, HeaderValue::from_static(ACCEPTED));
        if let Some(session_id) = &self.session_id {
            request_headers.insert(
                MCP_SESSION_ID,
                HeaderValue::from_str(session_id)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
            );
        }

        let response = self
            .client
            .post(self.endpoint.clone())
            .headers(request_headers)
            .body(body)
            .send()
            .await
            .map_err(reqwest_error_to_io)?;
        let status = response.status();
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "MCP HTTP POST `{}` returned HTTP {status}",
                self.endpoint
            )));
        }

        // A server MAY assign/refresh the session id on any response; record
        // it so subsequent POSTs echo it back (spec requirement).
        if let Some(session_id) = response
            .headers()
            .get(MCP_SESSION_ID)
            .and_then(|value| value.to_str().ok())
        {
            self.session_id = Some(session_id.to_string());
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.contains("text/event-stream") {
            return read_sse_stream::<TResult>(response, id).await;
        }

        let bytes = response.bytes().await.map_err(reqwest_error_to_io)?;
        let response: JsonRpcResponse<TResult> =
            serde_json::from_slice(&bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("MCP HTTP response is not a JSON-RPC response: {error}"),
                )
            })?;
        Ok(response)
    }
}

/// Drain a `text/event-stream` POST response, returning the first JSON-RPC
/// response whose id matches. Non-matching events (notifications, progress)
/// are skipped, matching the legacy SSE read loop.
async fn read_sse_stream<TResult: DeserializeOwned>(
    response: reqwest::Response,
    id: JsonRpcId,
) -> io::Result<JsonRpcResponse<TResult>> {
    let mut stream = response.bytes_stream();
    let mut parser = IncrementalSseParser::new();
    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                let chunk = String::from_utf8_lossy(&bytes[..]);
                for event in parser.push_chunk(&chunk) {
                    if let Ok(response) = serde_json::from_str::<JsonRpcResponse<TResult>>(&event.data)
                    {
                        if response.id == id {
                            return Ok(response);
                        }
                    }
                }
            }
            Some(Err(error)) => return Err(reqwest_error_to_io(error)),
            None => {
                for event in parser.finish() {
                    if let Ok(response) = serde_json::from_str::<JsonRpcResponse<TResult>>(&event.data)
                    {
                        if response.id == id {
                            return Ok(response);
                        }
                    }
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "MCP HTTP stream closed before response",
                ));
            }
        }
    }
}

#[async_trait]
impl McpConnection for McpHttpConnection {
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
        Ok(false)
    }

    async fn shutdown(&mut self) {}
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
                format!("invalid MCP HTTP header name `{key}`: {error}"),
            )
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP HTTP header value for `{key}`: {error}"),
            )
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

fn reqwest_error_to_io(error: reqwest::Error) -> io::Error {
    io::Error::other(format!("MCP HTTP error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::{McpClientAuth, McpRemoteTransport};
    use crate::mcp_server_manager::{McpInitializeClientInfo, McpInitializeParams};
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, Response, StatusCode};
    use axum::{routing::post, Router};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// A canned HTTP response the mock server returns for one POST.
    struct MockResp {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    /// Shared mock state: a FIFO of canned responses plus every received
    /// request's headers (so tests can assert what the client sent).
    struct MockState {
        responses: Mutex<Vec<MockResp>>,
        request_headers: Mutex<Vec<HeaderMap>>,
    }

    /// `POST /mcp` handler: record the request headers, pop the next canned
    /// response, return it verbatim.
    async fn handle_mcp(
        State(state): State<Arc<MockState>>,
        request_headers: HeaderMap,
        _body: Bytes,
    ) -> Response<Body> {
        state
            .request_headers
            .lock()
            .expect("request_headers lock")
            .push(request_headers);
        let resp = state
            .responses
            .lock()
            .expect("responses lock")
            .remove(0);
        let mut builder = Response::builder().status(
            StatusCode::from_u16(resp.status).expect("valid status"),
        );
        for (name, value) in &resp.headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(resp.body)).expect("valid body")
    }

    /// Spawn a mock Streamable HTTP server on an ephemeral port.
    async fn spawn_mock_http(responses: Vec<MockResp>) -> (std::net::SocketAddr, Arc<MockState>) {
        let state = Arc::new(MockState {
            responses: Mutex::new(responses),
            request_headers: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/mcp", post(handle_mcp))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, state)
    }

    fn transport(addr: std::net::SocketAddr) -> McpRemoteTransport {
        McpRemoteTransport {
            url: format!("http://{addr}/mcp"),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        }
    }

    fn init_params() -> McpInitializeParams {
        McpInitializeParams {
            protocol_version: "2025-03-26".to_string(),
            capabilities: serde_json::json!({}),
            client_info: McpInitializeClientInfo {
                name: "test".to_string(),
                version: "0.1.0".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn http_initializes_lists_and_calls_echoing_session_id() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock-http","version":"0.1.0"}}}"#;
        let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}}"#;
        let call = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echo:hi"}],"isError":false}}"#;
        let (addr, state) = spawn_mock_http(vec![
            MockResp {
                status: 200,
                headers: vec![
                    ("content-type", "application/json"),
                    ("mcp-session-id", "sess-abc"),
                ],
                body: init.to_string(),
            },
            MockResp {
                status: 200,
                headers: vec![("content-type", "application/json")],
                body: list.to_string(),
            },
            MockResp {
                status: 200,
                headers: vec![("content-type", "application/json")],
                body: call.to_string(),
            },
        ])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-http")
            .await
            .expect("connect");
        assert!(connection.session_id.is_none());

        let initialized = connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await
            .expect("initialize");
        assert_eq!(initialized.id, JsonRpcId::Number(1));
        assert_eq!(
            initialized.result.expect("init result").server_info.name,
            "mock-http"
        );
        // Session id assigned by the server on initialize is recorded.
        assert_eq!(connection.session_id.as_deref(), Some("sess-abc"));

        let tools = connection
            .list_tools(JsonRpcId::Number(2), None)
            .await
            .expect("list tools");
        assert_eq!(tools.result.expect("list result").tools.len(), 1);

        // The 2nd request must echo Mcp-Session-Id and advertise both Accept types.
        let request_headers = state.request_headers.lock().expect("headers lock");
        assert_eq!(
            request_headers[1]
                .get("mcp-session-id")
                .expect("session id sent")
                .to_str()
                .expect("ascii"),
            "sess-abc"
        );
        assert_eq!(
            request_headers[1].get("accept").expect("accept sent").to_str().expect("ascii"),
            "application/json, text/event-stream"
        );
        drop(request_headers);

        let call_result = connection
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
        assert!(!call_result
            .result
            .expect("call result")
            .is_error
            .unwrap_or(true));
    }

    #[tokio::test]
    async fn http_reads_response_from_sse_stream() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock-http-sse","version":"0.1.0"}}}"#;
        let sse_body = format!("event: message\ndata: {init}\n\n");
        let (addr, _state) = spawn_mock_http(vec![MockResp {
            status: 200,
            headers: vec![("content-type", "text/event-stream")],
            body: sse_body,
        }])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-http-sse")
            .await
            .expect("connect");
        let initialized = connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await
            .expect("initialize");
        assert_eq!(initialized.id, JsonRpcId::Number(1));
        assert_eq!(
            initialized.result.expect("init result").server_info.name,
            "mock-http-sse"
        );
    }

    #[tokio::test]
    async fn http_non_2xx_surfaces_error() {
        let (addr, _state) = spawn_mock_http(vec![MockResp {
            status: 404,
            headers: vec![],
            body: String::new(),
        }])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-http-404")
            .await
            .expect("connect");
        let result = connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await;
        let error = result.expect_err("initialize should fail on 404");
        assert!(error.to_string().contains("404"), "error was: {error}");
    }
}
