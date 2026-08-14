//! Regression tests for the three API reliability holes:
//! 1. no connect/read timeout — a dead connection hung the session forever,
//! 2. transient gateway errors delivered as HTTP 400 were never retried,
//! 3. non-SSE error responses (HTML pages, bare JSON) were silently dropped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use api::{
    AnthropicClient, InputContentBlock, InputMessage, MessageRequest, OpenAiCompatClient,
    OpenAiCompatConfig, ProxyConfig, SseParser, TimeoutConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Read an entire HTTP request (headers + content-length body) so the client
/// never sees the connection close mid-write (which would surface as a
/// transport error and muddy retry counting). Returns the request line.
async fn read_full_request(sock: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let request_line = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let Ok(n) = sock.read(&mut chunk).await else {
            return request_line(&buf);
        };
        if n == 0 {
            return request_line(&buf);
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(header_end) = buf
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        else {
            continue;
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if buf.len() >= header_end + content_length {
            return request_line(&buf);
        }
    }
}

fn http_response(status: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Spawn a server that answers each `POST /v1/messages` with the next queued
/// response (repeating the last one) and answers every other path (e.g. the
/// `count_tokens` preflight) with 404. Returns the bound address and a counter
/// of `/v1/messages` hits.
async fn spawn_scripted_server(responses: Vec<String>) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    spawn_scripted_server_for_path("POST /v1/messages ", responses).await
}

/// Like [`spawn_scripted_server`] but counting hits on an arbitrary
/// request-line prefix (e.g. the OpenAI-compat `POST /chat/completions `).
async fn spawn_scripted_server_for_path(
    request_line_prefix: &'static str,
    responses: Vec<String>,
) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_hits = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let hits = server_hits.clone();
            let responses = responses.clone();
            tokio::spawn(async move {
                let request_line = read_full_request(&mut sock).await;
                let response = if request_line.starts_with(request_line_prefix) {
                    let hit = hits.fetch_add(1, Ordering::SeqCst);
                    responses[hit.min(responses.len() - 1)].clone()
                } else {
                    http_response("404 Not Found", "text/plain", "not found")
                };
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    (addr, hits)
}

fn sample_request() -> MessageRequest {
    MessageRequest {
        model: "claude-sonnet-4-6".to_string(),
        max_tokens: 16,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "hi".to_string(),
            }],
        }],
        ..Default::default()
    }
}

/// Fix 1: with a read timeout configured, a request to a server that accepts
/// the TCP connection but never responds fails with a timeout error instead
/// of hanging the session forever.
#[tokio::test]
async fn read_timeout_unblocks_request_to_silent_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let _hold = sock;
                std::future::pending::<()>().await;
            });
        }
    });

    let client = api::build_http_client_with_opts(
        &ProxyConfig::default(),
        &TimeoutConfig::from_seconds(5, 1),
    )
    .expect("client builds");
    let pending = client
        .post(format!("http://{addr}/v1/messages"))
        .json(&serde_json::json!({"model": "m", "max_tokens": 1}))
        .send();

    let result = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("request must fail fast instead of hanging");
    let error = result.expect_err("silent server must produce an error");
    assert!(
        error.is_timeout(),
        "error should be a timeout, got: {error:?}"
    );
}

/// Fix 2: HTTP 400 carrying a transient gateway body is retried and can
/// recover on the next attempt.
#[tokio::test]
async fn transient_gateway_400_is_retried_until_success() {
    let (addr, hits) = spawn_scripted_server(vec![
        http_response(
            "400 Bad Request",
            "text/plain",
            "HTTP 400 from backend (no parseable body)",
        ),
        http_response(
            "200 OK",
            "application/json",
            "{\"id\":\"msg_recovered\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"Recovered\"}],\"model\":\"claude-sonnet-4-6\",\"stop_reason\":\"end_turn\",\"stop_sequence\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}",
        ),
    ])
    .await;

    let client = AnthropicClient::new("test-key")
        .with_base_url(format!("http://{addr}"))
        .with_retry_policy(2, Duration::from_millis(1), Duration::from_millis(2));

    let response = client
        .send_message(&sample_request(), None)
        .await
        .expect("transient gateway 400 should be retried into success");
    assert_eq!(response.id, "msg_recovered");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "one failed attempt plus one successful retry"
    );
}

/// Fix 2 guard: a genuine client-error 400 (bad request, unknown model,
/// oversized prompt) is still not retried.
#[tokio::test]
async fn genuine_client_error_400_is_not_retried() {
    let (addr, hits) = spawn_scripted_server(vec![http_response(
        "400 Bad Request",
        "application/json",
        "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"model: field required\"}}",
    )])
    .await;

    let client = AnthropicClient::new("test-key")
        .with_base_url(format!("http://{addr}"))
        .with_retry_policy(2, Duration::from_millis(1), Duration::from_millis(2));

    let error = client
        .send_message(&sample_request(), None)
        .await
        .expect_err("genuine 400 should fail");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "client errors get exactly one attempt"
    );
    assert!(!error.is_retryable());
    assert!(error.to_string().contains("model: field required"));
}

/// Fix 1 config surface: explicit seconds, zero-disables, and env parsing
/// (zero disables, garbage falls back to the defaults).
#[test]
fn timeout_config_resolves_seconds_zero_and_env() {
    let config = TimeoutConfig::from_seconds(10, 0);
    assert_eq!(config.connect_timeout, Some(Duration::from_secs(10)));
    assert_eq!(config.read_timeout, None);

    let default = TimeoutConfig::default();
    assert_eq!(default.connect_timeout, Some(Duration::from_secs(30)));
    assert_eq!(default.read_timeout, Some(Duration::from_secs(300)));

    // Env parsing: explicit value, zero-disables, garbage falls back.
    // Serialized against other env-touching tests via the process env itself
    // (this is the only test in this binary touching these variables).
    std::env::set_var("SUDOCODE_API_CONNECT_TIMEOUT", "5");
    std::env::set_var("SUDOCODE_API_READ_TIMEOUT", "0");
    let config = TimeoutConfig::from_env();
    assert_eq!(config.connect_timeout, Some(Duration::from_secs(5)));
    assert_eq!(config.read_timeout, None);

    std::env::set_var("SUDOCODE_API_CONNECT_TIMEOUT", "soon");
    std::env::remove_var("SUDOCODE_API_READ_TIMEOUT");
    let config = TimeoutConfig::from_env();
    assert_eq!(config, TimeoutConfig::default());

    std::env::remove_var("SUDOCODE_API_CONNECT_TIMEOUT");
}

/// Fix 2 (OpenAI-compat path): the same transient gateway 400 is retried.
#[tokio::test]
async fn openai_transient_gateway_400_is_retried_until_success() {
    let (addr, _hits) = spawn_scripted_server_for_path(
        "POST /chat/completions ",
        vec![
            http_response(
                "400 Bad Request",
                "text/plain",
                "HTTP 400 from backend (no parseable body)",
            ),
            http_response(
                "200 OK",
                "application/json",
                "{\"id\":\"chatcmpl_ok\",\"model\":\"grok-3\",\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"Recovered\",\"tool_calls\":[]},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}",
            ),
        ],
    )
    .await;

    let client = OpenAiCompatClient::new("xai-test-key", OpenAiCompatConfig::xai())
        .with_base_url(format!("http://{addr}"))
        .with_retry_policy(2, Duration::from_millis(1), Duration::from_millis(2));

    let mut request = sample_request();
    request.model = "grok-3".to_string();
    let response = client
        .send_message(&request, None)
        .await
        .expect("transient gateway 400 should be retried into success");
    assert_eq!(response.total_tokens(), 5);
}

/// Fix 3 (OpenAI-compat path): a 200 response carrying an HTML body instead
/// of an SSE stream surfaces an error — including when the body has no SSE
/// frame separator at all (exercises the end-of-stream parser flush).
#[tokio::test]
async fn openai_streaming_html_response_surfaces_error() {
    let (addr, _hits) = spawn_scripted_server_for_path(
        "POST /chat/completions ",
        vec![http_response(
            "200 OK",
            "text/html",
            "<html><head><title>504 Gateway Time-out</title></head><body>nginx</body></html>",
        )],
    )
    .await;

    let client = OpenAiCompatClient::new("xai-test-key", OpenAiCompatConfig::xai())
        .with_base_url(format!("http://{addr}"));

    let mut request = sample_request();
    request.model = "grok-3".to_string();
    request.stream = true;
    let mut stream = client
        .stream_message(&request, None)
        .await
        .expect("HTTP handshake succeeds; the body is the problem");

    let error = loop {
        match stream.next_event().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("stream ended silently; the HTML error was swallowed"),
            Err(error) => break error,
        }
    };
    assert!(
        error.to_string().contains("504 Gateway Time-out"),
        "error should carry the HTML snippet: {error}"
    );
}

/// Fix 3 parser-level behavior through the public `SseParser` API: HTML and
/// bare JSON error bodies surface errors (mid-stream and at finish), while
/// benign SSE noise stays ignored.
#[test]
fn sse_parser_surfaces_non_sse_bodies_and_ignores_benign_frames() {
    // HTML with a frame separator errors mid-stream.
    let mut parser = SseParser::new().with_context("Anthropic", "claude-sonnet-4-6");
    let error = parser
        .push(b"<html><head><title>502 Bad Gateway</title></head>\n\n<body>nginx</body></html>")
        .expect_err("HTML body must surface an error");
    assert!(
        error.to_string().contains("502 Bad Gateway"),
        "error should carry the HTML snippet: {error}"
    );
    assert!(!error.is_retryable());

    // HTML without any separator errors on finish.
    let mut parser = SseParser::new().with_context("Anthropic", "claude-sonnet-4-6");
    assert!(parser
        .push(b"<!DOCTYPE html><html><body>504 Gateway Time-out</body></html>")
        .expect("no full frame yet")
        .is_empty());
    let error = parser
        .finish()
        .expect_err("trailing HTML must surface an error");
    assert!(error.to_string().contains("504 Gateway Time-out"));

    // A bare JSON error envelope surfaces its type and message.
    let mut parser = SseParser::new().with_context("Anthropic", "claude-sonnet-4-6");
    assert!(parser
        .push(br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
        .expect("no full frame yet")
        .is_empty());
    let error = parser
        .finish()
        .expect_err("bare JSON error must surface an error");
    let rendered = error.to_string();
    assert!(
        rendered.contains("overloaded_error") && rendered.contains("Overloaded"),
        "error should carry type and message: {rendered}"
    );

    // Benign inputs keep their previous behavior: no events, no errors.
    let mut parser = SseParser::new();
    assert!(parser
        .push(b": keepalive\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\ndata: [DONE]\n\n")
        .expect("benign frames parse")
        .is_empty());
    assert!(parser
        .push(b"{\"type\":\"message\"}")
        .expect("JSON without an error member is not an error")
        .is_empty());
    assert!(parser
        .finish()
        .expect("JSON without an error member is not an error")
        .is_empty());
}

/// Fix 3: a 200 response whose body is an HTML error page (instead of an SSE
/// stream) surfaces an error carrying a body snippet, rather than ending the
/// stream silently with no events.
#[tokio::test]
async fn streaming_html_response_surfaces_error() {
    let (addr, _hits) = spawn_scripted_server(vec![http_response(
        "200 OK",
        "text/html",
        "<html><head><title>502 Bad Gateway</title></head><body>nginx</body></html>",
    )])
    .await;

    let client = AnthropicClient::new("test-key").with_base_url(format!("http://{addr}"));

    let mut request = sample_request();
    request.stream = true;
    let mut stream = client
        .stream_message(&request, None)
        .await
        .expect("HTTP handshake succeeds; the body is the problem");

    let error = loop {
        match stream.next_event().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("stream ended silently; the HTML error was swallowed"),
            Err(error) => break error,
        }
    };
    let rendered = error.to_string();
    assert!(
        rendered.contains("502 Bad Gateway"),
        "error should carry the HTML snippet: {rendered}"
    );
}
