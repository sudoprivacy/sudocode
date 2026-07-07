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
use crate::mcp_remote::{resolve_headers, MAX_RESPONSE_BYTES};
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
    ///
    /// Redirects are **disabled** on the client: this connection carries
    /// user-supplied auth headers and a server-issued `Mcp-Session-Id`, which
    /// reqwest would forward to any redirect target (only a fixed set of
    /// standard sensitive headers are stripped cross-host). Disabling means a
    /// legitimate redirect (http→https upgrade, load-balancer 307, trailing
    /// slash) surfaces as an error rather than being followed — the safe
    /// default for a credentialed client.
    ///
    /// Only `headers`/`headersHelper` authentication is supported here;
    /// `transport.auth` (OAuth) is intentionally not consumed by the remote
    /// transports (see `McpRemoteTransport.auth`).
    pub(crate) async fn connect(
        transport: &McpRemoteTransport,
        server_name: &str,
    ) -> io::Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(reqwest_error_to_io)?;
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
    /// arrives. A `Mcp-Session-Id` response header is recorded **only on
    /// `initialize`** (the spec assigns it there) and echoed on subsequent
    /// requests.
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

        // Record the session id only on `initialize` (the spec assigns it
        // there). Deliberately do not refresh on later responses.
        if method == "initialize" {
            if let Some(session_id) = response
                .headers()
                .get(MCP_SESSION_ID)
                .and_then(|value| value.to_str().ok())
            {
                self.session_id = Some(session_id.to_string());
            }
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.contains("text/event-stream") {
            return read_sse_stream::<TResult>(response, id, MAX_RESPONSE_BYTES).await;
        }

        // JSON branch (also the fallback when Content-Type is missing/unknown:
        // probing would require reading the whole body, which the spec allows a
        // server to keep open past the response — so we attempt JSON and, on
        // failure, surface a message that names the actual Content-Type).
        let bytes = read_limited(response, MAX_RESPONSE_BYTES).await?;
        serde_json::from_slice::<JsonRpcResponse<TResult>>(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "MCP HTTP response is not a JSON-RPC response (Content-Type: {content_type:?}, expected `application/json` or `text/event-stream`): {error}"
                ),
            )
        })
    }
}

/// Drain a `text/event-stream` POST response, returning the first JSON-RPC
/// response whose id matches. Non-matching events (notifications, progress)
/// are skipped. Streaming with early return is preserved (the spec lets a
/// server keep the stream open past the response); bytes read are capped to
/// bound memory.
async fn read_sse_stream<TResult: DeserializeOwned>(
    response: reqwest::Response,
    id: JsonRpcId,
    max_bytes: usize,
) -> io::Result<JsonRpcResponse<TResult>> {
    let mut stream = response.bytes_stream();
    let mut parser = IncrementalSseParser::new();
    let mut read = 0usize;
    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                read += bytes.len();
                if read > max_bytes {
                    return Err(io::Error::other(format!(
                        "MCP HTTP SSE stream exceeded {max_bytes} bytes"
                    )));
                }
                for event in parser.push_chunk(&bytes) {
                    if let Ok(response) =
                        serde_json::from_str::<JsonRpcResponse<TResult>>(&event.data)
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
                    if let Ok(response) =
                        serde_json::from_str::<JsonRpcResponse<TResult>>(&event.data)
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

/// Read the full response body into a `Vec<u8>`, bounding memory by `max_bytes`.
async fn read_limited(response: reqwest::Response, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(reqwest_error_to_io)?;
        if buf.len() + chunk.len() > max_bytes {
            return Err(io::Error::other(format!(
                "MCP HTTP response exceeded {max_bytes} bytes"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
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
        let mut builder =
            Response::builder().status(StatusCode::from_u16(resp.status).expect("valid status"));
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
            request_headers[1]
                .get("accept")
                .expect("accept sent")
                .to_str()
                .expect("ascii"),
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

    /// High-1 (via the HTTP path): multibyte UTF-8 carried in an SSE event
    /// survives byte-level parsing.
    #[tokio::test]
    async fn http_sse_preserves_multibyte_content() {
        // tools/list response whose tool name is CJK.
        let list = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"中文工具","description":"描述","inputSchema":{"type":"object"}}]}}"#;
        let sse_body = format!("data: {list}\n\n");
        let (addr, _state) = spawn_mock_http(vec![MockResp {
            status: 200,
            headers: vec![("content-type", "text/event-stream")],
            body: sse_body,
        }])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-cjk")
            .await
            .expect("connect");
        let tools = connection
            .list_tools(JsonRpcId::Number(1), None)
            .await
            .expect("list tools");
        assert_eq!(tools.result.expect("list result").tools[0].name, "中文工具");
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

    /// High-2: a redirect must NOT be followed (credentials must not leak).
    #[tokio::test]
    async fn http_redirect_is_not_followed() {
        let (addr, _state) = spawn_mock_http(vec![MockResp {
            status: 307,
            headers: vec![("location", "http://attacker.example/leak")],
            body: String::new(),
        }])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-redirect")
            .await
            .expect("connect");
        let error = connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await
            .expect_err("307 must surface as an error, not be followed");
        assert!(error.to_string().contains("307"), "error was: {error}");
    }

    /// Medium-3: response body beyond the cap is rejected.
    #[tokio::test]
    async fn http_read_limited_enforces_byte_cap() {
        let (addr, _state) = spawn_mock_http(vec![MockResp {
            status: 200,
            headers: vec![("content-type", "application/json")],
            body: "0".repeat(100),
        }])
        .await;
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/mcp"))
            .body("")
            .send()
            .await
            .expect("request");
        let error = read_limited(response, 10)
            .await
            .expect_err("body over cap must error");
        assert!(error.to_string().contains("10 bytes"), "error was: {error}");
    }

    /// Low-5: only the initialize response assigns/refreshes the session id.
    #[tokio::test]
    async fn http_session_id_not_refreshed_after_initialize() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1.0"}}}"#;
        let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
        let (addr, _state) = spawn_mock_http(vec![
            MockResp {
                status: 200,
                headers: vec![
                    ("content-type", "application/json"),
                    ("mcp-session-id", "initial"),
                ],
                body: init.to_string(),
            },
            // A later response tries to "refresh" the session id — must be ignored.
            MockResp {
                status: 200,
                headers: vec![
                    ("content-type", "application/json"),
                    ("mcp-session-id", "should-be-ignored"),
                ],
                body: list.to_string(),
            },
        ])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-sess")
            .await
            .expect("connect");
        connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await
            .expect("initialize");
        assert_eq!(connection.session_id.as_deref(), Some("initial"));
        connection
            .list_tools(JsonRpcId::Number(2), None)
            .await
            .expect("list tools");
        // Unchanged — the post-initialize Mcp-Session-Id was NOT applied.
        assert_eq!(connection.session_id.as_deref(), Some("initial"));
    }

    /// Low-4: when the JSON branch fails, the error names the actual
    /// Content-Type so a mislabeled/missing type is diagnosable.
    #[tokio::test]
    async fn http_json_parse_error_names_content_type() {
        let (addr, _state) = spawn_mock_http(vec![MockResp {
            status: 200,
            // No Content-Type; body is not JSON.
            headers: vec![],
            body: "not-json".to_string(),
        }])
        .await;

        let mut connection = McpHttpConnection::connect(&transport(addr), "mock-ct")
            .await
            .expect("connect");
        let error = connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await
            .expect_err("non-JSON body must error");
        let msg = error.to_string();
        assert!(msg.contains("Content-Type"), "error was: {msg}");
    }
}
