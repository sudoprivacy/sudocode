//! WebSocket-based ACP server using axum.
//!
//! Adapts an axum `WebSocket` into the SDK's `Lines` transport so the same
//! handler chain used for stdio serves WebSocket connections. This gives WS
//! full feature parity (including elicitation/permission prompting) for free.

use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;

use crate::acp_sdk_server::{
    new_abort_registry, run_acp_on_transport, AbortRegistry, SdkAcpConfig, SdkAcpDelegate,
    SharedDelegate,
};

static WEB_UI_HTML: &str = include_str!("acp_web_ui.html");

#[derive(Clone)]
struct AppState {
    config: SdkAcpConfig,
    delegate: SharedDelegate,
    abort_registry: AbortRegistry,
}

/// Run an ACP server over WebSocket + serve the embedded web UI.
///
/// # Errors
///
/// Returns an error if the TCP listener or axum server fails.
pub async fn run_acp_ws_server(
    config: SdkAcpConfig,
    delegate: Box<dyn SdkAcpDelegate>,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = AppState {
        config,
        delegate: Arc::new(Mutex::new(delegate)),
        abort_registry: new_abort_registry(),
    };
    let app = Router::new()
        .route("/", get(serve_html))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!("[acp-ws] listening on http://0.0.0.0:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_html() -> impl IntoResponse {
    Html(WEB_UI_HTML)
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: WebSocket, state: AppState) {
    eprintln!("[acp-ws] client connected");

    let (sink, stream) = socket.split();

    // Adapt Stream<Item=Result<Message, axum::Error>> -> Stream<Item=io::Result<String>>
    let incoming: std::pin::Pin<Box<dyn futures::Stream<Item = std::io::Result<String>> + Send>> =
        Box::pin(stream.filter_map(|result| async {
            match result {
                Ok(Message::Text(t)) => Some(Ok(t.to_string())),
                _ => None,
            }
        }));

    // Adapt Sink<Message, Error=axum::Error> -> Sink<String, Error=io::Error>
    let outgoing: std::pin::Pin<
        Box<dyn futures::Sink<String, Error = std::io::Error> + Send>,
    > = Box::pin(
        sink.sink_map_err(|e| std::io::Error::other(e.to_string()))
            .with(|line: String| async move {
                Ok::<_, std::io::Error>(Message::Text(line.into()))
            }),
    );

    let transport = agent_client_protocol::Lines::new(outgoing, incoming);

    if let Err(e) = run_acp_on_transport(
        &state.config,
        Arc::clone(&state.delegate),
        Arc::clone(&state.abort_registry),
        transport,
    )
    .await
    {
        eprintln!("[acp-ws] transport error: {e}");
    }

    eprintln!("[acp-ws] client disconnected");
}
