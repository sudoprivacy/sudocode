//! MCP WebSocket transport (2025-03-26 spec).
//!
//! Wire shape: the client opens a persistent WebSocket connection to the MCP
//! endpoint. JSON-RPC requests are sent as text frames and JSON-RPC responses
//! arrive as text frames on the same connection. Notifications (e.g.
//! `notifications/progress`) may arrive interleaved between the request and its
//! matching response. This module implements that shape on top of
//! `tokio-tungstenite` and exposes it through the
//! [`McpConnection`](crate::McpConnection) trait so
//! [`McpServerManager`](crate::mcp_server_manager::McpServerManager) drives it
//! exactly like the stdio, SSE, and HTTP transports.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use crate::mcp_client::McpRemoteTransport;
use crate::mcp_connection::McpConnection;
use crate::mcp_remote::resolve_headers;
use crate::mcp_server_manager::{
    JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpGetPromptParams,
    McpGetPromptResult, McpInitializeParams, McpInitializeResult, McpListPromptsParams,
    McpListPromptsResult, McpListResourcesParams, McpListResourcesResult, McpListToolsParams,
    McpListToolsResult, McpProgressNotification, McpReadResourceParams, McpReadResourceResult,
    McpToolCallParams, McpToolCallResult,
};

/// Type alias for the writer half of the WebSocket stream. The reader half
/// lives in a background task that forwards text frames to an mpsc channel.
type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// A live MCP WebSocket connection: a persistent WebSocket driven as a
/// split sink + background reader task that multiplexes text frames into an
/// mpsc channel for ordered consumption.
pub struct McpWsConnection {
    sink: Mutex<WsSink>,
    events: Mutex<tokio::sync::mpsc::Receiver<String>>,
    closed: Arc<AtomicBool>,
    read_task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for McpWsConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpWsConnection")
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl McpWsConnection {
    /// Open a WebSocket connection to `transport.url`, injecting resolved
    /// headers (static + `headersHelper`). The connection is split into a
    /// writer (held in `sink`) and a reader (driven in a background task that
    /// feeds an mpsc channel). Ping frames are answered automatically by
    /// tungstenite; Close frames set the `closed` flag so `has_exited` reports
    /// true.
    pub(crate) async fn connect(
        transport: &McpRemoteTransport,
        server_name: &str,
    ) -> io::Result<Self> {
        let headers = resolve_headers(transport, server_name).await;

        let mut request = transport.url.as_str().into_client_request().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid MCP WebSocket url `{}`: {e}", transport.url),
            )
        })?;

        // Inject resolved headers into the upgrade request.
        let req_headers = request.headers_mut();
        for (key, value) in &headers {
            if let (Ok(name), Ok(val)) = (
                key.parse::<tokio_tungstenite::tungstenite::http::HeaderName>(),
                value.parse::<tokio_tungstenite::tungstenite::http::HeaderValue>(),
            ) {
                req_headers.insert(name, val);
            }
        }

        let (ws, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("MCP WebSocket connect to `{}` failed: {e}", transport.url),
                )
            })?;

        let (sink, stream) = ws.split();
        let (sender, receiver) = tokio::sync::mpsc::channel::<String>(64);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_task = closed.clone();

        let read_task = tokio::spawn(async move {
            let mut stream = stream;
            while let Some(msg_result) = stream.next().await {
                match msg_result {
                    Ok(Message::Text(text)) => {
                        if sender.send(text.to_string()).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) => {
                        closed_task.store(true, Ordering::Relaxed);
                        break;
                    }
                    // Ping/Pong are handled automatically by tungstenite.
                    Ok(
                        Message::Ping(_)
                        | Message::Pong(_)
                        | Message::Binary(_)
                        | Message::Frame(_),
                    ) => {
                        continue;
                    }
                    Err(_) => {
                        closed_task.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
            closed_task.store(true, Ordering::Relaxed);
        });

        Ok(Self {
            sink: Mutex::new(sink),
            events: Mutex::new(receiver),
            closed,
            read_task,
        })
    }

    /// Send a JSON-RPC request as a WebSocket text frame and read the matching
    /// response. Non-matching frames (notifications, progress, responses with
    /// different ids) are handled appropriately until the matching response
    /// arrives.
    async fn request<TParams, TResult>(
        &self,
        method: &str,
        id: JsonRpcId,
        params: Option<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>>
    where
        TParams: Serialize,
        TResult: DeserializeOwned,
    {
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let json = serde_json::to_string(&request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        {
            let mut sink = self.sink.lock().await;
            sink.send(Message::Text(json.into())).await.map_err(|e| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("MCP WebSocket send failed: {e}"),
                )
            })?;
        }

        let mut events = self.events.lock().await;
        loop {
            let text = events.recv().await.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "MCP WebSocket closed before response",
                )
            })?;

            // Try as the expected response first.
            if let Ok(response) = serde_json::from_str::<JsonRpcResponse<TResult>>(&text) {
                if response.id == id {
                    return Ok(response);
                }
            }

            // Try as a notification (progress).
            if let Ok(notification) = serde_json::from_str::<JsonRpcNotification>(&text) {
                if notification.method == "notifications/progress" {
                    if let Some(params) = notification.params {
                        if let Ok(progress) =
                            serde_json::from_value::<McpProgressNotification>(params)
                        {
                            crate::mcp_server_manager::emit_mcp_progress(progress);
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl McpConnection for McpWsConnection {
    async fn initialize(
        &mut self,
        id: JsonRpcId,
        params: McpInitializeParams,
    ) -> io::Result<JsonRpcResponse<McpInitializeResult>> {
        self.request("initialize", id, Some(params)).await
    }

    async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>> {
        self.request("tools/list", id, params).await
    }

    async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>> {
        self.request("tools/call", id, Some(params)).await
    }

    async fn list_resources(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListResourcesParams>,
    ) -> io::Result<JsonRpcResponse<McpListResourcesResult>> {
        self.request("resources/list", id, params).await
    }

    async fn read_resource(
        &mut self,
        id: JsonRpcId,
        params: McpReadResourceParams,
    ) -> io::Result<JsonRpcResponse<McpReadResourceResult>> {
        self.request("resources/read", id, Some(params)).await
    }

    async fn list_prompts(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListPromptsParams>,
    ) -> io::Result<JsonRpcResponse<McpListPromptsResult>> {
        self.request("prompts/list", id, params).await
    }

    async fn get_prompt(
        &mut self,
        id: JsonRpcId,
        params: McpGetPromptParams,
    ) -> io::Result<JsonRpcResponse<McpGetPromptResult>> {
        self.request("prompts/get", id, Some(params)).await
    }

    async fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.closed.load(Ordering::Relaxed))
    }

    async fn shutdown(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        // Send a close frame, best-effort.
        if let Ok(mut sink) = self.sink.try_lock() {
            let _ = sink.send(Message::Close(None)).await;
        }
        self.read_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::McpClientAuth;
    use crate::mcp_server_manager::{McpInitializeClientInfo, McpInitializeParams};
    use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use std::collections::BTreeMap;
    use std::sync::Arc;

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

    fn transport(addr: std::net::SocketAddr) -> McpRemoteTransport {
        McpRemoteTransport {
            url: format!("ws://{addr}/ws"),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        }
    }

    /// Spawn a mock MCP WebSocket server that echoes canned JSON-RPC
    /// responses in order for each incoming text frame.
    async fn spawn_mock_ws(responses: Vec<String>) -> std::net::SocketAddr {
        let responses = Arc::new(tokio::sync::Mutex::new(responses));
        let app = Router::new().route(
            "/ws",
            get({
                let responses = responses.clone();
                move |ws: WebSocketUpgrade| {
                    let responses = responses.clone();
                    async move {
                        ws.on_upgrade(move |socket| handle_ws(socket, responses))
                            .into_response()
                    }
                }
            }),
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

    async fn handle_ws(mut socket: WebSocket, responses: Arc<tokio::sync::Mutex<Vec<String>>>) {
        while let Some(Ok(msg)) = socket.recv().await {
            match msg {
                AxumMessage::Text(_) => {
                    let mut resps = responses.lock().await;
                    if resps.is_empty() {
                        break;
                    }
                    let resp = resps.remove(0);
                    if socket.send(AxumMessage::Text(resp.into())).await.is_err() {
                        break;
                    }
                }
                AxumMessage::Close(_) => break,
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn ws_initializes_lists_and_calls() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock-ws","version":"0.1.0"}}}"#;
        let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}}"#;
        let call = r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echo:hi"}],"isError":false}}"#;

        let addr = spawn_mock_ws(vec![init.to_string(), list.to_string(), call.to_string()]).await;

        let mut connection = McpWsConnection::connect(&transport(addr), "mock-ws")
            .await
            .expect("connect");

        let initialized = connection
            .initialize(JsonRpcId::Number(1), init_params())
            .await
            .expect("initialize");
        assert_eq!(initialized.id, JsonRpcId::Number(1));
        assert_eq!(
            initialized.result.expect("init result").server_info.name,
            "mock-ws"
        );

        let tools = connection
            .list_tools(JsonRpcId::Number(2), None)
            .await
            .expect("list tools");
        assert_eq!(tools.result.expect("list result").tools.len(), 1);

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
    async fn ws_handles_interleaved_notifications() {
        let progress = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"tok","progress":0.5,"total":1.0}}"#;
        let call = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"done"}],"isError":false}}"#;

        // The mock server sends a progress notification before the response.
        let _addr = spawn_mock_ws(vec![progress.to_string(), call.to_string()]).await;

        // Override the handler to send both messages for a single request.
        let responses = Arc::new(tokio::sync::Mutex::new(vec![
            progress.to_string(),
            call.to_string(),
        ]));
        let app = Router::new().route(
            "/ws",
            get({
                let responses = responses.clone();
                move |ws: WebSocketUpgrade| {
                    let responses = responses.clone();
                    async move {
                        ws.on_upgrade(move |mut socket| async move {
                            if let Some(Ok(AxumMessage::Text(_))) = socket.recv().await {
                                let mut resps = responses.lock().await;
                                // Send all queued responses for this single request.
                                while !resps.is_empty() {
                                    let resp = resps.remove(0);
                                    if socket.send(AxumMessage::Text(resp.into())).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        })
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr2 = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let transport2 = McpRemoteTransport {
            url: format!("ws://{addr2}/ws"),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let mut connection = McpWsConnection::connect(&transport2, "mock-ws-progress")
            .await
            .expect("connect");

        let result = connection
            .call_tool(
                JsonRpcId::Number(1),
                McpToolCallParams {
                    name: "test".to_string(),
                    arguments: None,
                    meta: None,
                },
            )
            .await
            .expect("call tool with interleaved notifications");
        assert!(!result.result.expect("call result").is_error.unwrap_or(true));
    }

    #[tokio::test]
    async fn ws_has_exited_reports_closed() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"mock","version":"0.1.0"}}}"#;
        let addr = spawn_mock_ws(vec![init.to_string()]).await;

        let mut connection = McpWsConnection::connect(&transport(addr), "mock-ws-exit")
            .await
            .expect("connect");
        assert!(!connection.has_exited().await.expect("has_exited"));

        connection.shutdown().await;
        assert!(connection
            .has_exited()
            .await
            .expect("has_exited after shutdown"));
    }

    #[tokio::test]
    async fn ws_connect_failure_surfaces_error() {
        let transport = McpRemoteTransport {
            url: "ws://127.0.0.1:1/nonexistent".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let result = McpWsConnection::connect(&transport, "bad-ws").await;
        assert!(result.is_err());
    }
}
