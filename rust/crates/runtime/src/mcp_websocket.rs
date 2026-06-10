//! MCP WebSocket transport implementation.
//!
//! Connects to an MCP server over WebSocket with JSON-RPC messages carried
//! directly as text frames.  The subprotocol is set to `"mcp"` per the MCP
//! WebSocket transport specification.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::time::Duration;

use futures::stream::SplitSink;
use futures::SinkExt;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;

use crate::mcp_client::{McpRemoteTransport, DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS};
use crate::mcp_transport::{
    JsonRpcId, JsonRpcRequest, JsonRpcResponse, McpInitializeParams, McpInitializeResult,
    McpListResourcesParams, McpListResourcesResult, McpListToolsParams, McpListToolsResult,
    McpReadResourceParams, McpReadResourceResult, McpToolCallParams, McpToolCallResult,
    McpTransportProcess,
};

pub struct McpWebSocketProcess {
    server_name: String,
    sink: SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>,
    inbound_rx: mpsc::UnboundedReceiver<JsonRpcResponse<JsonValue>>,
    reader_task: tokio::task::JoinHandle<()>,
    pending: HashMap<JsonRpcId, oneshot::Sender<JsonRpcResponse<JsonValue>>>,
    tool_call_timeout_ms: u64,
}

impl fmt::Debug for McpWebSocketProcess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpWebSocketProcess")
            .field("server_name", &self.server_name)
            .field("tool_call_timeout_ms", &self.tool_call_timeout_ms)
            .field("pending_count", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl McpWebSocketProcess {
    /// Connect to the MCP WebSocket server at `transport.url`.
    ///
    /// Sets the `"mcp"` subprotocol, injects a `User-Agent` header and any
    /// static headers from the transport configuration, then starts a
    /// background reader task that parses text frames into `JsonRpcResponse`
    /// messages.
    pub async fn connect(
        server_name: &str,
        transport: &McpRemoteTransport,
        scope: crate::config::ConfigSource,
        workspace_is_trusted: bool,
    ) -> io::Result<Self> {
        let mut request = (&transport.url)
            .into_client_request()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        // Set the MCP subprotocol.
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("mcp"));

        // User-Agent header.
        let version = env!("CARGO_PKG_VERSION");
        request.headers_mut().insert(
            "User-Agent",
            HeaderValue::from_str(&format!("sudocode/{version}")).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("failed to parse User-Agent header value: {e}"),
                )
            })?,
        );

        // Build headers from static config + optional headers_helper.
        let extra_headers = crate::mcp_headers_helper::build_request_headers(
            server_name,
            transport,
            scope,
            workspace_is_trusted,
        )
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        for (name, value) in extra_headers {
            if let Some(name) = name {
                request.headers_mut().insert(name, value);
            }
        }

        let (ws_stream, _response) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, None)
                .await
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("WebSocket connection to server `{server_name}` failed: {e}"),
                    )
                })?;

        let (sink, stream) = ws_stream.split();

        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let owned_server_name = server_name.to_string();
        let reader_task = tokio::spawn(async move {
            read_ws_frames(stream, inbound_tx, &owned_server_name).await;
        });

        Ok(Self {
            server_name: server_name.to_string(),
            sink,
            inbound_rx,
            reader_task,
            pending: HashMap::new(),
            tool_call_timeout_ms: DEFAULT_MCP_TOOL_CALL_TIMEOUT_MS,
        })
    }

    /// Send a JSON-RPC request over the WebSocket and wait for the matching
    /// response to arrive from the reader task.
    async fn rpc<TParams: Serialize, TResult: DeserializeOwned>(
        &mut self,
        id: JsonRpcId,
        method: impl Into<String>,
        params: Option<TParams>,
    ) -> io::Result<JsonRpcResponse<TResult>> {
        let method = method.into();
        let request = JsonRpcRequest::new(id.clone(), method.clone(), params);

        let payload = serde_json::to_string(&request)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Register a oneshot before sending so that the reader task can
        // dispatch the response back to us.
        let (response_tx, mut response_rx) = oneshot::channel();
        self.pending.insert(id.clone(), response_tx);

        // Send the text frame.
        self.sink
            .send(Message::Text(payload.into()))
            .await
            .map_err(|e| {
                self.pending.remove(&id);
                io::Error::new(io::ErrorKind::BrokenPipe, e)
            })?;

        // Drain inbound messages until we receive the response for our id.
        let deadline = Duration::from_millis(self.tool_call_timeout_ms);
        let response = timeout(deadline, async {
            loop {
                // Check if the oneshot was already fulfilled by a buffered
                // inbound message.
                if let Ok(response) = response_rx.try_recv() {
                    return Ok(response);
                }

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
                            "WebSocket inbound stream closed",
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
                .map(|v| serde_json::from_value(v))
                .transpose()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            error: response.error,
        };

        Ok(typed_response)
    }
}

#[async_trait::async_trait]
impl McpTransportProcess for McpWebSocketProcess {
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
        Ok(self.reader_task.is_finished())
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        // Send a close frame to the server.
        let _ = self.sink.send(Message::Close(None)).await;
        self.sink
            .close()
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionAborted, e))?;
        // Abort the reader task.
        self.reader_task.abort();
        let reader_task = std::mem::replace(&mut self.reader_task, tokio::task::spawn(async {}));
        let _ = reader_task.await;
        Ok(())
    }

    fn resolved_tool_call_timeout_ms(&self) -> u64 {
        self.tool_call_timeout_ms
    }
}

/// Background reader task: consumes WebSocket frames and dispatches
/// `JsonRpcResponse` messages into the inbound channel.
async fn read_ws_frames(
    mut stream: futures::stream::SplitStream<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
    inbound_tx: mpsc::UnboundedSender<JsonRpcResponse<JsonValue>>,
    server_name: &str,
) {
    use futures::StreamExt;

    while let Some(frame_result) = stream.next().await {
        match frame_result {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<JsonRpcResponse<JsonValue>>(&text) {
                    Ok(message) => {
                        if inbound_tx.send(message).is_err() {
                            // Receiver dropped — the process has been shut down.
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("WebSocket message parse error from server `{server_name}`: {e}");
                    }
                }
            }
            Ok(Message::Close(_)) => {
                eprintln!("WebSocket connection to server `{server_name}` closed by peer");
                break;
            }
            Ok(_) => {
                // Ignore binary, ping, pong frames.
            }
            Err(e) => {
                eprintln!("WebSocket read error from server `{server_name}`: {e}");
                break;
            }
        }
    }
}
