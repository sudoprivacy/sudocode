//! Minimal OpenAI-compatible `/chat/completions` mock — just enough to stand
//! in for sudorouter in tests that exercise the `vlm_describe` side-call.
//!
//! Mirrors the raw-tokio pattern from `mock-anthropic-service` (no axum, no
//! cross-crate dep). Captures every request body so tests can assert that
//! the cli sent what it should have.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct CapturedVlmRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub raw_body: String,
}

pub struct OpenAiCompatMock {
    base_url: String,
    requests: Arc<Mutex<Vec<CapturedVlmRequest>>>,
    description: Arc<Mutex<String>>,
    shutdown: Option<oneshot::Sender<()>>,
    join_handle: JoinHandle<()>,
}

impl OpenAiCompatMock {
    /// Spin up on a random local port. The mock will reply to any
    /// `POST /chat/completions` (any prefix path is fine) with a fixed
    /// description in the OpenAI-compatible shape.
    pub async fn spawn(description: impl Into<String>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let description = Arc::new(Mutex::new(description.into()));
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

        let req_state = Arc::clone(&requests);
        let desc_state = Arc::clone(&description);
        let join_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let req_state = Arc::clone(&req_state);
                        let desc_state = Arc::clone(&desc_state);
                        tokio::spawn(async move {
                            let _ = handle_connection(socket, req_state, desc_state).await;
                        });
                    }
                }
            }
        });

        Ok(Self {
            base_url: format!("http://{address}/v1"),
            requests,
            description,
            shutdown: Some(shutdown_tx),
            join_handle,
        })
    }

    /// Sudorouter-shaped base URL (ends in `/v1`); ready to drop into
    /// `sudocode.json` under `auth_modes.proxy.sudorouter.baseUrl`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn captured_requests(&self) -> Vec<CapturedVlmRequest> {
        self.requests.lock().await.clone()
    }
}

impl Drop for OpenAiCompatMock {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.join_handle.abort();
    }
}

async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    requests: Arc<Mutex<Vec<CapturedVlmRequest>>>,
    description: Arc<Mutex<String>>,
) -> io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = Vec::new();
    let mut headers_end: Option<usize> = None;
    loop {
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        total.extend_from_slice(&buf[..n]);
        if let Some(p) = find_double_crlf(&total) {
            headers_end = Some(p + 4);
            break;
        }
    }
    let head_end = headers_end.expect("header end set above");
    let head_str = std::str::from_utf8(&total[..head_end]).unwrap_or("");
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    for h in lines {
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut body = total[head_end..].to_vec();
    while body.len() < content_length {
        let n = socket.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let raw_body = String::from_utf8_lossy(&body).to_string();

    let auth = headers.get("authorization").cloned();
    requests.lock().await.push(CapturedVlmRequest {
        method,
        path,
        authorization: auth,
        raw_body,
    });

    let desc = description.lock().await.clone();
    let response_body = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": 0,
        "model": "mock-vlm",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": desc },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20 }
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

fn find_double_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|w| w == b"\r\n\r\n")
}
