//! Exercise real CLI failures from interrupted, empty, refused or rejected requests.
mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::TestEnv;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy)]
enum ResponseKind {
    Interrupted,
    Complete,
    StopReasonOnly,
    DelayedTerminal,
    StopReasonKeepAlive,
    EmptyThenComplete,
    AlwaysEmpty,
    EmptyThinkingThenComplete,
    AlwaysEmptyThinking,
    InvalidRequestWithStatusDigits,
    MissingModel,
    MissingModelUnavailable,
    MissingModelCodeUnavailable,
    MissingModelCodeOnlyUnavailable,
    OverloadedThenComplete,
    RefusedStream,
    RefusedJson,
    RefusedAfterText,
    RefusedTail,
    UsageAtStart,
    ChatDoneKeepAlive,
    ChatTotalOnly,
    ResponsesDoneKeepAlive,
    NativeHeartbeat,
    ChatHeartbeat,
    ChatAnnotations,
    ResponsesHeartbeat,
    AlwaysHeartbeat,
    HeartbeatAfterOverload,
    ToolThenHeartbeat,
    ToolThenEmptyThenComplete,
    NativePartial,
    ChatPartial,
    ResponsesPartial,
    NativeDropThenContinue,
    ChatDropThenContinue,
    ResponsesDropThenContinue,
    ToolThenDrop,
    NativeSlow,
    ChatSlow,
    ResponsesSlow,
    FiniteHeartbeat,
    SignedCapture,
    GeminiCapture,
    CapturePaced,
    CaptureBurst,
    SplicedAttempts,
    RetryUsageComplete,
}

impl ResponseKind {
    const fn api_format(self) -> Option<&'static str> {
        match self {
            Self::ChatDoneKeepAlive
            | Self::ChatTotalOnly
            | Self::MissingModelCodeUnavailable
            | Self::ChatHeartbeat
            | Self::ChatAnnotations
            | Self::HeartbeatAfterOverload
            | Self::ChatPartial
            | Self::ChatDropThenContinue
            | Self::ChatSlow => Some("openai-completions"),
            Self::ResponsesDoneKeepAlive
            | Self::ResponsesHeartbeat
            | Self::ResponsesPartial
            | Self::ResponsesDropThenContinue
            | Self::ResponsesSlow => Some("openai-responses"),
            _ => None,
        }
    }

    const fn drops_once(self) -> bool {
        matches!(
            self,
            Self::NativeDropThenContinue
                | Self::ChatDropThenContinue
                | Self::ResponsesDropThenContinue
                | Self::ToolThenDrop
        )
    }

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
    terminal_connection_state: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
    worker: Option<thread::JoinHandle<()>>,
}

fn peer_closed(socket: &TcpStream) -> bool {
    match socket.peek(&mut [0]) {
        Ok(0) => true,
        Err(error) => !matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        _ => false,
    }
}

impl StreamProvider {
    fn new(kind: ResponseKind) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stopped = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        // 0: not checked yet; 1: connected when terminal tail is sent; 2: closed.
        let terminal_connection_state = Arc::new(AtomicUsize::new(0));
        let terminal_state = terminal_connection_state.clone();
        let stop = stopped.clone();
        let count = requests.clone();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let Some((first, body)) = read_stream_request(&mut socket) else {
                    continue;
                };
                if !first.starts_with("POST ") {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}");
                    continue;
                }
                let index = count.fetch_add(1, Ordering::SeqCst);
                let request: Value = serde_json::from_slice(&body).unwrap();
                if !matches!(kind, ResponseKind::GeminiCapture) {
                    assert_eq!(request["stream"], true);
                }
                captured.lock().unwrap().push(request.clone());
                if liveness_response(&mut socket, kind, index, &stop) {
                    continue;
                }
                if let Some(response) = error_response(kind, index) {
                    let _ = socket.write_all(response.as_bytes());
                    continue;
                }
                let body = stream_body_for(kind, index);
                // Promise bytes that never arrive: reqwest must raise a body
                // read error, instead of accepting ordinary clean HTTP EOF.
                let missing = if matches!(
                    kind,
                    ResponseKind::Interrupted
                        | ResponseKind::Complete
                        | ResponseKind::StopReasonKeepAlive
                        | ResponseKind::ChatDoneKeepAlive
                        | ResponseKind::ChatTotalOnly
                        | ResponseKind::ResponsesDoneKeepAlive
                ) {
                    128
                } else {
                    0
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + missing);
                write_stream_response(&mut socket, kind, &response, &terminal_state, &stop);
            }
        });
        Self {
            url,
            stopped,
            requests,
            terminal_connection_state,
            bodies,
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

fn error_response(kind: ResponseKind, index: usize) -> Option<String> {
    let (status, error) = match kind {
        ResponseKind::MissingModel => (
            "404 Not Found",
            json!({"type":"invalid_request_error","message":"selected deployment absent"}),
        ),
        ResponseKind::InvalidRequestWithStatusDigits => (
            "400 Bad Request",
            json!({"type":"invalid_request_error","message":"invalid max_tokens; diagnostic id: 404-429-502-503"}),
        ),
        ResponseKind::MissingModelUnavailable => (
            "503 Service Unavailable",
            json!({"type":"model_not_found","message":"selected model is not available"}),
        ),
        ResponseKind::MissingModelCodeUnavailable => (
            "503 Service Unavailable",
            json!({"type":"invalid_request_error","code":"model_not_found","message":"selected model is not available"}),
        ),
        ResponseKind::MissingModelCodeOnlyUnavailable => (
            "503 Service Unavailable",
            json!({"code":"model_not_found","message":"selected model is not available"}),
        ),
        ResponseKind::OverloadedThenComplete if index == 0 => (
            "503 Service Unavailable",
            json!({"type":"overloaded_error","message":"Please retry after a brief wait"}),
        ),
        _ => return None,
    };
    let body = json!({"error":error}).to_string();
    Some(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nRequest-Id: req_provider_error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()))
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

const CAPTURE_TEXT: &str = "STREAM_BODY_VERIFIED\n\ncourse\n\nI will inspect the profile.";

#[inline]
fn padded_capture_body(label: &str, repetitions: usize) -> String {
    let mut body = response_body(true, false);
    let marker = body.find("event: message_delta").unwrap();
    let padding = format!(": {}\n\n", label.repeat(56)).repeat(repetitions);
    body.insert_str(marker, &padding);
    body
}

fn signed_capture_body() -> String {
    let body = response_body(true, false)
        .replace("\"index\":0", "\"index\":1")
        .replace(
            "STREAM_BODY_VERIFIED",
            "STREAM_BODY_VERIFIED\\n\\ncourse\\n\\nI will inspect the profile.",
        );
    let split = body.find("event: content_block_start").unwrap();
    let mut prefix = body[..split].to_string();
    prefix.push_str(": diagnostic heartbeat\r\n\r\n");
    for event in [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"诊断 reasoning"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"diagnostic-signature"}}),
        json!({"type":"content_block_stop","index":0}),
    ] {
        write!(
            &mut prefix,
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
        .unwrap();
    }
    prefix.push_str(&body[split..]);
    prefix
}

fn assert_capture_session(path: &std::path::Path, expected: usize) {
    let session = runtime::Session::load_from_path(path).unwrap();
    let assistants: Vec<_> = session
        .messages()
        .iter()
        .filter(|message| message.role == runtime::MessageRole::Assistant)
        .collect();
    assert_eq!(assistants.len(), expected);
    for message in assistants {
        assert!(message.blocks.iter().any(|block| matches!(block,
            runtime::ContentBlock::Text { text } if text == CAPTURE_TEXT)));
        assert!(message.blocks.iter().any(|block| matches!(block,
            runtime::ContentBlock::Thinking { thinking, signature }
            if thinking == "诊断 reasoning" && signature.as_deref() == Some("diagnostic-signature"))));
    }
}

fn assert_raw_capture_files(directory: &std::path::Path, expected: usize, log: &std::path::Path) {
    let sse: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "gz"))
        .collect();
    assert_eq!(sse.len(), expected);
    let events: Vec<Value> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for path in sse {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(
            decoded,
            signed_capture_body().as_bytes(),
            "retain exact body bytes including heartbeat, UTF-8 and separate signed blocks"
        );
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(path.with_extension("").with_extension("json")).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            metadata["end_reason"].as_str(),
            Some("http_eof" | "body_reader_dropped")
        ));
        assert_eq!(metadata["gzip_complete"], true);
        assert_eq!(metadata["saved_all_observed_bytes"], true);
        assert!(events.iter().any(|event| event["event"] == "request_debug"
            && event["attributes"]["request_id"] == metadata["response"]["request_id"]));
    }
}

#[test]
fn raw_stream_capture_preserves_signed_content_through_resume() {
    let env = TestEnv::new("raw-stream-resume");
    if env.is_live() {
        return;
    } // Deliberately fragmented protocol fixture.
    let provider = StreamProvider::new(ResponseKind::SignedCapture);
    configure_peer(&env, &provider, ResponseKind::SignedCapture);
    let log = env.workspace_root().join("capture-events.jsonl");
    let variables = [("SCODE_LOG_PATH", log.to_str().unwrap())];
    let mut cli = env.spawn_with_env(&["--print", "Report the stream result"], &variables);
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let screen = common::expect_screen(
        &cli,
        |screen| screen.contains("STREAM_BODY_VERIFIED"),
        env.timeout(),
        "recorded response rendered",
    );
    assert!(screen.contains("course"));
    let store = runtime::SessionStore::from_cwd(env.workspace_root()).unwrap();
    let path = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|session| session.message_count > 0)
        .unwrap()
        .path;
    assert_capture_session(&path, 1);
    let session = runtime::Session::load_from_path(&path).unwrap();
    let directory =
        runtime::Session::response_captures_dir_for(&path, &session.session_id).unwrap();
    assert_raw_capture_files(&directory, 1, &log);

    let mut resumed = env.spawn_with_env(&["--resume"], &variables);
    common::expect_input_line_cleared(&resumed, env.timeout(), "resumed capture prompt");
    resumed.send("Continue the stream result\r").unwrap();
    common::expect_screen(
        &resumed,
        |_| {
            runtime::Session::load_from_path(&path).is_ok_and(|session| {
                session
                    .messages()
                    .iter()
                    .filter(|m| m.role == runtime::MessageRole::Assistant)
                    .count()
                    == 2
            })
        },
        env.timeout(),
        "resumed response persisted",
    );
    common::expect_input_line_cleared(&resumed, env.timeout(), "resumed response complete");
    resumed.send("/exit\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);
    assert_capture_session(&path, 2);
    assert_raw_capture_files(&directory, 2, &log);
    let bodies = provider.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    let assistant = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert!(assistant["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|block| block["type"] == "thinking" && block["signature"] == "diagnostic-signature"));
    assert!(assistant["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|block| block["type"] == "text" && block["text"] == CAPTURE_TEXT));
}

#[test]
fn raw_stream_capture_can_be_disabled_or_budget_limited() {
    for configuration in [json!({"enabled":false}), json!({"maxSessionBytes":1})] {
        let env = TestEnv::new("raw-stream-disabled");
        if env.is_live() {
            return;
        }
        let provider = StreamProvider::new(ResponseKind::SignedCapture);
        configure_peer(&env, &provider, ResponseKind::SignedCapture);
        set_response_recording(&env, configuration);
        let mut cli = env.spawn(&["--print", "Report the stream result"]);
        assert_eq!(cli.expect_eof().unwrap(), 0);
        common::expect_screen(
            &cli,
            |screen| screen.contains("STREAM_BODY_VERIFIED"),
            env.timeout(),
            "uncaptured response rendered",
        );
        let (path, directory) = capture_session_paths(&env);
        assert_capture_session(&path, 1);
        assert!(
            !directory.exists() || std::fs::read_dir(directory).unwrap().next().is_none(),
            "disabled or exhausted recording must save no body"
        );
    }
}

#[test]
fn raw_stream_capture_write_failure_preserves_turn() {
    let env = TestEnv::new("raw-stream-write-failure");
    if env.is_live() {
        return;
    }
    let provider = StreamProvider::new(ResponseKind::SignedCapture);
    configure_peer(&env, &provider, ResponseKind::SignedCapture);
    let mut cli = env.spawn(&[]);
    common::expect_input_line_cleared(&cli, env.timeout(), "capture failure prompt");
    let (_, blocked) = capture_session_paths(&env);
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, "preserve-existing-file").unwrap();
    cli.send("Report the stream result\r").unwrap();
    common::expect_screen(
        &cli,
        |screen| screen.contains("STREAM_BODY_VERIFIED"),
        env.timeout(),
        "capture failure must preserve real response",
    );
    common::expect_input_line_cleared(&cli, env.timeout(), "capture failure turn complete");
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(blocked).unwrap(),
        "preserve-existing-file"
    );
}

fn set_response_recording(env: &TestEnv, recording: Value) {
    let path = env.config_home().join("settings.json");
    let mut settings: Value = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => panic!("read fixture settings: {error}"),
    };
    settings["responseRecording"] = recording;
    std::fs::write(path, settings.to_string()).unwrap();
}

fn capture_session_paths(env: &TestEnv) -> (std::path::PathBuf, std::path::PathBuf) {
    let path = runtime::SessionStore::from_cwd(env.workspace_root())
        .unwrap()
        .list_sessions()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .path;
    let session = runtime::Session::load_from_path(&path).unwrap();
    let directory =
        runtime::Session::response_captures_dir_for(&path, &session.session_id).unwrap();
    (path, directory)
}

#[test]
fn raw_stream_capture_keeps_streaming_and_resume_responsive() {
    let mut cases = [
        ("response-log-perf-off", false, 0),
        ("response-log-perf-on", true, 0),
        ("response-log-perf-history", true, 1000),
    ];
    let order = std::env::var("SCODE_TEST_RESPONSE_LOG_PERF_ORDER")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    cases.rotate_left(order % 3);
    if order >= 3 {
        cases.reverse();
    }
    for (label, enabled, old_logs) in cases {
        let env = TestEnv::new_mock(label);
        let provider = StreamProvider::new(ResponseKind::CapturePaced);
        configure_peer(&env, &provider, ResponseKind::CapturePaced);
        set_response_recording(&env, json!({"enabled":enabled}));
        let seed = runtime::Session::new().with_workspace_root(env.workspace_root());
        let session_id = seed.session_id.clone();
        let path = runtime::SessionStore::from_cwd(env.workspace_root())
            .unwrap()
            .create_handle(&seed.session_id)
            .path;
        seed.save_to_path(&path).unwrap();
        let directory =
            runtime::Session::response_captures_dir_for(&path, &seed.session_id).unwrap();
        drop(seed);
        seed_old_response_logs(&directory, old_logs);
        let started = Instant::now();
        let mut cli = env.spawn(&["--resume", &session_id]);
        common::expect_input_line_cleared(&cli, env.timeout(), "resume performance prompt");
        let ready_ms = started.elapsed().as_secs_f64() * 1000.0;
        let sent = Instant::now();
        cli.send("Report the stream result\r").unwrap();
        common::expect_screen(
            &cli,
            |screen| screen.contains("STREAM_BODY_VERIFIED"),
            env.timeout(),
            "first streamed text",
        );
        let first_text_ms = sent.elapsed().as_secs_f64() * 1000.0;
        common::expect_screen(
            &cli,
            |_| {
                runtime::Session::load_from_path(&path).is_ok_and(|session| {
                    session
                        .messages()
                        .iter()
                        .any(|message| message.role == runtime::MessageRole::Assistant)
                })
            },
            env.timeout(),
            "paced response persisted",
        );
        common::expect_input_line_cleared(&cli, env.timeout(), "paced response prompt");
        let response_ms = sent.elapsed().as_secs_f64() * 1000.0;
        assert!(!common::screen_tail(&cli, 12000).contains("SSE_LOG_ONLY_PADDING"));
        let ending = Instant::now();
        cli.send("/exit\r").unwrap();
        assert_eq!(cli.expect_eof().unwrap(), 0);
        let exit_ms = ending.elapsed().as_secs_f64() * 1000.0;
        let bodies: Vec<_> = std::fs::read_dir(&directory)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "gz")
                    && !path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("old-")
            })
            .collect();
        assert_eq!(bodies.len(), usize::from(enabled));
        if let Some(body) = bodies.first() {
            let mut raw = String::new();
            flate2::read::GzDecoder::new(std::fs::File::open(body).unwrap())
                .read_to_string(&mut raw)
                .unwrap();
            assert!(raw.contains("SSE_LOG_ONLY_PADDING"));
            assert!(raw.contains("event: message_stop"));
            let metadata: Value = serde_json::from_slice(
                &std::fs::read(body.with_extension("").with_extension("json")).unwrap(),
            )
            .unwrap();
            assert_eq!(metadata["saved_all_observed_bytes"], true);
        }
        println!(
            "RESPONSE_LOG_PERFORMANCE {}",
            json!({"case":label,"enabled":enabled,"old_logs":old_logs,"ready_ms":ready_ms,"first_text_ms":first_text_ms,"response_ms":response_ms,"exit_ms":exit_ms})
        );
    }
}

#[test]
fn raw_stream_capture_burst_preserves_output_and_reports_any_missing_evidence() {
    let env = TestEnv::new_mock("response-log-burst");
    let provider = StreamProvider::new(ResponseKind::CaptureBurst);
    configure_peer(&env, &provider, ResponseKind::CaptureBurst);
    let mut cli = env.spawn(&["--print", "Report the stream result"]);
    assert_eq!(cli.expect_eof().unwrap(), 0);
    common::expect_screen(
        &cli,
        |screen| screen.contains("STREAM_BODY_VERIFIED"),
        env.timeout(),
        "burst response rendered",
    );
    let (path, directory) = capture_session_paths(&env);
    let session = runtime::Session::load_from_path(&path).unwrap();
    assert!(session.messages().iter().flat_map(|m| &m.blocks).any(|block|
        matches!(block, runtime::ContentBlock::Text { text } if text == "STREAM_BODY_VERIFIED")));
    let body = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "gz"))
        .unwrap();
    let mut saved = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&body).unwrap())
        .read_to_end(&mut saved)
        .unwrap();
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(body.with_extension("").with_extension("json")).unwrap(),
    )
    .unwrap();
    let expected = padded_capture_body("SSE_BURST_CAPTURE_PADDING", 8192);
    assert_eq!(
        saved,
        &expected.as_bytes()[..saved.len()],
        "background batches preserve a contiguous wire prefix"
    );
    assert_eq!(metadata["gzip_complete"], true);
    assert_eq!(metadata["observed_bytes"], expected.len());
    assert_eq!(metadata["saved_bytes"], saved.len());
    if saved.len() == expected.len() {
        assert_eq!(metadata["saved_all_observed_bytes"], true);
        assert!(metadata["capture_error"].is_null());
    } else {
        assert_eq!(metadata["saved_all_observed_bytes"], false);
        assert_eq!(metadata["truncated"], true);
        assert_eq!(
            metadata["capture_error"],
            "response_capture_queue_saturated"
        );
    }
}

#[test]
fn raw_stream_capture_truncation_preserves_complete_assistant() {
    let env = TestEnv::new("raw-stream-truncated");
    if env.is_live() {
        return;
    }
    let provider = StreamProvider::new(ResponseKind::SignedCapture);
    configure_peer(&env, &provider, ResponseKind::SignedCapture);
    set_response_recording(&env, json!({"maxResponseBytes":80}));
    let mut cli = env.spawn(&["--print", "Report the stream result"]);
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let (path, directory) = capture_session_paths(&env);
    assert_capture_session(&path, 1);
    let body = std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "gz"))
        .unwrap();
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&body).unwrap())
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, &signed_capture_body().as_bytes()[..80]);
    let metadata: Value = serde_json::from_slice(
        &std::fs::read(body.with_extension("").with_extension("json")).unwrap(),
    )
    .unwrap();
    assert_eq!(metadata["saved_all_observed_bytes"], false);
    assert_eq!(metadata["gzip_complete"], true);
    assert_eq!(metadata["saved_bytes"], 80);
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

#[test]
fn raw_stream_capture_records_each_streaming_protocol() {
    for (kind, marker) in [
        (ResponseKind::ChatDoneKeepAlive, "data: [DONE]"),
        (
            ResponseKind::ResponsesDoneKeepAlive,
            "event: response.completed",
        ),
        (ResponseKind::GeminiCapture, "\"candidates\""),
    ] {
        let env = TestEnv::new("raw-stream-protocols");
        if env.is_live() {
            return;
        }
        let provider = StreamProvider::new(kind);
        configure_peer(&env, &provider, kind);
        let mut cli = env.spawn(&["--print", "Report the stream result"]);
        assert_eq!(cli.expect_eof().unwrap(), 0);
        common::expect_screen(
            &cli,
            |screen| screen.contains("STREAM_BODY_VERIFIED"),
            env.timeout(),
            "actual adapter response",
        );
        let (_, directory) = capture_session_paths(&env);
        let body = std::fs::read_dir(directory)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "gz"))
            .unwrap();
        let mut raw = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&body).unwrap())
            .read_to_string(&mut raw)
            .unwrap();
        assert!(
            raw.contains(marker),
            "save the real protocol before normalization: {raw}"
        );
        assert!(raw.contains("STREAM_BODY_VERIFIED"));
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(body.with_extension("").with_extension("json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["gzip_complete"], true);
        assert_eq!(metadata["saved_all_observed_bytes"], true);
    }
}

fn configure_peer(env: &TestEnv, provider: &StreamProvider, kind: ResponseKind) {
    let path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["auth_modes"]["api-key"]["anthropic"]["baseUrl"] = json!(provider.url);
    if matches!(kind, ResponseKind::GeminiCapture) {
        config["auth_modes"]["api-key"]["gemini"] =
            json!({"baseUrl":provider.url,"apiKey":"test-pty-key"});
        config["models"]["sonnet"] = json!({"alias":"sonnet","providers":{"api-key":{"provider":"gemini","model":"gemini-2.5-flash"}}});
    }
    if let Some(format) = kind.api_format() {
        config["models"]["sonnet"] = json!({"alias":"sonnet", "providers":{"api-key":{
            "provider":"anthropic", "model":"sonnet", "api":format
        }}});
    }
    std::fs::write(path, config.to_string()).unwrap();
}

#[test]
fn spliced_attempts_preserve_partial_progress_without_mixing_cache_usage() {
    check_response(ResponseKind::SplicedAttempts);
}

#[test]
fn successful_start_with_zero_cache_creation_reaches_session_accounting() {
    check_response(ResponseKind::RetryUsageComplete);
}

// render takes a closure over screens with different lifetimes.
#[allow(clippy::redundant_closure_for_method_calls)]
fn check_response(kind: ResponseKind) {
    let env = TestEnv::new("stream-body-close");
    if env.is_live() {
        return;
    } // the peer must produce the specified protocol fault
    let provider = StreamProvider::new(kind);
    configure_peer(&env, &provider, kind);
    let log_path = env.workspace_root().join("provider-events.jsonl");
    let mut args = vec!["--permission-mode", "read-only"];
    if !matches!(kind, ResponseKind::OverloadedThenComplete) {
        args.push("--print");
    }
    args.push("Report the stream result");
    if matches!(
        kind,
        ResponseKind::ChatTotalOnly | ResponseKind::UsageAtStart
    ) {
        args.extend(["--output-format", "json"]);
    }
    let mut cli = env.spawn_with_env(
        &args,
        &[
            ("SCODE_LOG_PATH", log_path.to_str().unwrap()),
            ("SUDOCODE_API_READ_TIMEOUT", "2"),
        ],
    );
    cli.set_default_timeout(common::at_least(Duration::from_secs(45)));
    let is_missing_model = matches!(
        kind,
        ResponseKind::MissingModelUnavailable
            | ResponseKind::MissingModelCodeUnavailable
            | ResponseKind::MissingModelCodeOnlyUnavailable
    );
    if is_missing_model {
        cli.set_default_timeout(common::at_least(Duration::from_secs(15)));
    }
    if matches!(kind, ResponseKind::OverloadedThenComplete) {
        cli.expect(r"retry 1/8 .* 503")
            .expect("a transient 503 must report its retry");
    }
    let exit = cli.expect_eof().unwrap_or_else(|error| {
        panic!(
            "stream did not complete: {error}; requests={}\n{}",
            provider.requests.load(Ordering::SeqCst),
            common::screen_tail(&cli, 12000)
        )
    });
    if matches!(
        kind,
        ResponseKind::AlwaysEmpty | ResponseKind::AlwaysEmptyThinking
    ) {
        assert_ne!(exit, 0, "two empty responses cannot complete a turn");
    }
    // expect_eof waits for the child, not the PTY reader. ConPTY can deliver
    // the final diagnostic after the process has already exited.
    let screen = common::expect_screen(
        &cli,
        |screen| {
            if exit == 0 {
                screen.contains(
                    if matches!(
                        kind,
                        ResponseKind::SplicedAttempts | ResponseKind::RetryUsageComplete
                    ) {
                        "RETRY_USAGE_SUCCESS"
                    } else {
                        "STREAM_BODY_VERIFIED"
                    },
                )
            } else {
                common::screen_contains(screen, "runtime_error:")
            }
        },
        env.timeout(),
        "complete process output",
    );
    ResponseCheck {
        kind,
        env: &env,
        provider: &provider,
        screen: &screen,
        exit,
        log_path: &log_path,
    }
    .assert_result();
}

fn assert_progress_trace(log_path: &std::path::Path) {
    let events: Vec<Value> = std::fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let request_id = events
        .iter()
        .find(|event| event["event"] == "request_debug")
        .unwrap()["attributes"]["request_id"]
        .as_str()
        .unwrap();
    let rows: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "response_stream_progress")
        .map(|event| &event["attributes"])
        .collect();
    assert!(
        rows.len() >= 3,
        "headers, bytes and completion must be observable"
    );
    assert_eq!(rows[0]["bytes_read"], 0);
    assert_eq!(rows[0]["finished"], false);
    let last = rows.last().unwrap();
    assert_eq!(last["finished"], true);
    assert!(last["bytes_read"].as_u64().unwrap() > 0);
    assert!(last["normalized_events"].as_u64().unwrap() > 0);
    for row in rows {
        assert_eq!(
            row["request_id"], request_id,
            "transport, progress and usage share the client ID"
        );
        assert!(row.get("body").is_none() && row.get("content").is_none());
        assert!(
            !row.to_string().contains("STREAM_BODY_VERIFIED"),
            "progress is metadata only"
        );
    }
    assert!(events.iter().any(|event| event["event"] == "response_usage"
        && event["attributes"]["request_id"] == request_id));
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
fn logical_stop_completes_without_a_coalesced_message_stop() {
    check_response(ResponseKind::StopReasonOnly);
}

#[test]
fn logical_stop_drains_a_delayed_terminal_event_before_releasing_the_response() {
    check_response(ResponseKind::DelayedTerminal);
}

#[test]
fn logical_stop_with_a_nonterminating_body_has_bounded_recovery() {
    check_response(ResponseKind::StopReasonKeepAlive);
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
fn unsigned_empty_thinking_recovers_once_and_returns_the_answer() {
    check_response(ResponseKind::EmptyThinkingThenComplete);
}

#[test]
fn unsigned_empty_thinking_cannot_complete_a_turn_after_recovery() {
    check_response(ResponseKind::AlwaysEmptyThinking);
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

#[test]
fn initial_usage_survives_a_zero_filled_terminal_snapshot() {
    check_response(ResponseKind::UsageAtStart);
}

#[test]
fn chat_done_finishes_without_http_eof_and_preserves_usage() {
    check_response(ResponseKind::ChatDoneKeepAlive);
}

#[test]
fn responses_completed_finishes_without_http_eof_and_preserves_usage() {
    check_response(ResponseKind::ResponsesDoneKeepAlive);
}

#[test]
fn reported_total_survives_print_persistence_and_resume_without_inventing_a_breakdown() {
    check_response(ResponseKind::ChatTotalOnly);
}

#[test]
fn typed_missing_model_503_fails_once_with_the_provider_reason() {
    check_response(ResponseKind::MissingModelUnavailable);
}

#[test]
fn openai_missing_model_code_503_fails_once_with_the_provider_reason() {
    check_response(ResponseKind::MissingModelCodeUnavailable);
}

#[test]
fn code_only_missing_model_503_fails_once_with_the_provider_reason() {
    check_response(ResponseKind::MissingModelCodeOnlyUnavailable);
}

#[test]
fn ordinary_503_retries_visibly_and_completes() {
    check_response(ResponseKind::OverloadedThenComplete);
}

fn liveness_response(
    socket: &mut TcpStream,
    kind: ResponseKind,
    index: usize,
    stop: &AtomicBool,
) -> bool {
    let is_live = kind.is_liveness_fixture();
    if !is_live {
        return false;
    }
    if matches!(kind, ResponseKind::ToolThenEmptyThenComplete) && index > 0 {
        // The normal response fixture sends one terminal empty response and
        // then a final answer, after the first request's real tool execution.
        return false;
    }
    if matches!(kind, ResponseKind::HeartbeatAfterOverload) && index == 0 {
        let body = r#"{"error":{"type":"overloaded_error","message":"temporarily overloaded"}}"#;
        let _ = write!(socket, "HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        return true;
    }
    let tool_turn = matches!(
        kind,
        ResponseKind::ToolThenHeartbeat | ResponseKind::ToolThenEmptyThenComplete
    ) && index == 0;
    // These drop mid-response on the first request only; later requests are
    // the continuation and complete with different text.
    let drops_once = kind.drops_once();
    let tool_then_drop = matches!(kind, ResponseKind::ToolThenDrop) && index == 0;
    let partial = matches!(
        kind,
        ResponseKind::NativePartial | ResponseKind::ChatPartial | ResponseKind::ResponsesPartial
    ) || (drops_once && index == 0);
    let texts = if drops_once && index > 0 {
        ["CONTINUED_", "AFTER_", "DROP"]
    } else {
        ["STREAM_", "BODY_", "VERIFIED"]
    };
    let slow = matches!(
        kind,
        ResponseKind::NativeSlow | ResponseKind::ChatSlow | ResponseKind::ResponsesSlow
    );
    let heartbeat = matches!(
        kind,
        ResponseKind::AlwaysHeartbeat | ResponseKind::FiniteHeartbeat
    ) || (matches!(
        kind,
        ResponseKind::HeartbeatAfterOverload | ResponseKind::ToolThenHeartbeat
    ) && index == 1)
        || (matches!(
            kind,
            ResponseKind::NativeHeartbeat
                | ResponseKind::ChatHeartbeat
                | ResponseKind::ChatAnnotations
                | ResponseKind::ResponsesHeartbeat
        ) && index == 0);
    let (prefix, deltas, tail) = liveness_frames(kind, texts, tool_turn, tool_then_drop);
    let body = format!("{prefix}{}{tail}", deltas.join(""));
    let length = if heartbeat || partial {
        1_000_000
    } else {
        body.len()
    };
    if write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").is_err() { return true; }
    if !heartbeat {
        let _ = socket.write_all(prefix.as_bytes());
        for delta in &deltas {
            if slow {
                thread::sleep(Duration::from_millis(900));
            }
            if socket.write_all(delta.as_bytes()).is_err() {
                return true;
            }
        }
        if !partial {
            let _ = socket.write_all(tail.as_bytes());
        }
    }
    if heartbeat || partial {
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let started = std::time::Instant::now();
        while !stop.load(Ordering::SeqCst) && !peer_closed(socket) {
            if matches!(kind, ResponseKind::FiniteHeartbeat)
                && started.elapsed() >= Duration::from_secs(3)
            {
                let _ = socket.write_all(body.as_bytes());
                break;
            }
            let frame: &[u8] = if matches!(kind, ResponseKind::ChatAnnotations) {
                b"data: {\"id\":\"\",\"model\":\"\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"content_filter_results\":{}}]}\n\n"
            } else {
                b": ping\n\n"
            };
            if socket.write_all(frame).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    true
}

fn check_liveness(
    kind: ResponseKind,
    read_timeout: &str,
    expected_requests: usize,
    is_success: bool,
) {
    let env = TestEnv::new_mock("stream-liveness");
    let provider = StreamProvider::new(kind);
    configure_peer(&env, &provider, kind);
    let log = env.workspace_root().join("stream-events.jsonl");
    let is_tool = matches!(
        kind,
        ResponseKind::ToolThenHeartbeat
            | ResponseKind::ToolThenEmptyThenComplete
            | ResponseKind::ToolThenDrop
    );
    let always_drops = matches!(
        kind,
        ResponseKind::NativePartial | ResponseKind::ChatPartial | ResponseKind::ResponsesPartial
    );
    let continues = always_drops || kind.drops_once();
    let interactive = matches!(
        kind,
        ResponseKind::NativeHeartbeat
            | ResponseKind::ChatHeartbeat
            | ResponseKind::ChatAnnotations
            | ResponseKind::ResponsesHeartbeat
            | ResponseKind::HeartbeatAfterOverload
            | ResponseKind::NativePartial
            | ResponseKind::ChatPartial
            | ResponseKind::ResponsesPartial
            | ResponseKind::NativeDropThenContinue
            | ResponseKind::ChatDropThenContinue
            | ResponseKind::ResponsesDropThenContinue
    );
    let mut args = vec![
        "--permission-mode",
        if is_tool {
            "danger-full-access"
        } else {
            "read-only"
        },
    ];
    if !interactive {
        args.push("--print");
    }
    args.push("Report the stream result");
    let mut cli = env.spawn_with_env(
        &args,
        &[
            ("SUDOCODE_API_READ_TIMEOUT", read_timeout),
            ("SCODE_LOG_PATH", log.to_str().unwrap()),
        ],
    );
    cli.set_default_timeout(common::at_least(Duration::from_secs(30)));
    let code = cli.expect_eof().unwrap_or_else(|error| {
        panic!(
            "stream liveness: {error}; requests={}\n{}",
            provider.requests.load(Ordering::SeqCst),
            common::screen_tail(&cli, 6000)
        )
    });
    assert_eq!(code == 0, is_success, "{}", common::screen_tail(&cli, 6000));
    let actual_requests = provider.requests.load(Ordering::SeqCst);
    let diagnostic = if actual_requests == expected_requests {
        String::new()
    } else {
        liveness_diagnostic(kind, &cli, &log)
    };
    assert_eq!(actual_requests, expected_requests, "{diagnostic}");
    let screen = common::expect_screen(
        &cli,
        |s| {
            common::screen_contains(
                s,
                if kind.drops_once() {
                    "CONTINUED_AFTER_DROP"
                } else if is_success {
                    "STREAM_BODY_VERIFIED"
                } else if interactive {
                    "automatic replay disabled"
                } else {
                    "runtime_error:"
                },
            )
        },
        common::DEFAULT_TIMEOUT,
        "complete liveness result",
    );
    let observed = ResponseCheck {
        kind,
        env: &env,
        provider: &provider,
        screen: &screen,
        exit: code,
        log_path: &log,
    };
    let bodies = provider.bodies.lock().unwrap();
    observed.assert_liveness_replay(&bodies, expected_requests, is_tool, continues);
    observed.assert_liveness_completion(&bodies, is_tool, always_drops, interactive);
}

#[test]
fn heartbeat_only_streams_replay_before_the_first_provider_event() {
    for kind in [
        ResponseKind::NativeHeartbeat,
        ResponseKind::ChatHeartbeat,
        ResponseKind::ChatAnnotations,
        ResponseKind::ResponsesHeartbeat,
    ] {
        check_liveness(kind, "2", 2, true);
    }
}

#[test]
fn dropped_stream_continues_from_the_partial_reply() {
    for kind in [
        ResponseKind::NativeDropThenContinue,
        ResponseKind::ChatDropThenContinue,
        ResponseKind::ResponsesDropThenContinue,
    ] {
        check_liveness(kind, "2", 2, true);
    }
}

#[test]
fn completed_tool_is_not_rerun_when_the_stream_drops_after_it() {
    // The stream must drop after the tool finishes; an unfinished tool is
    // cancelled instead. Leave the tool ample time on a loaded runner.
    check_liveness(ResponseKind::ToolThenDrop, "8", 2, true);
}

#[test]
fn repeated_stream_drops_stop_after_bounded_continuations() {
    for kind in [
        ResponseKind::NativePartial,
        ResponseKind::ChatPartial,
        ResponseKind::ResponsesPartial,
    ] {
        check_liveness(kind, "2", 4, false);
    }
}

#[test]
fn active_provider_events_outlive_the_idle_timeout() {
    for kind in [
        ResponseKind::NativeSlow,
        ResponseKind::ChatSlow,
        ResponseKind::ResponsesSlow,
    ] {
        check_liveness(kind, "2", 1, true);
    }
}

#[test]
fn completed_tool_survives_an_empty_continuation_replay() {
    check_liveness(ResponseKind::ToolThenHeartbeat, "2", 3, true);
}

#[test]
fn empty_tool_continuation_reuses_history_and_finalizes_without_reexecuting_tool() {
    check_liveness(ResponseKind::ToolThenEmptyThenComplete, "2", 3, true);
}

#[test]
fn http_retries_and_stream_replay_share_one_attempt_sequence() {
    check_liveness(ResponseKind::HeartbeatAfterOverload, "2", 3, true);
}

#[test]
fn empty_stream_replay_is_bounded_once() {
    check_liveness(ResponseKind::AlwaysHeartbeat, "2", 2, false);
}

#[test]
fn zero_read_timeout_preserves_a_long_empty_wait() {
    check_liveness(ResponseKind::FiniteHeartbeat, "0", 1, true);
}
struct ResponseCheck<'a> {
    kind: ResponseKind,
    env: &'a TestEnv,
    provider: &'a StreamProvider,
    screen: &'a str,
    exit: u32,
    log_path: &'a std::path::Path,
}

impl ResponseCheck<'_> {
    fn assert_result(&self) {
        if matches!(
            self.kind,
            ResponseKind::SplicedAttempts | ResponseKind::RetryUsageComplete
        ) {
            self.assert_spliced_accounting();
        } else if matches!(
            self.kind,
            ResponseKind::MissingModelUnavailable
                | ResponseKind::MissingModelCodeUnavailable
                | ResponseKind::MissingModelCodeOnlyUnavailable
        ) {
            self.assert_missing_model();
        } else if matches!(
            self.kind,
            ResponseKind::UsageAtStart
                | ResponseKind::ChatDoneKeepAlive
                | ResponseKind::ChatTotalOnly
                | ResponseKind::ResponsesDoneKeepAlive
        ) {
            self.assert_usage_accounting();
        } else {
            self.assert_regular_response();
        }
    }
    fn assert_spliced_accounting(&self) {
        let kind = self.kind;
        let env = self.env;
        let provider = self.provider;
        let screen = self.screen;
        let exit = self.exit;

        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            1,
            "never replay a stream after partial progress escaped"
        );
        let store = runtime::SessionStore::from_cwd(env.workspace_root()).unwrap();
        let path = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|session| session.message_count > 0)
            .unwrap()
            .path;
        let session = runtime::Session::load_from_path(&path).unwrap();
        let assistant = session
            .messages()
            .iter()
            .rev()
            .find(|message| message.role == runtime::MessageRole::Assistant)
            .unwrap();
        let usage = assistant.usage.expect("retain the observed response usage");
        assert_eq!(usage.input_tokens, 2);
        if matches!(kind, ResponseKind::SplicedAttempts) {
            assert_ne!(exit, 0, "a spliced stream must fail: {screen}");
            assert!(
                common::screen_contains(screen, "multiple message_start"),
                "{screen}"
            );
            let saved_text: String = assistant
                .blocks
                .iter()
                .filter_map(|block| match block {
                    runtime::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                saved_text.lines().next(),
                Some("FAILED_ATTEMPT_PARTIAL"),
                "retain A before the runtime's interruption annotation"
            );
            assert!(
                !saved_text.contains("RETRY_USAGE_SUCCESS"),
                "B must never enter saved history"
            );
            assert!(
                !screen.contains("RETRY_USAGE_SUCCESS"),
                "never present B as A: {screen}"
            );
            assert_eq!(usage.cache_creation_input_tokens, 5235);
            assert_eq!(usage.cache_read_input_tokens, 124_730);
            assert_eq!(usage.output_tokens, 0);
        } else {
            assert_eq!(exit, 0, "{screen}");
            assert!(screen.contains("RETRY_USAGE_SUCCESS"), "{screen}");
            assert_eq!(usage.cache_creation_input_tokens, 0);
            assert_eq!(usage.cache_read_input_tokens, 129_965);
            assert_eq!(usage.output_tokens, 1569);
        }
    }
    fn assert_missing_model(&self) {
        let provider = self.provider;
        let screen = self.screen;
        let exit = self.exit;

        assert_ne!(exit, 0, "missing model must fail: {screen}");
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            1,
            "never retry a configuration problem"
        );
        assert!(screen.contains("model_not_found"), "{screen}");
        assert!(screen.contains("req_provider_error"), "{screen}");
        assert!(
            common::screen_contains(screen, "selected model is not available"),
            "{screen}"
        );
        assert!(!screen.contains("retry 1/8"), "{screen}");
    }
    fn assert_usage_accounting(&self) {
        let kind = self.kind;
        let env = self.env;
        let provider = self.provider;
        let screen = self.screen;
        let exit = self.exit;
        let log_path = self.log_path;

        assert_eq!(exit, 0, "{screen}");
        assert_progress_trace(log_path);
        let store = runtime::SessionStore::from_cwd(env.workspace_root()).unwrap();
        let path = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|session| session.message_count > 0)
            .unwrap()
            .path;
        let session = runtime::Session::load_from_path(&path).unwrap();
        let usage = session
            .messages()
            .iter()
            .rev()
            .find(|message| message.role == runtime::MessageRole::Assistant)
            .unwrap()
            .usage
            .expect("persist the provider's actual usage");
        if matches!(kind, ResponseKind::ChatTotalOnly) {
            self.assert_total_only_bill(&session, &usage);
            self.assert_total_only_resume(&path);
            return;
        }
        if matches!(
            kind,
            ResponseKind::ChatDoneKeepAlive | ResponseKind::ResponsesDoneKeepAlive
        ) {
            assert_eq!(usage.input_tokens, 7, "cached input is counted separately");
            assert_eq!(usage.cache_read_input_tokens, 5);
            assert_eq!(usage.context_tokens(), Some(12));
            assert_eq!(usage.total_tokens(), 20, "never double-count cached tokens");
            assert!(usage.has_complete_token_breakdown());
        } else {
            assert_eq!(usage.input_tokens, 12);
        }
        assert_eq!(
            usage.output_tokens,
            if matches!(kind, ResponseKind::UsageAtStart) {
                7
            } else {
                8
            }
        );
        if matches!(kind, ResponseKind::UsageAtStart) {
            assert!(
                common::screen_contains(screen, "\"cost_units\":17"),
                "{screen}"
            );
            assert!(
                common::screen_contains(screen, "\"cost_currency\":\"sudo_point\""),
                "{screen}"
            );
            assert_eq!(usage.cache_creation_input_tokens, 3);
            assert_eq!(usage.cache_read_input_tokens, 5);
            assert_eq!(usage.cost_units, Some(17));
            assert_eq!(
                usage.cost_currency,
                Some(runtime::UsageCostCurrency::SudoPoint)
            );
        }
        if matches!(kind, ResponseKind::ChatDoneKeepAlive) {
            assert_eq!(
                provider.terminal_connection_state.load(Ordering::SeqCst),
                1,
                "read the independently framed usage tail after finish_reason"
            );
        }
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            1,
            "never regenerate a completed response"
        );
    }
    fn assert_total_only_bill(&self, session: &runtime::Session, usage: &runtime::TokenUsage) {
        let provider = self.provider;
        let screen = self.screen;
        let log_path = self.log_path;

        assert_eq!(usage.input_tokens, 0, "never infer input allocation");
        assert_eq!(usage.output_tokens, 0, "never infer output allocation");
        assert_eq!(
            usage.total_tokens(),
            25,
            "retain the provider's actual total"
        );
        assert_eq!(usage.reported_total_tokens, Some(25));
        assert!(!usage.has_complete_token_breakdown());
        assert!(runtime::UsageTracker::from_session(session)
            .current_context_usage()
            .is_none());
        assert!(usage
            .estimate_cost_usd_with_pricing(runtime::ModelPricing::default_sonnet_tier())
            .is_none());
        assert!(
            common::screen_contains(screen, "\"total_tokens\":25"),
            "{screen}"
        );
        assert!(
            common::screen_contains(screen, "\"token_breakdown_complete\":false"),
            "{screen}"
        );
        assert!(
            !common::screen_contains(screen, "\"cost_units\":null"),
            "unknown bills stay omitted"
        );
        assert!(
            !common::screen_contains(screen, "\"cost_currency\":null"),
            "unknown currencies stay omitted"
        );
        assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
        let events: Vec<Value> = std::fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(events.iter().any(|event| event["event"] == "response_usage"
            && event["attributes"]["reported_total_tokens"] == 25));
    }
    fn assert_total_only_resume(&self, path: &std::path::Path) {
        let env = self.env;
        let provider = self.provider;
        // A second real CLI process must restore the total-only receipt,
        // add a fully allocated response, and recover its context anchor.
        let mut resumed = env.spawn_with_env(&["--permission-mode", "read-only", "--resume"], &[]);
        common::expect_input_line_cleared(&resumed, env.timeout(), "resumed usage prompt");
        resumed.send("Continue the stream result").unwrap();
        common::expect_input_line(
            &resumed,
            "Continue the stream result",
            env.timeout(),
            "resume input",
        );
        resumed.send("\r").unwrap();
        common::expect_screen(
            &resumed,
            |_| {
                runtime::Session::load_from_path(path).is_ok_and(|session| {
                    session
                        .messages()
                        .iter()
                        .filter(|message| message.role == runtime::MessageRole::Assistant)
                        .count()
                        == 2
                })
            },
            env.timeout(),
            "persist the resumed response",
        );
        common::expect_input_line_cleared(&resumed, env.timeout(), "resumed turn complete");
        resumed.send("/exit").unwrap();
        common::expect_input_line(&resumed, "/exit", env.timeout(), "exit input");
        resumed.send("\r").unwrap();
        assert_eq!(resumed.expect_eof().unwrap(), 0);
        let restored = runtime::Session::load_from_path(path).unwrap();
        assert_eq!(
            restored
                .messages()
                .iter()
                .filter(|message| message.role == runtime::MessageRole::Assistant)
                .count(),
            2,
            "resume must update the original transcript"
        );
        let tracker = runtime::UsageTracker::from_session(&restored);
        assert_eq!(tracker.cumulative_usage().total_tokens(), 45);
        assert_eq!(tracker.cumulative_usage().known_total_tokens(), 20);
        assert!(!tracker.cumulative_usage().has_complete_token_breakdown());
        assert_eq!(tracker.current_context_usage(), Some(12));
        assert_eq!(provider.requests.load(Ordering::SeqCst), 2);
    }
    fn assert_completed_response(&self) {
        let kind = self.kind;
        let provider = self.provider;
        let screen = self.screen;
        let exit = self.exit;

        assert_eq!(exit, 0, "{screen}");
        assert!(screen.contains("STREAM_BODY_VERIFIED"), "{screen}");
        if matches!(kind, ResponseKind::DelayedTerminal) {
            assert_eq!(provider.terminal_connection_state.load(Ordering::SeqCst), 1,
                "client must read a separately framed message_stop, not close immediately on stop_reason");
        }
        assert_eq!(
            provider.requests.load(Ordering::SeqCst),
            if matches!(
                kind,
                ResponseKind::EmptyThenComplete
                    | ResponseKind::EmptyThinkingThenComplete
                    | ResponseKind::OverloadedThenComplete
            ) {
                2
            } else {
                1
            },
            "only a response with no usable content should be regenerated, once"
        );
        if matches!(
            kind,
            ResponseKind::EmptyThenComplete | ResponseKind::EmptyThinkingThenComplete
        ) {
            let bodies = provider.bodies.lock().unwrap();
            assert_eq!(
                bodies[0], bodies[1],
                "an empty response retries the original complete wire request"
            );
        }
    }
    fn assert_regular_response(&self) {
        let kind = self.kind;
        let provider = self.provider;
        let screen = self.screen;
        let exit = self.exit;
        let log_path = self.log_path;
        if matches!(
            kind,
            ResponseKind::Complete
                | ResponseKind::StopReasonOnly
                | ResponseKind::DelayedTerminal
                | ResponseKind::StopReasonKeepAlive
                | ResponseKind::EmptyThenComplete
                | ResponseKind::EmptyThinkingThenComplete
                | ResponseKind::OverloadedThenComplete
        ) {
            self.assert_completed_response();
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
                common::model_unavailable_in_screen(screen),
                matches!(kind, ResponseKind::MissingModel),
                "availability must follow the HTTP status, not digits in a diagnostic ID: {screen}"
            );
            assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
        } else if kind.is_refused() {
            assert_ne!(exit, 0, "a refused turn cannot succeed: {screen}");
            assert_refusal_screen(screen);
            assert_eq!(
                provider.requests.load(Ordering::SeqCst),
                1,
                "a refusal must never be retried"
            );
            assert_refusal_trace(log_path);
        } else if matches!(
            kind,
            ResponseKind::AlwaysEmpty | ResponseKind::AlwaysEmptyThinking
        ) {
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
}

fn read_stream_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut input = BufReader::new(socket.try_clone().unwrap());
    let mut first = String::new();
    if input.read_line(&mut first).unwrap_or(0) == 0 {
        return None;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            return None;
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

    Some((first, body))
}

fn stream_body_for(kind: ResponseKind, index: usize) -> String {
    let empty = matches!(
        kind,
        ResponseKind::AlwaysEmpty | ResponseKind::AlwaysEmptyThinking
    ) || (matches!(
        kind,
        ResponseKind::EmptyThenComplete | ResponseKind::EmptyThinkingThenComplete
    ) && index == 0)
        || (matches!(kind, ResponseKind::ToolThenEmptyThenComplete) && index == 1);
    let mut body = refusal_body(kind)
        .unwrap_or_else(|| response_body(!matches!(kind, ResponseKind::Interrupted), empty));
    body = adjust_stream_body(kind, index, body);
    if empty
        && matches!(
            kind,
            ResponseKind::AlwaysEmptyThinking | ResponseKind::EmptyThinkingThenComplete
        )
    {
        let boundary = body.find("event: message_delta").unwrap();
        body.insert_str(boundary, "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
    }
    if matches!(
        kind,
        ResponseKind::StopReasonOnly | ResponseKind::StopReasonKeepAlive
    ) {
        // The provider adapter stops reading after stop_reason. A
        // message_stop in a later packet is therefore never seen.
        let end = body.find("event: message_stop").unwrap();
        body.truncate(end);
    }

    body
}

fn adjust_stream_body(kind: ResponseKind, index: usize, mut body: String) -> String {
    match kind {
                    ResponseKind::SplicedAttempts | ResponseKind::RetryUsageComplete => {
                        let fixture: Value = serde_json::from_str(include_str!(
                            "../../../test-fixtures/retry_usage_stream.json"
                        )).unwrap();
                        let mut events = fixture["events"].as_array().unwrap().clone();
                        if matches!(kind, ResponseKind::RetryUsageComplete) {
                            events.remove(0);
                        } else {
                            // Partial progress from A must remain visible, but
                            // must never be silently joined to response B.
                            events.splice(1..1, [
                                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"FAILED_ATTEMPT_PARTIAL"}}),
                                json!({"type":"content_block_stop","index":0}),
                            ]);
                        }
                        body = events.iter().fold(String::new(), |mut fixture_text, event| {
mock_anthropic_service::append_sse(&mut fixture_text, event["type"].as_str().unwrap(), event);
fixture_text
});
                    }
                    ResponseKind::GeminiCapture => body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"STREAM_BODY_VERIFIED\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":8}}\n\n".to_string(),
                    ResponseKind::SignedCapture => body = signed_capture_body(),
                    ResponseKind::CapturePaced => {
                        body = padded_capture_body("SSE_LOG_ONLY_PADDING", 512);
                    }
                    ResponseKind::CaptureBurst => {
                        body = padded_capture_body("SSE_BURST_CAPTURE_PADDING", 8192);
                    }
                    ResponseKind::UsageAtStart => {
                        body = body.replace(
                            "\"input_tokens\":12,\"output_tokens\":0",
                            "\"input_tokens\":12,\"output_tokens\":7,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":5,\"cost_units\":17,\"cost_currency\":\"sudo_point\"",
                        ).replace("\"output_tokens\":8", "\"output_tokens\":0");
                    }
                    ResponseKind::ChatDoneKeepAlive | ResponseKind::ChatTotalOnly => {
                        body = concat!(
                            "data: {\"id\":\"chat\",\"choices\":[{\"delta\":{\"content\":\"STREAM_BODY_VERIFIED\"}}]}\n\n",
                            "data: {\"id\":\"chat\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                            "data: {\"id\":\"chat\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":8}}\n\n",
                            "data: [DONE]\n\n",
                        ).into();
                        if matches!(kind, ResponseKind::ChatTotalOnly) && index == 0 {
                            body = body.replace(
                                "\"prompt_tokens\":12,\"completion_tokens\":8",
                                "\"prompt_tokens\":0,\"completion_tokens\":0,\"total_tokens\":25",
                            );
                        }
                        if matches!(kind, ResponseKind::ChatDoneKeepAlive) {
                            body = body.replace(
                                "\"prompt_tokens\":12,\"completion_tokens\":8",
                                "\"prompt_tokens\":12,\"completion_tokens\":8,\"total_tokens\":20,\"prompt_tokens_details\":{\"cached_tokens\":5}",
                            );
                        }
                    }
                    ResponseKind::ResponsesDoneKeepAlive => {
                        body = concat!(
                            "event: response.created\ndata: {\"id\":\"response\",\"model\":\"sonnet\"}\n\n",
                            "event: response.output_text.delta\ndata: {\"delta\":\"STREAM_BODY_VERIFIED\"}\n\n",
                            "event: response.output_text.done\ndata: {}\n\n",
                            "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":8,\"total_tokens\":20,\"input_tokens_details\":{\"cached_tokens\":5}}}}\n\n",
                        ).into();
                    }
                    _ => {}
                }

    body
}

fn write_stream_response(
    socket: &mut TcpStream,
    kind: ResponseKind,
    response: &str,
    terminal_state: &AtomicUsize,
    stop: &AtomicBool,
) {
    if matches!(kind, ResponseKind::CapturePaced) {
        socket.set_nodelay(true).unwrap();
        for bytes in response.as_bytes().chunks(1024) {
            if socket.write_all(bytes).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        return;
    }
    if matches!(kind, ResponseKind::SignedCapture) {
        // Split a UTF-8 code point and the terminal frame across
        // separate packets; capture must preserve their raw bytes.
        let split = response.find("诊断").unwrap() + 1;
        let tail = response.find("event: message_stop").unwrap();
        let bytes = response.as_bytes();
        socket.write_all(&bytes[..split]).unwrap();
        thread::sleep(Duration::from_millis(20));
        socket.write_all(&bytes[split..tail]).unwrap();
        thread::sleep(Duration::from_millis(20));
        let _ = socket.write_all(&bytes[tail..]);
        return;
    }
    if matches!(kind, ResponseKind::DelayedTerminal) {
        let split = response.find("event: message_stop").unwrap();
        socket.write_all(&response.as_bytes()[..split]).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let closed = peer_closed(socket);
        terminal_state.store(if closed { 2 } else { 1 }, Ordering::SeqCst);
        let _ = socket.write_all(&response.as_bytes()[split..]);
        return;
    }
    if matches!(
        kind,
        ResponseKind::ChatDoneKeepAlive | ResponseKind::ChatTotalOnly
    ) {
        let split = response
            .find("data: {\"id\":\"chat\",\"choices\":[],\"usage\"")
            .unwrap();
        socket.write_all(&response.as_bytes()[..split]).unwrap();
        // Observe that the client stays connected after the
        // logical stop, before sending the separate usage tail.
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let closed = peer_closed(socket);
        terminal_state.store(if closed { 2 } else { 1 }, Ordering::SeqCst);
        let _ = socket.write_all(&response.as_bytes()[split..]);
    } else {
        let _ = socket.write_all(response.as_bytes());
    }
    if matches!(
        kind,
        ResponseKind::StopReasonKeepAlive
            | ResponseKind::ChatDoneKeepAlive
            | ResponseKind::ChatTotalOnly
            | ResponseKind::ResponsesDoneKeepAlive
    ) {
        // Keep the HTTP body open after a logical/protocol stop.
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        while !stop.load(Ordering::SeqCst) && !peer_closed(socket) {}
    }
}

impl ResponseKind {
    fn is_liveness_fixture(self) -> bool {
        matches!(
            self,
            ResponseKind::NativeHeartbeat
                | ResponseKind::ChatHeartbeat
                | ResponseKind::ChatAnnotations
                | ResponseKind::ResponsesHeartbeat
                | ResponseKind::AlwaysHeartbeat
                | ResponseKind::HeartbeatAfterOverload
                | ResponseKind::ToolThenHeartbeat
                | ResponseKind::ToolThenEmptyThenComplete
                | ResponseKind::NativePartial
                | ResponseKind::ChatPartial
                | ResponseKind::ResponsesPartial
                | ResponseKind::NativeDropThenContinue
                | ResponseKind::ChatDropThenContinue
                | ResponseKind::ResponsesDropThenContinue
                | ResponseKind::ToolThenDrop
                | ResponseKind::NativeSlow
                | ResponseKind::ChatSlow
                | ResponseKind::ResponsesSlow
                | ResponseKind::FiniteHeartbeat
        )
    }
}

fn liveness_frames(
    kind: ResponseKind,
    texts: [&str; 3],
    tool_turn: bool,
    tool_then_drop: bool,
) -> (String, Vec<String>, String) {
    let mut prefix = String::new();
    let mut tail: String;
    let mut deltas = Vec::new();
    match kind.api_format() {
        Some("openai-completions") => {
            for text in texts {
                deltas.push(format!(
                    "data: {}\n\n",
                    json!({"id":"chat","choices":[{"delta":{"content":text}}]})
                ));
            }
            tail = "data: {\"id\":\"chat\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"id\":\"chat\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":8}}\n\ndata: [DONE]\n\n".into();
        }
        Some("openai-responses") => {
            prefix =
                "event: response.created\ndata: {\"id\":\"live\",\"model\":\"sonnet\"}\n\n".into();
            for text in texts {
                deltas.push(format!(
                    "event: response.output_text.delta\ndata: {}\n\n",
                    json!({"delta":text})
                ));
            }
            tail = "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":8}}}\n\n".into();
        }
        _ => {
            prefix = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"live\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"sonnet\",\"content\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".into();
            for text in texts {
                deltas.push(format!("event: content_block_delta\ndata: {}\n\n", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})));
            }
            tail = "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":8}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".into();
        }
    }
    if tool_turn {
        prefix = prefix[..prefix.find("event: content_block_start").unwrap()].to_owned();
        prefix.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"once\",\"name\":\"bash\",\"input\":{}}}\n\n");
        let _ = std::fmt::Write::write_fmt(
            &mut prefix,
            format_args!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":json!({"command":"sleep 3; echo once >> once.txt"}).to_string()}})
            ),
        );
        tail = tail.replace("end_turn", "tool_use");
        deltas.clear();
    }
    if tool_then_drop {
        // A complete tool call, then text that is cut off mid-response.
        prefix = prefix[..prefix.find("event: content_block_start").unwrap()].to_owned();
        prefix.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"once\",\"name\":\"bash\",\"input\":{}}}\n\n");
        let _ = std::fmt::Write::write_fmt(
            &mut prefix,
            format_args!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":json!({"command":"echo once >> once.txt"}).to_string()}})
            ),
        );
        prefix.push_str(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        prefix.push_str("event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n");
        deltas = deltas
            .iter()
            .map(|delta| delta.replace("\"index\":0", "\"index\":1"))
            .collect();
    }

    (prefix, deltas, tail)
}

fn liveness_diagnostic(
    kind: ResponseKind,
    cli: &pty_expect::PtySession,
    log: &std::path::Path,
) -> String {
    // Preserve the failing protocol and real retry events. A count alone
    // cannot distinguish an HTTP retry from a safe stream replay.
    let events: Vec<Value> = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| {
            event["event"].as_str().is_some_and(|name| {
                name.starts_with("request_") || name.contains("retry") || name.contains("stream")
            })
        })
        .map(|event| {
            let attrs = event["attributes"]
                .as_object()
                .map(|attrs| {
                    attrs
                        .iter()
                        .filter(|(key, _)| {
                            matches!(
                                key.as_str(),
                                "attempt"
                                    | "status"
                                    | "error"
                                    | "reason"
                                    | "elapsed_ms"
                                    | "request_id"
                            )
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<serde_json::Map<_, _>>()
                })
                .unwrap_or_default();
            json!({"event": event["event"], "attributes": attrs})
        })
        .take(64)
        .collect();
    format!(
        "kind={kind:?}; events={events:?}\n{}",
        common::screen_tail(cli, 6000)
    )
}

impl ResponseCheck<'_> {
    fn assert_liveness_replay(
        &self,
        bodies: &[Value],
        expected_requests: usize,
        is_tool: bool,
        continues: bool,
    ) {
        let kind = self.kind;
        if matches!(kind, ResponseKind::ToolThenEmptyThenComplete) {
            assert_eq!(bodies.len(), 3);
            assert!(bodies[1]["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()));
            assert!(bodies[2]["tools"].is_null());
            assert!(bodies[2]["tool_choice"].is_null());
            let mut expected = bodies[1].clone();
            expected.as_object_mut().unwrap().remove("tools");
            expected.as_object_mut().unwrap().remove("tool_choice");
            // With no tool prefix, the provider makes the static system block
            // global; this is the same cache policy as the original fallback.
            assert!(expected["system"][0]["cache_control"]
                .get("scope")
                .is_none());
            expected["system"][0]["cache_control"]["scope"] = json!("global");
            let history = expected["messages"].as_array_mut().unwrap();
            let breakpoint = history.last_mut().unwrap()["content"]
                .as_array_mut()
                .unwrap()
                .last_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("cache_control")
                .expect("the original final message has its cache breakpoint");
            let messages = bodies[2]["messages"].as_array().unwrap();
            assert_eq!(messages.len(), history.len() + 1);
            let final_prompt = messages.last().unwrap();
            assert_eq!(final_prompt["role"], "user");
            let blocks = final_prompt["content"].as_array().unwrap();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0]["cache_control"], breakpoint);
            let text = blocks[0]["text"].as_str().unwrap();
            assert!(text.starts_with("The previous tool execution is complete."));
            assert!(text.contains("Do not call tools."));
            history.push(final_prompt.clone());
            assert_eq!(
            expected, bodies[2],
            "only tools, the appended final prompt and the existing cache-boundary policy change"
        );
        } else if continues {
            // A continuation is a new request built from the saved partial
            // reply, never a resend of the request whose stream dropped.
            assert_ne!(bodies[0], bodies[1], "a continuation is never a resend");
            let continuation_body = bodies[1].to_string();
            assert!(
                continuation_body.contains("STREAM_BODY_VERIFIED"),
                "the continuation carries the partial reply: {continuation_body}"
            );
            if !is_tool {
                assert!(
                    continuation_body.contains("cut off by a connection error"),
                    "the model is told to continue: {continuation_body}"
                );
            }
        } else if expected_requests > 1 {
            let first = usize::from(is_tool);
            assert!(
                bodies[first..].iter().all(|body| body == &bodies[first]),
                "safe replay must preserve the request bytes"
            );
        }
    }
    fn assert_liveness_completion(
        &self,
        bodies: &[Value],
        is_tool: bool,
        always_drops: bool,
        interactive: bool,
    ) {
        let kind = self.kind;
        let env = self.env;
        let screen = self.screen;
        let log_path = self.log_path;
        if is_tool {
            assert_eq!(
                std::fs::read_to_string(env.workspace_root().join("once.txt")).unwrap(),
                "once\n",
                "completed tools are never executed twice"
            );
            assert!(bodies[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))));
        }
        if always_drops {
            assert!(
                common::screen_contains(screen, "continuing (3/3)"),
                "{screen}"
            );
            assert!(common::screen_contains(screen, "automatic replay disabled"));
        }
        if kind.drops_once() && interactive {
            assert!(
                common::screen_contains(screen, "continuing (1/3)"),
                "{screen}"
            );
            assert_eq!(
                screen.matches("STREAM_BODY_VERIFIED").count(),
                1,
                "partial output is not repeated: {screen}"
            );
        }
        if matches!(kind, ResponseKind::HeartbeatAfterOverload) {
            assert!(
                common::screen_contains(screen, "retry 2/8"),
                "one shared retry sequence: {screen}"
            );
            let attempts: Vec<_> = std::fs::read_to_string(log_path)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|event| event["event"] == "request_started")
                .map(|event| event["attributes"]["attempt"].as_u64().unwrap())
                .collect();
            assert_eq!(attempts, [1, 2, 3]);
        }
    }
}

fn seed_old_response_logs(directory: &std::path::Path, old_logs: usize) {
    if old_logs > 0 {
        std::fs::create_dir_all(directory).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&vec![b'x'; 64 * 1024]).unwrap();
        let old_body = encoder.finish().unwrap();
        for index in 0..old_logs {
            std::fs::write(directory.join(format!("old-{index}.sse.gz")), &old_body).unwrap();
        }
    }
}
