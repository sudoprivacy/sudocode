//! Exercise real CLI failures from interrupted, empty, refused or rejected requests.
mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::TestEnv;
use serde_json::{json, Value};

#[derive(Clone, Copy)]
enum ResponseKind {
    Interrupted,
    Complete,
    SplitTerminal,
    EmptyThenComplete,
    AlwaysEmpty,
    InvalidRequestWithStatusDigits,
    MissingModel,
    RefusedStream,
    RefusedJson,
    RefusedAfterText,
    RefusedTail,
}

impl ResponseKind {
    const fn is_refused(self) -> bool {
        matches!(
            self,
            Self::RefusedStream | Self::RefusedJson | Self::RefusedAfterText | Self::RefusedTail
        )
    }
}

struct StreamProvider {
    url: String,
    stopped: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
}

impl StreamProvider {
    fn new(kind: ResponseKind) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = stopped.clone();
        let count = requests.clone();
        let worker = thread::spawn(move || {
            'connections: while !stop.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut input = BufReader::new(socket.try_clone().unwrap());
                let mut first = String::new();
                if input.read_line(&mut first).unwrap_or(0) == 0 {
                    continue;
                }
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if input.read_line(&mut line).unwrap_or(0) == 0 {
                        continue 'connections;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                if !first.starts_with("POST /v1/messages ") {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}");
                    continue;
                }
                let index = count.fetch_add(1, Ordering::SeqCst);
                let request: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["stream"], true);
                if let Some(response) = error_response(kind) {
                    let _ = socket.write_all(response.as_bytes());
                    continue;
                }
                let empty = matches!(kind, ResponseKind::AlwaysEmpty)
                    || (matches!(kind, ResponseKind::EmptyThenComplete) && index == 0);
                let body = refusal_body(kind).unwrap_or_else(|| {
                    response_body(!matches!(kind, ResponseKind::Interrupted), empty)
                });
                // Promise bytes that never arrive: reqwest must raise a body
                // read error, instead of accepting ordinary clean HTTP EOF.
                let missing = if matches!(kind, ResponseKind::Interrupted | ResponseKind::Complete)
                {
                    128
                } else {
                    0
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + missing);
                if matches!(kind, ResponseKind::SplitTerminal) {
                    // Real gateways can deliver the logical stop and its final
                    // frame in separate reads. Keep the connection alive while
                    // the CLI processes the first part, as observed live.
                    let split = response.rfind("event: message_stop").unwrap();
                    let _ = socket.write_all(&response.as_bytes()[..split]);
                    let _ = socket.flush();
                    thread::sleep(Duration::from_millis(250));
                    let _ = socket.write_all(&response.as_bytes()[split..]);
                } else {
                    let _ = socket.write_all(response.as_bytes());
                }
            }
        });
        Self {
            url,
            stopped,
            requests,
            worker: Some(worker),
        }
    }
}

impl Drop for StreamProvider {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn error_response(kind: ResponseKind) -> Option<String> {
    let (status, message) = match kind {
        ResponseKind::MissingModel => ("404 Not Found", "selected deployment absent"),
        ResponseKind::InvalidRequestWithStatusDigits => (
            "400 Bad Request",
            "invalid max_tokens; diagnostic id: 404-429-502-503",
        ),
        _ => return None,
    };
    let body = json!({"error":{"type":"invalid_request_error","message":message}}).to_string();
    Some(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()))
}

fn response_body(terminal: bool, empty: bool) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"body-close","type":"message","role":"assistant","model":"sonnet","content":[],"usage":{"input_tokens":12,"output_tokens":0}}}),
    ];
    if !empty {
        events.extend([
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"STREAM_BODY_VERIFIED"}}),
        ]);
        if terminal {
            events.push(json!({"type":"content_block_stop","index":0}));
        }
    }
    if terminal {
        events.extend([
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens": if empty { 0 } else { 8 }}}),
            json!({"type":"message_stop"}),
        ]);
    }
    let mut body = String::new();
    for event in events {
        write!(
            &mut body,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
        .unwrap();
    }
    body
}

fn refusal_body(kind: ResponseKind) -> Option<String> {
    if !kind.is_refused() {
        return None;
    }
    let details = json!({"type":"refusal","category":"cyber","explanation":"The provider declined this request."});
    if matches!(kind, ResponseKind::RefusedJson) {
        return Some(json!({"type":"message","id":"refused-json","role":"assistant","model":"sonnet","content":[],"stop_reason":"refusal","stop_details":details,"usage":{"input_tokens":12,"output_tokens":0}}).to_string());
    }
    let mut body = response_body(false, !matches!(kind, ResponseKind::RefusedAfterText));
    let event = json!({"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":details},"usage":{"input_tokens":12,"output_tokens":0}});
    write!(&mut body, "event: message_delta\ndata: {event}").unwrap();
    if !matches!(kind, ResponseKind::RefusedTail) {
        body.push_str("\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    }
    Some(body)
}

// render takes a closure over screens with different lifetimes.
#[allow(clippy::redundant_closure_for_method_calls)]
fn check_response(kind: ResponseKind) {
    let env = TestEnv::new("stream-body-close");
    if env.is_live() {
        return;
    } // the peer must produce the specified protocol fault
    let provider = StreamProvider::new(kind);
    let path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["auth_modes"]["api-key"]["anthropic"]["baseUrl"] = json!(provider.url);
    std::fs::write(path, config.to_string()).unwrap();
    let log_path = env.workspace_root().join("provider-events.jsonl");
    let mut cli = env.spawn_with_env(
        &[
            "--permission-mode",
            "read-only",
            "--print",
            "Report the stream result",
        ],
        &[("SCODE_LOG_PATH", log_path.to_str().unwrap())],
    );
    cli.set_default_timeout(common::at_least(Duration::from_secs(45)));
    let exit = cli.expect_eof().unwrap();
    // expect_eof waits for the child, not the PTY reader. ConPTY can deliver
    // the final diagnostic after the process has already exited.
    let screen = common::expect_screen(
        &cli,
        |screen| {
            if exit == 0 {
                screen.contains("STREAM_BODY_VERIFIED")
            } else {
                common::screen_contains(screen, "runtime_error:")
            }
        },
        env.timeout(),
        "complete process output",
    );
    if matches!(
        kind,
        ResponseKind::Complete | ResponseKind::SplitTerminal | ResponseKind::EmptyThenComplete
    ) {
        assert_eq!(exit, 0, "{screen}");
        assert!(screen.contains("STREAM_BODY_VERIFIED"), "{screen}");
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            if matches!(kind, ResponseKind::EmptyThenComplete) {
                2
            } else {
                1
            },
            "only a response with no usable content should be regenerated, once"
        );
    } else if matches!(
        kind,
        ResponseKind::InvalidRequestWithStatusDigits | ResponseKind::MissingModel
    ) {
        assert_ne!(exit, 0, "provider error must fail the turn: {screen}");
        let squeezed: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squeezed.contains("apireturned"),
            "the real provider error must reach the CLI: {screen}"
        );
        assert_eq!(
            common::model_unavailable_in_screen(&screen),
            matches!(kind, ResponseKind::MissingModel),
            "availability must follow the HTTP status, not digits in a diagnostic ID: {screen}"
        );
        assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
    } else if kind.is_refused() {
        assert_ne!(exit, 0, "a refused turn cannot succeed: {screen}");
        assert_refusal_screen(&screen);
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            1,
            "a refusal must never be retried"
        );
        assert_refusal_trace(&log_path);
    } else if matches!(kind, ResponseKind::AlwaysEmpty) {
        assert_ne!(
            exit, 0,
            "two empty responses cannot complete a turn: {screen}"
        );
        assert!(
            screen.contains("assistant stream produced no content"),
            "{screen}"
        );
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            2,
            "empty response recovery must be bounded"
        );
    } else {
        assert_ne!(exit, 0, "a partial body cannot complete a turn: {screen}");
        assert!(
            screen.contains("incomplete") || screen.contains("truncated"),
            "{screen}"
        );
    }
}

fn assert_refusal_screen(screen: &str) {
    let squeezed: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(squeezed.contains("providerrefusedtherequest"), "{screen}");
    assert!(squeezed.contains("cyber"), "{screen}");
    assert!(
        squeezed.contains("Theproviderdeclinedthisrequest."),
        "{screen}"
    );
    assert!(
        !screen.contains("assistant stream produced no content"),
        "{screen}"
    );
    assert!(
        squeezed.contains("[error-kind:provider_refusal]"),
        "{screen}"
    );
}

fn assert_refusal_trace(log_path: &std::path::Path) {
    let events: Vec<Value> = std::fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let refusals: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "provider_refusal")
        .collect();
    assert_eq!(refusals.len(), 1, "the report needs one structured refusal");
    assert_eq!(refusals[0]["attributes"]["category"], "cyber");
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "response_usage"
                && event["attributes"]["input_tokens"] == 12),
        "refusal usage must still be recorded"
    );
}

#[test]
fn terminal_response_survives_a_broken_http_body() {
    check_response(ResponseKind::Complete);
}

#[test]
fn logical_terminal_response_completes_before_a_delayed_stop_frame() {
    check_response(ResponseKind::SplitTerminal);
}

#[test]
fn interrupted_content_is_not_a_successful_response() {
    check_response(ResponseKind::Interrupted);
}

#[test]
fn empty_terminal_response_retries_and_returns_the_answer() {
    check_response(ResponseKind::EmptyThenComplete);
}

#[test]
fn repeated_empty_terminal_responses_fail_after_one_retry() {
    check_response(ResponseKind::AlwaysEmpty);
}

#[test]
fn invalid_request_status_digits_do_not_make_a_live_test_skip() {
    check_response(ResponseKind::InvalidRequestWithStatusDigits);
}

#[test]
fn missing_model_http_status_is_recognized_as_unavailable() {
    check_response(ResponseKind::MissingModel);
}

#[test]
fn explicit_stream_refusal_is_reported_without_retry() {
    check_response(ResponseKind::RefusedStream);
}

#[test]
fn explicit_json_refusal_is_reported_without_retry() {
    check_response(ResponseKind::RefusedJson);
}

#[test]
fn explicit_refusal_after_partial_text_cannot_succeed() {
    check_response(ResponseKind::RefusedAfterText);
}

#[test]
fn explicit_refusal_in_unterminated_tail_is_not_retried_as_truncation() {
    check_response(ResponseKind::RefusedTail);
}
