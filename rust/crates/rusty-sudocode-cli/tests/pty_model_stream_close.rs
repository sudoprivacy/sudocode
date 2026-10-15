//! Exercise real CLI failures from interrupted, empty, refused or rejected requests.
mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
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
    StopReasonOnly,
    DelayedTerminal,
    StopReasonKeepAlive,
    EmptyThenComplete,
    AlwaysEmpty,
    EmptyThinkingThenComplete,
    AlwaysEmptyThinking,
    InvalidRequestWithStatusDigits,
    MissingModel,
    RefusedStream,
    RefusedJson,
    RefusedAfterText,
    RefusedTail,
    UsageAtStart,
    ChatDoneKeepAlive,
    ChatTotalOnly,
    ResponsesDoneKeepAlive,
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
    terminal_connection_state: Arc<AtomicUsize>,
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
                if !first.starts_with("POST ") {
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
                let empty = matches!(
                    kind,
                    ResponseKind::AlwaysEmpty | ResponseKind::AlwaysEmptyThinking
                ) || (matches!(
                    kind,
                    ResponseKind::EmptyThenComplete | ResponseKind::EmptyThinkingThenComplete
                ) && index == 0);
                let mut body = refusal_body(kind).unwrap_or_else(|| {
                    response_body(!matches!(kind, ResponseKind::Interrupted), empty)
                });
                match kind {
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
                if matches!(kind, ResponseKind::DelayedTerminal) {
                    let split = response.find("event: message_stop").unwrap();
                    socket.write_all(response[..split].as_bytes()).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_millis(250)))
                        .unwrap();
                    let closed = peer_closed(&socket);
                    terminal_state.store(if closed { 2 } else { 1 }, Ordering::SeqCst);
                    let _ = socket.write_all(response[split..].as_bytes());
                    continue;
                }
                if matches!(
                    kind,
                    ResponseKind::ChatDoneKeepAlive | ResponseKind::ChatTotalOnly
                ) {
                    let split = response
                        .find("data: {\"id\":\"chat\",\"choices\":[],\"usage\"")
                        .unwrap();
                    socket.write_all(response[..split].as_bytes()).unwrap();
                    // Observe that the client stays connected after the
                    // logical stop, before sending the separate usage tail.
                    socket
                        .set_read_timeout(Some(Duration::from_millis(250)))
                        .unwrap();
                    let closed = peer_closed(&socket);
                    terminal_state.store(if closed { 2 } else { 1 }, Ordering::SeqCst);
                    let _ = socket.write_all(response[split..].as_bytes());
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
                    while !stop.load(Ordering::SeqCst) && !peer_closed(&socket) {}
                }
            }
        });
        Self {
            url,
            stopped,
            requests,
            terminal_connection_state,
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
    match kind {
        ResponseKind::ChatDoneKeepAlive
        | ResponseKind::ChatTotalOnly
        | ResponseKind::ResponsesDoneKeepAlive => {
            let format = if matches!(
                kind,
                ResponseKind::ChatDoneKeepAlive | ResponseKind::ChatTotalOnly
            ) {
                "openai-completions"
            } else {
                "openai-responses"
            };
            config["models"]["sonnet"] = json!({"alias":"sonnet", "providers":{"api-key":{
                "provider":"anthropic", "model":"sonnet", "api":format
            }}});
        }
        _ => {}
    }
    std::fs::write(path, config.to_string()).unwrap();
    let log_path = env.workspace_root().join("provider-events.jsonl");
    let mut args = vec![
        "--permission-mode",
        "read-only",
        "--print",
        "Report the stream result",
    ];
    if matches!(kind, ResponseKind::ChatTotalOnly) {
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
    let exit = cli.expect_eof().unwrap();
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
        ResponseKind::UsageAtStart
            | ResponseKind::ChatDoneKeepAlive
            | ResponseKind::ChatTotalOnly
            | ResponseKind::ResponsesDoneKeepAlive
    ) {
        assert_eq!(exit, 0, "{screen}");
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
            .messages
            .iter()
            .rev()
            .find(|message| message.role == runtime::MessageRole::Assistant)
            .unwrap()
            .usage
            .expect("persist the provider's actual usage");
        if matches!(kind, ResponseKind::ChatTotalOnly) {
            assert_eq!(usage.input_tokens, 0, "never infer input allocation");
            assert_eq!(usage.output_tokens, 0, "never infer output allocation");
            assert_eq!(
                usage.total_tokens(),
                25,
                "retain the provider's actual total"
            );
            assert_eq!(usage.reported_total_tokens, Some(25));
            assert!(!usage.has_complete_token_breakdown());
            assert!(runtime::UsageTracker::from_session(&session)
                .current_context_usage()
                .is_none());
            assert!(usage
                .estimate_cost_usd_with_pricing(runtime::ModelPricing::default_sonnet_tier())
                .is_none());
            assert!(
                common::screen_contains(&screen, "\"total_tokens\":25"),
                "{screen}"
            );
            assert!(
                common::screen_contains(&screen, "\"token_breakdown_complete\":false"),
                "{screen}"
            );
            assert_eq!(provider.requests.load(Ordering::SeqCst), 1);
            let events: Vec<Value> = std::fs::read_to_string(&log_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert!(events.iter().any(|event| event["event"] == "response_usage"
                && event["attributes"]["reported_total_tokens"] == 25));

            // A second real CLI process must restore the total-only receipt,
            // add a fully allocated response, and recover its context anchor.
            let mut resumed =
                env.spawn_with_env(&["--permission-mode", "read-only", "--resume"], &[]);
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
                    runtime::Session::load_from_path(&path).is_ok_and(|session| {
                        session
                            .messages
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
            let restored = runtime::Session::load_from_path(&path).unwrap();
            assert_eq!(
                restored
                    .messages
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
        return;
    }
    if matches!(
        kind,
        ResponseKind::Complete
            | ResponseKind::StopReasonOnly
            | ResponseKind::DelayedTerminal
            | ResponseKind::StopReasonKeepAlive
            | ResponseKind::EmptyThenComplete
            | ResponseKind::EmptyThinkingThenComplete
    ) {
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
                ResponseKind::EmptyThenComplete | ResponseKind::EmptyThinkingThenComplete
            ) {
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
