//! Capture the actual HTTP payload after provider conversion while driving the
//! real CLI through a PTY. No assertion treats mock usage as a real cache hit.
mod common;
#[path = "support/request_evidence.rs"]
mod request_evidence;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::TestEnv;
use pty_expect::PtySession;
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};
use std::fmt::Write as _;

const COMPACT: &str = "Create a concise checkpoint";
const RESPONSE_LOG_ONLY: &str = "RESPONSE_LOG_ONLY_MUST_STAY_OUT_OF_CONTEXT";
const WAIT: Duration = Duration::from_secs(30);
const DIAGNOSTIC_SSE: usize = 1;
const DIAGNOSTIC_JSON: usize = 2;
const RESTARTED_MESSAGE: usize = 3;
const INTERRUPTED_MESSAGE: usize = 4;
const DIAGNOSTIC_BETA: &str = "cache-diagnosis-2026-04-07";

#[inline]
fn response_body_paths(directory: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "gz"))
        .collect()
}

#[derive(Clone)]
struct CapturedRequest {
    body: Value,
    beta: Vec<String>,
}

struct Capture {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    retry_next: Arc<AtomicBool>,
    pressure_next: Arc<AtomicBool>,
    thinking_blocks: Arc<AtomicBool>,
    diagnostic_shape: Arc<AtomicUsize>,
    rejections: Arc<Mutex<VecDeque<String>>>,
    external_append: Arc<Mutex<Option<(std::path::PathBuf, String)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Capture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let retry_next = Arc::new(AtomicBool::new(false));
        let pressure_next = Arc::new(AtomicBool::new(false));
        let thinking_blocks = Arc::new(AtomicBool::new(false));
        let thinking = Arc::clone(&thinking_blocks);
        let diagnostic_shape = Arc::new(AtomicUsize::new(0));
        let shape = Arc::clone(&diagnostic_shape);
        let rejections = Arc::new(Mutex::new(VecDeque::new()));
        let failures = Arc::clone(&rejections);
        let external_append = Arc::new(Mutex::new(None));
        let append = Arc::clone(&external_append);
        let stop = Arc::new(AtomicBool::new(false));
        let (captured, retry, pressure, stopped) = (
            Arc::clone(&requests),
            Arc::clone(&retry_next),
            Arc::clone(&pressure_next),
            Arc::clone(&stop),
        );
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => serve(
                        socket, &captured, &retry, &pressure, &thinking, &shape, &failures, &append,
                    ),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("capture accept: {e}"),
                }
            }
        });
        Self {
            url,
            requests,
            retry_next,
            pressure_next,
            thinking_blocks,
            diagnostic_shape,
            rejections,
            external_append,
            stop,
            thread: Some(worker),
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.body.clone())
            .collect()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn read_request(socket: &TcpStream) -> Option<(String, Vec<u8>, Vec<String>)> {
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return None;
    }
    let mut length = 0;
    let mut beta = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            return None;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
            if key.eq_ignore_ascii_case("anthropic-beta") {
                beta.push(value.trim().to_string());
            }
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    Some((first, bytes, beta))
}

fn serve(
    mut socket: TcpStream,
    captured: &Mutex<Vec<CapturedRequest>>,
    retry: &AtomicBool,
    pressure: &AtomicBool,
    thinking: &AtomicBool,
    diagnostic_shape: &AtomicUsize,
    rejections: &Mutex<VecDeque<String>>,
    external_append: &Mutex<Option<(std::path::PathBuf, String)>>,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let Some((first, bytes, beta)) = read_request(&socket) else {
        return;
    };
    if !first.starts_with("POST ") || first.contains("count_tokens") {
        let body = if first.contains("count_tokens") {
            "{\"input_tokens\":2000}"
        } else {
            "{\"data\":[]}"
        };
        respond(&mut socket, "200 OK", "application/json", body);
        return;
    }
    let request: Value = serde_json::from_slice(&bytes).unwrap();
    let binding = beta
        .iter()
        .any(|header| header.contains("thinking-binding-controls-2026-08-01"));
    let number = {
        let mut all = captured.lock().unwrap();
        all.push(CapturedRequest {
            body: request.clone(),
            beta,
        });
        all.len()
    };
    if retry.swap(false, Ordering::Relaxed) {
        respond(
            &mut socket,
            "503 Service Unavailable",
            "application/json",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"retry fixture"}}"#,
        );
        return;
    }
    if let Some(message) = rejections.lock().unwrap().pop_front() {
        if let Some((path, record)) = external_append.lock().unwrap().take() {
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(record.as_bytes())
                .unwrap();
        }
        respond(
            &mut socket,
            "400 Bad Request",
            "application/json",
            &json!({
                "type":"error", "error":{"type":"invalid_request_error", "message":message}
            })
            .to_string(),
        );
        return;
    }
    let content = response_content(&request, number);
    let usage = json!({"input_tokens": if pressure.swap(false, Ordering::Relaxed) { 990_000 } else { 2_000 },"output_tokens":100});
    let tool_use = content["type"] == "tool_use";
    let stop_reason = if tool_use { "tool_use" } else { "end_turn" };
    let shape = diagnostic_shape.load(Ordering::Relaxed);
    let diagnostics = json!({"marker":"缓存诊断 → café", "comparison":"fixture-only"});
    if shape == DIAGNOSTIC_JSON {
        let mut body = json!({"type":"message", "id":format!("reply_{number}"),
            "role":"assistant", "model":request["model"], "content":[content],
            "stop_reason":stop_reason, "usage":usage, "diagnostics":diagnostics});
        if binding {
            body["input_transformations"] = json!([]);
        }
        respond_fragmented(
            &mut socket,
            "application/json",
            &serde_json::to_string_pretty(&body).unwrap(),
        );
        return;
    }
    let start = if tool_use {
        json!({"type":"tool_use","id":content["id"],"name":content["name"],"input":{}})
    } else {
        json!({"type":"text","text":""})
    };
    let delta = if tool_use {
        // Live providers emit whitespace inside tool arguments. Their durable
        // object representation must not look like an external history edit.
        json!({"type":"input_json_delta","partial_json":serde_json::to_string_pretty(&content["input"]).unwrap()})
    } else {
        json!({"type":"text_delta","text":content["text"]})
    };
    let block_index = if thinking.load(Ordering::Relaxed) {
        thinking_fixture(number).len()
    } else {
        0
    };
    let mut events = vec![
        (
            "message_start",
            json!({"type":"message_start","message":{"id":format!("reply_{number}"),"type":"message","role":"assistant","content":[],"model":request["model"],"stop_reason":null,"usage":usage}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":block_index,"content_block":start}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":block_index,"delta":delta}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":block_index}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":usage}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ];
    if shape == RESTARTED_MESSAGE {
        events.insert(
            0,
            (
                "message_start",
                json!({"type":"message_start","message":{
                    "id":"abandoned-attempt", "type":"message", "role":"assistant", "content":[],
                    "model":request["model"], "usage":{"input_tokens":2,
                    "cache_creation_input_tokens":5235,"cache_read_input_tokens":124730}
                }}),
            ),
        );
    }
    if shape == INTERRUPTED_MESSAGE {
        events.truncate(1);
        diagnostic_shape.store(0, Ordering::Relaxed);
    }
    if binding {
        events[0].1["message"]["input_transformations"] = json!([]);
        if number == 4 {
            events[4].1["input_transformations"] = json!([{
                "type":"thinking_dropped", "reason":"model_binding_mismatch",
                "path":"messages.1.content.0", "signature":"never-log-provider-blocks"
            }]);
        }
    }
    if shape == DIAGNOSTIC_SSE {
        events[0].1["message"]["diagnostics"] = diagnostics;
    }
    if block_index > 0 {
        let mut prefix = Vec::new();
        for (index, block) in thinking_fixture(number).into_iter().enumerate() {
            if block["type"] == "thinking" {
                prefix.push(("content_block_start", json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":""}})));
                prefix.push(("content_block_delta", json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":block["thinking"]}})));
                // Signatures can span deltas, even with no readable text.
                let signature = block["signature"].as_str().unwrap();
                for chunk in [&signature[..4], &signature[4..]] {
                    prefix.push(("content_block_delta", json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":chunk}})));
                }
            } else {
                prefix.push((
                    "content_block_start",
                    json!({"type":"content_block_start","index":index,"content_block":block}),
                ));
            }
            prefix.push((
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
        events.splice(1..1, prefix);
    }
    let mut body = format!(": {RESPONSE_LOG_ONLY}\n\n");
    for (event, data) in events {
        if shape == DIAGNOSTIC_SSE {
            // Legal multi-line SSE data, JSON whitespace and CRLF framing.
            write!(body, "event: {event}\r\n").unwrap();
            for line in serde_json::to_string_pretty(&data).unwrap().lines() {
                write!(body, "data: {line}\r\n").unwrap();
            }
            body.push_str("\r\n");
        } else {
            write!(body, "event: {event}\ndata: {data}\n\n").unwrap();
        }
    }
    if shape == DIAGNOSTIC_SSE {
        respond_fragmented(&mut socket, "text/event-stream", &body);
    } else {
        respond(&mut socket, "200 OK", "text/event-stream", &body);
    }
}

fn respond_fragmented(socket: &mut TcpStream, mime: &str, body: &str) {
    socket.set_nodelay(true).unwrap();
    write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").unwrap();
    // HTTP chunks deliberately split UTF-8 codepoints as well as SSE/JSON.
    for bytes in body.as_bytes().chunks(2) {
        write!(socket, "{:x}\r\n", bytes.len()).unwrap();
        socket.write_all(bytes).unwrap();
        socket.write_all(b"\r\n").unwrap();
        socket.flush().unwrap();
    }
    socket.write_all(b"0\r\n\r\n").unwrap();
}

fn thinking_fixture(number: usize) -> Vec<Value> {
    vec![
        json!({"type":"thinking","thinking":"","signature":format!("empty-signature-{number}")}),
        json!({"type":"thinking","thinking":"Visible reasoning summary.","signature":format!("visible-signature-{number}")}),
        json!({"type":"thinking","thinking":"Another separate summary.","signature":format!("next-signature-{number}")}),
        json!({"type":"redacted_thinking","data":format!("opaque-thinking-{number}")}),
    ]
}

fn assert_thinking_replayed(request: &Value, number: usize) {
    let messages = without_cache_markers(request["messages"].clone());
    let blocks = messages
        .as_array()
        .unwrap()
        .iter()
        .find_map(|message| {
            let content = message["content"].as_array()?;
            content
                .iter()
                .any(|b| b["text"] == format!("PREFIX_REPLY_{number}"))
                .then_some(content)
        })
        .expect("the preceding assistant response must be replayed");
    assert_eq!(&blocks[..blocks.len() - 1], thinking_fixture(number),
        "thinking must preserve empty signed blocks, separate block boundaries, split signatures and ciphertext");
}

#[test]
fn thinking_blocks_survive_turns_and_resume_without_merging_or_dropping() {
    let env = TestEnv::new_mock("thinking-block-boundaries");
    let capture = Capture::new();
    capture.thinking_blocks.store(true, Ordering::Relaxed);
    let path = fixture(&env, &capture);
    let mut legacy = Session::load_from_path(&path).unwrap();
    legacy
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: Some(String::new()),
            },
            ContentBlock::Thinking {
                thinking: "Legacy unsigned reasoning.".into(),
                signature: Some(String::new()),
            },
            ContentBlock::Text {
                text: "LEGACY_UNSIGNED_ANSWER".into(),
            },
        ]))
        .unwrap();
    legacy.save_to_path(&path).unwrap();
    // The fixture writer must hand the session to the resumed CLI exclusively.
    drop(legacy);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "First signed response.");
    turn(&mut cli, "Replay the signed response.");
    exit(&mut cli);
    assert_thinking_replayed(&capture.bodies()[1], 1);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Replay after restart.");
    exit(&mut cli);
    let requests = capture.bodies();
    assert_thinking_replayed(&requests[2], 1);
    assert_thinking_replayed(&requests[2], 2);
    for request in &requests {
        let blocks: Vec<_> = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .collect();
        assert!(blocks
            .iter()
            .any(|block| block["text"] == "LEGACY_UNSIGNED_ANSWER"));
        assert!(
            blocks.iter().all(|block| {
                block["type"] != "thinking"
                    || block["signature"].as_str().is_some_and(|s| !s.is_empty())
            }),
            "unsigned legacy thinking must not invalidate the resumed request"
        );
    }
}

fn unanswered_turn_fixture(
    env: &TestEnv,
    capture: &Capture,
    tail: &str,
) -> (std::path::PathBuf, Vec<ConversationMessage>) {
    let path = fixture(env, capture);
    let mut original = Session::load_from_path(&path).unwrap();
    original
        .push_user_text("Inspect the OLD_TASK profile.")
        .unwrap();
    if tail != "user" {
        original
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: Some("old-tool-signature".into()),
                },
                ContentBlock::Text {
                    text: "OLD_TASK\ncourse\nInspecting the old profile.".into(),
                },
                ContentBlock::ToolUse {
                    id: "old-profile-call".into(),
                    name: "PowerShell".into(),
                    input: r#"{"command":"echo OLD_TASK"}"#.into(),
                    thought_signature: None,
                },
            ]))
            .unwrap();
        original
            .push_message(ConversationMessage::tool_result(
                "old-profile-call",
                "PowerShell",
                r#"{"stdout":"OLD_TASK done","exit_code":0}"#,
                false,
            ))
            .unwrap();
    }
    if tail == "complete" {
        original
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "The old task is complete.".into(),
            }]))
            .unwrap();
    }
    original.save_to_path(&path).unwrap();
    (path, original.messages().to_vec())
}

#[test]
fn resumed_unanswered_turns_end_before_new_requests_without_rewriting_history() {
    for (mode, tail) in [
        ("off", "tool"),
        ("queue", "tool"),
        ("off", "user"),
        ("off", "complete"),
    ] {
        let env = TestEnv::new_mock("unanswered-turn-boundary");
        let capture = Capture::new();
        let (path, original_messages) = unanswered_turn_fixture(&env, &capture, tail);
        let expected_sentinels = usize::from(tail != "complete");
        let mut cli = spawn_with_env(&env, &path, &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)]);
        turn(&mut cli, "Start a different task: NEW_TASK first.");
        turn(&mut cli, "Continue NEW_TASK with the previous answer.");
        exit(&mut cli);
        let mut cli = spawn_with_env(&env, &path, &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)]);
        common::expect_screen_settled(
            &cli,
            |screen| {
                screen.contains("Continue NEW_TASK") && !screen.contains("No response requested.")
            },
            WAIT,
            "internal turn marker stays out of replay",
        );
        turn(&mut cli, "Recall NEW_TASK after restarting.");
        exit(&mut cli);
        let requests = capture.bodies();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            let messages = request["messages"].as_array().unwrap();
            let sentinels: Vec<_> = messages
                .iter()
                .enumerate()
                .filter(|(_, message)| {
                    message["role"] == "assistant"
                        && message["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block["type"] == "text" && block["text"] == "No response requested."
                            })
                        })
                })
                .map(|(index, _)| index)
                .collect();
            assert_eq!(
                sentinels.len(),
                expected_sentinels,
                "{mode}/{tail}: missing or duplicated turn boundary"
            );
            if let Some(&index) = sentinels.first() {
                assert_eq!(messages[index - 1]["role"], "user");
                assert_eq!(messages[index + 1]["role"], "user");
                assert!(messages[index + 1]["content"]
                    .to_string()
                    .contains("NEW_TASK first"));
                if tail == "tool" {
                    assert_eq!(
                        messages[index - 1]["content"][0]["tool_use_id"],
                        "old-profile-call"
                    );
                    assert!(messages[index - 1]["content"]
                        .to_string()
                        .contains("OLD_TASK done"));
                }
            }
        }
        for pair in requests.windows(2) {
            fixed_prefix(&pair[0], &pair[1]);
            history_prefix(&pair[0], &pair[1]);
        }
        let persisted = Session::load_from_path(&path).unwrap();
        assert_eq!(
            &persisted.messages()[..original_messages.len()],
            &original_messages,
            "old signed history or tool results changed"
        );
        assert_eq!(
            persisted
                .messages()
                .iter()
                .filter(|message| {
                    message.role == runtime::MessageRole::Assistant
                        && message.blocks
                            == vec![ContentBlock::Text {
                                text: "No response requested.".into(),
                            }]
                })
                .count(),
            expected_sentinels
        );
    }
}

fn repair_archives(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let prefix = format!(
        "{}.before-thinking-repair-",
        path.file_name().unwrap().to_str().unwrap()
    );
    std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|entry| {
            entry
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&prefix)
        })
        .collect()
}

fn without_thinking(mut messages: Value) -> Value {
    for message in messages.as_array_mut().unwrap() {
        if message["role"] == "assistant" {
            message["content"].as_array_mut().unwrap().retain(|block| {
                block["type"] != "thinking" && block["type"] != "redacted_thinking"
            });
        }
    }
    messages
}

#[test]
fn rejected_thinking_is_archived_once_and_new_signatures_survive_resume() {
    for (mode, shared) in [("off", false), ("queue", false), ("off", true)] {
        let env = TestEnv::new_mock("thinking-repair-resume");
        let capture = Capture::new();
        capture.thinking_blocks.store(true, Ordering::Relaxed);
        let path = fixture(&env, &capture);
        if shared {
            use runtime::fs_backend::{FsBackend, RecordLogMode, StdFsBackend};
            use runtime::history::{HistoryLog, HistoryRecord, RecordOutcome};

            let seed = Session::load_from_path(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            let log = StdFsBackend
                .record_log(&path.to_string_lossy(), RecordLogMode::CreateNew)
                .unwrap();
            let mut history = HistoryLog::new(log);
            let record = HistoryRecord::initialize(&seed).unwrap();
            assert_eq!(
                history.commit(&record, Duration::from_secs(5)).unwrap(),
                RecordOutcome::Applied
            );
        }
        let config_path = env.config_home().join("sudocode.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] =
            json!("claude-opus-5-5");
        config["models"]["claude-sonnet"]["extraBody"] = json!({"betas":["thinking-binding-controls-2026-08-01"],"thinking":{"block_binding":{"prefix_mismatch_behavior":"error"}}});
        std::fs::write(config_path, config.to_string()).unwrap();
        let mut cli = spawn_with_env(&env, &path, &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)]);
        turn(&mut cli, "discover CronList");
        assert_eq!(capture.bodies().len(), 2);
        let before = Session::load_from_path(&path).unwrap();
        let before_bytes = std::fs::read(&path).unwrap();
        capture.rejections.lock().unwrap().push_back("Invalid `signature` in `thinking` block. The block is bound to a different conversation.".into());
        turn(
            &mut cli,
            "Keep the discovered tool result after signature repair.",
        );
        assert_eq!(
            capture.bodies().len(),
            4,
            "one rejection and exactly one repaired request"
        );
        let requests = capture.bodies();
        assert!(requests
            .iter()
            .all(|body| body["thinking"]["block_binding"]["prefix_mismatch_behavior"] == "error"));
        assert!(capture
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.beta == ["thinking-binding-controls-2026-08-01"]));
        fixed_prefix(&requests[2], &requests[3]);
        assert_eq!(
            without_thinking(without_cache_markers(requests[2]["messages"].clone())),
            without_cache_markers(requests[3]["messages"].clone())
        );
        let archives = repair_archives(&path);
        assert_eq!(archives.len(), 1);
        let archived_bytes = std::fs::read(&archives[0]).unwrap();
        assert!(
            archived_bytes.starts_with(&before_bytes),
            "archive retains the original durable bytes and appended prompt"
        );
        let archived = Session::load_from_path(&archives[0]).unwrap();
        assert_eq!(
            &archived.messages()[..before.messages().len()],
            before.messages()
        );
        let repaired = Session::load_from_path(&path).unwrap();
        for (old, new) in before.messages().iter().zip(repaired.messages()) {
            let mut expected = old.clone();
            if expected.role == runtime::MessageRole::Assistant {
                expected.blocks.retain(|block| {
                    !matches!(
                        block,
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
                    )
                });
            }
            if expected.blocks != old.blocks {
                assert_ne!(
                    old.history.as_ref().map(|identity| &identity.id),
                    new.history.as_ref().map(|identity| &identity.id),
                    "changed signed content must have a new immutable identity"
                );
            }
            expected.history.clone_from(&new.history);
            assert_eq!(
                &expected, new,
                "text, tool records and usage stay unchanged"
            );
        }
        turn(
            &mut cli,
            "Keep the new signed response on the next request.",
        );
        assert_eq!(capture.bodies().len(), 5, "one continuation request");
        assert_thinking_replayed(capture.bodies().last().unwrap(), 4);
        exit(&mut cli);
        let mut cli = spawn_with_env(&env, &path, &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)]);
        turn(&mut cli, "Keep the new signed response after restart.");
        assert_eq!(capture.bodies().len(), 6, "one resumed request");
        assert_thinking_replayed(capture.bodies().last().unwrap(), 4);
        assert_eq!(repair_archives(&path).len(), 1);
        exit(&mut cli);
        let log = std::fs::read_to_string(env.workspace_root().join("cache-prefix-requests.jsonl"))
            .unwrap();
        let events: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"] == "thinking_history_repaired")
                .count(),
            1
        );
        let acks: Vec<_> = events
            .iter()
            .filter(|e| e["event"] == "thinking_input_transformations")
            .collect();
        assert!(
            (5..=6).contains(&acks.len()),
            "mode={mode}, acknowledgments={}, attributes={:?}, PTY:\n{}",
            acks.len(),
            acks.iter()
                .map(|event| &event["attributes"])
                .collect::<Vec<_>>(),
            cli.render(|screen| screen.contents())
        );
        let final_updates: Vec<_> = acks
            .iter()
            .filter(|e| e["attributes"]["count"] == 1)
            .collect();
        assert_eq!(final_updates.len(), 1);
        assert_eq!(
            final_updates[0]["attributes"]["transformations"],
            json!([{
                "type":"thinking_dropped", "reason":"model_binding_mismatch",
                "path":"messages.1.content.0"
            }])
        );
        assert!(!log.contains("never-log-provider-blocks"));
    }
}

#[test]
fn thinking_repair_is_bounded_and_unrelated_bad_requests_preserve_history() {
    for (message, attempts, archives) in [
        (
            "Invalid `signature` in `thinking` block. recovery-test-rejection",
            2,
            1,
        ),
        ("invalid max_tokens recovery-test-rejection", 1, 0),
    ] {
        let env = TestEnv::new_mock("thinking-repair-bounded");
        let capture = Capture::new();
        let path = fixture(&env, &capture);
        capture
            .rejections
            .lock()
            .unwrap()
            .extend([message.to_string(), message.to_string()]);
        let mut cli = spawn(&env, &path);
        cli.send("Continue despite the deliberately rejected request.\r")
            .unwrap();
        cli.expect("recovery-test-rejection").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "rejected request completed");
        assert_eq!(capture.bodies().len(), attempts);
        assert_eq!(repair_archives(&path).len(), archives);
        exit(&mut cli);
    }
}

#[test]
fn thinking_repair_preserves_a_concurrent_transcript_append() {
    let env = TestEnv::new_mock("thinking-repair-concurrent");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let record = json!({"type":"message","message":{
        "role":"user", "blocks":[{"type":"text","text":"EXTERNAL_APPEND_MUST_SURVIVE"}]
    }})
    .to_string()
        + "\n";
    *capture.external_append.lock().unwrap() = Some((path.clone(), record));
    capture
        .rejections
        .lock()
        .unwrap()
        .push_back("Invalid `signature` in `thinking` block.".into());
    let mut cli = spawn(&env, &path);
    cli.send("Continue while another writer appends history.\r")
        .unwrap();
    cli.expect("Thinking repair preserved history").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "concurrent edit preserved");
    assert_eq!(capture.bodies().len(), 1);
    assert!(repair_archives(&path).is_empty());
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("EXTERNAL_APPEND_MUST_SURVIVE"));
    let session = Session::load_from_path(&path).unwrap();
    assert!(session.messages().iter().flat_map(|message| &message.blocks).any(|block|
        matches!(block,ContentBlock::Thinking {signature:Some(signature),..} if signature == "signed-prefix-0")));
    assert!(session.messages().iter().flat_map(|message| &message.blocks).any(|block|
        matches!(block,ContentBlock::Text {text} if text == "EXTERNAL_APPEND_MUST_SURVIVE")));
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(session.messages(), restored.messages());
    drop(cli);
}

#[test]
fn invalid_beta_configuration_sends_no_request_and_can_be_corrected() {
    let env = TestEnv::new_mock("thinking-beta-configuration");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    for (betas, error) in [
        (json!([42]), "must be an array of strings"),
        (json!(["invalid beta"]), "invalid beta name"),
    ] {
        config["models"]["claude-sonnet"]["extraBody"] = json!({"betas":betas});
        std::fs::write(&config_path, config.to_string()).unwrap();
        let mut cli = env.spawn(&["--resume", path.to_str().unwrap()]);
        cli.expect(error).unwrap();
        assert_ne!(cli.expect_eof().unwrap(), 0);
        assert!(capture.bodies().is_empty());
    }
    config["models"]["claude-sonnet"]["extraBody"] = json!({"betas":[]});
    std::fs::write(config_path, config.to_string()).unwrap();
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Continue after correcting the beta list.");
    exit(&mut cli);
    assert_eq!(capture.bodies().len(), 1);
    assert!(capture.requests.lock().unwrap()[0].beta.is_empty());
}

#[test]
fn adaptive_thinking_and_effort_stay_fixed_across_compaction_and_resume() {
    for model in [
        "claude-opus-4-8",
        "vendor/claude-opus-4-8",
        "claude-opus-5-5",
        "vendor/claude-opus-5-5",
    ] {
        let env = TestEnv::new_mock("adaptive-cache-prefix");
        let capture = Capture::new();
        capture.thinking_blocks.store(true, Ordering::Relaxed);
        let path = fixture(&env, &capture);
        let config_path = env.config_home().join("sudocode.json");
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!(model);
        std::fs::write(&config_path, config.to_string()).unwrap();
        let start = || {
            let cli = env.spawn(&[
                "--resume",
                path.to_str().unwrap(),
                "--effort",
                if model.contains("4-8") {
                    "xhigh"
                } else {
                    "max"
                },
                "--permission-mode",
                "read-only",
            ]);
            common::expect_input_line_cleared(&cli, WAIT, "adaptive resume ready");
            cli
        };
        let mut cli = start();
        turn(&mut cli, "Continue with adaptive thinking.");
        let baseline = capture.bodies()[0].clone();
        assert_eq!(baseline["model"], model);
        assert_eq!(
            baseline["thinking"],
            json!({"type":"adaptive","display":"summarized"})
        );
        assert_eq!(
            baseline["output_config"]["effort"],
            if model.contains("4-8") {
                "xhigh"
            } else {
                "max"
            }
        );
        assert!(baseline.get("reasoning_effort").is_none());
        turn(&mut cli, "Continue the next step.");
        assert_thinking_replayed(&capture.bodies()[1], 1);
        let response_dir = Session::response_captures_dir_for(
            &path,
            &Session::load_from_path(&path).unwrap().session_id,
        )
        .unwrap();
        common::expect_screen(
            &cli,
            |_| {
                let bodies = response_body_paths(&response_dir);
                bodies.len() == 2
                    && bodies.iter().all(|path| {
                        std::fs::read(path.with_extension("").with_extension("json"))
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                            .is_some_and(|metadata| metadata["gzip_complete"] == true)
                    })
            },
            WAIT,
            "two background response logs finalized",
        );
        let before_compact: Vec<_> = response_body_paths(&response_dir)
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                let mut body = String::new();
                flate2::read::GzDecoder::new(bytes.as_slice())
                    .read_to_string(&mut body)
                    .unwrap();
                assert!(body.contains(RESPONSE_LOG_ONLY));
                (path, bytes)
            })
            .collect();
        assert_eq!(before_compact.len(), 2);
        assert!(!common::screen_tail(&cli, 12000).contains(RESPONSE_LOG_ONLY));
        cli.send("/compact\r").unwrap();
        cli.expect("Messages removed").unwrap_or_else(|error| {
            panic!(
                "compaction did not complete: {error}; {}",
                common::screen_tail(&cli, 12000)
            )
        });
        common::expect_input_line_cleared(&cli, WAIT, "adaptive compaction finished");
        turn(&mut cli, "Continue after compaction.");
        exit(&mut cli);
        let bodies_before_restart = response_body_paths(&response_dir).len();
        let settings_path = env.config_home().join("settings.json");
        std::fs::write(&settings_path, r#"{"responseRecording":{"enabled":false}}"#).unwrap();
        let mut cli = start();
        turn(&mut cli, "Continue after restart.");
        exit(&mut cli);
        assert_eq!(
            response_body_paths(&response_dir).len(),
            bodies_before_restart
        );
        std::fs::write(&settings_path, r#"{"responseRecording":{"enabled":true}}"#).unwrap();
        let mut cli = start();
        turn(&mut cli, "Continue after enabling response logs again.");
        exit(&mut cli);
        assert_eq!(
            response_body_paths(&response_dir).len(),
            bodies_before_restart + 1
        );
        let requests = capture.bodies();
        assert!(requests.iter().any(|r| r.to_string().contains(COMPACT)));
        for request in &requests[1..] {
            fixed_prefix(&baseline, request);
        }
        for request in &requests {
            assert!(!request.to_string().contains(RESPONSE_LOG_ONLY));
        }
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains(RESPONSE_LOG_ONLY));
        for (path, bytes) in before_compact {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert_thinking_replayed(requests.last().unwrap(), requests.len() - 1);
    }
}

#[test]
fn adaptive_hidden_thinking_keeps_signed_history_and_uses_provider_default_effort() {
    let env = TestEnv::new_mock("adaptive-hidden-thinking");
    let capture = Capture::new();
    capture.thinking_blocks.store(true, Ordering::Relaxed);
    let path = fixture(&env, &capture);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!("claude-opus-5-5");
    std::fs::write(config_path, config.to_string()).unwrap();
    std::fs::write(
        env.config_home().join("settings.json"),
        r#"{"thinking":false}"#,
    )
    .unwrap();
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Thinking display is hidden.");
    turn(&mut cli, "Keep the hidden signed response.");
    exit(&mut cli);
    let requests = capture.bodies();
    for request in &requests {
        assert_eq!(
            request["thinking"],
            json!({"type":"adaptive","display":"omitted"})
        );
        assert_eq!(request["output_config"]["effort"], "medium");
        assert!(request.get("reasoning_effort").is_none());
    }
    assert_thinking_replayed(&requests[1], 1);
}

#[test]
fn opus48_thinking_can_be_disabled_and_reenabled_after_resume() {
    let env = TestEnv::new_mock("opus48-thinking-toggle");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!("claude-opus-4-8");
    std::fs::write(config_path, config.to_string()).unwrap();
    for is_enabled in [true, false, true] {
        std::fs::write(
            env.config_home().join("settings.json"),
            json!({"thinking": is_enabled}).to_string(),
        )
        .unwrap();
        let mut cli = spawn(&env, &path);
        turn(&mut cli, "Continue with the current thinking setting.");
        exit(&mut cli);
        let request = capture.bodies().pop().unwrap();
        if is_enabled {
            assert_eq!(
                request["thinking"],
                json!({"type":"adaptive","display":"summarized"})
            );
        } else {
            assert!(request.get("thinking").is_none());
        }
        assert_eq!(request["model"], "claude-opus-4-8");
        assert_eq!(request["output_config"]["effort"], "high");
    }
}

#[test]
fn effort_changes_preserve_signed_history_and_max_is_session_scoped() {
    let env = TestEnv::new_mock("effort-cache-prefix");
    let capture = Capture::new();
    capture.thinking_blocks.store(true, Ordering::Relaxed);
    let path = fixture(&env, &capture);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!("claude-opus-4-8");
    std::fs::write(config_path, config.to_string()).unwrap();
    let mut cli = spawn(&env, &path);
    for level in ["xhigh", "max", "low"] {
        cli.send(&format!("/effort {level}\r")).unwrap();
        cli.expect(&format!("Effective: {level}")).unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "effort applied");
        turn(&mut cli, "Continue with the selected effort.");
        assert_eq!(
            capture.bodies().last().unwrap()["output_config"]["effort"],
            level
        );
        let settings: Value = serde_json::from_slice(
            &std::fs::read(env.config_home().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["effortLevel"],
            if level == "max" { "xhigh" } else { level }
        );
    }
    cli.send("/effort max\r").unwrap();
    cli.expect("this session only").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "session max applied");
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "max compaction complete");
    turn(&mut cli, "Continue after checkpoint.");
    exit(&mut cli);
    let before_resume = capture.bodies();
    assert!(before_resume
        .iter()
        .any(|body| body.to_string().contains(COMPACT)));
    for pair in before_resume[..3].windows(2) {
        for key in ["model", "system", "tools", "metadata", "thinking"] {
            assert_eq!(pair[0][key], pair[1][key], "effort rewrote {key}");
        }
        history_prefix(&pair[0], &pair[1]);
    }
    assert_thinking_replayed(&before_resume[1], 1);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Continue after restart using persisted effort.");
    exit(&mut cli);
    let requests = capture.bodies();
    assert_eq!(requests.last().unwrap()["output_config"]["effort"], "low");
    history_prefix(before_resume.last().unwrap(), requests.last().unwrap());
    assert_thinking_replayed(requests.last().unwrap(), before_resume.len());
}

#[test]
fn tool_input_objects_and_legacy_strings_survive_cli_resume_without_number_loss() {
    for (legacy, changed_input) in [(false, false), (true, false), (false, true)] {
        let env = TestEnv::new_mock("tool-input-history");
        let capture = Capture::new();
        let path = fixture(&env, &capture);
        let input = json!({"fraction":0.375,"exponent":1.25e-7,"large":18446744073709551615u64,"nested":{"label":"缓存 → café 😀","list":[true,null,0.0625]}});
        let raw_input = input.to_string().replace('😀', r"\ud83d\ude00");
        let mut session = Session::load_from_path(&path).unwrap();
        session
            .push_user_text("Inspect the original sensor sample.")
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: "history-input".into(),
                    name: "sensor".into(),
                    input: raw_input.clone(),
                    thought_signature: None,
                },
            ]))
            .unwrap();
        session
            .push_message(ConversationMessage::tool_result(
                "history-input",
                "sensor",
                "sample=0.375",
                false,
            ))
            .unwrap();
        session.save_to_path(&path).unwrap();
        drop(session);
        if legacy || changed_input {
            let raw = std::fs::read_to_string(&path).unwrap();
            let mut changed_message = None;
            let mut rewritten: Vec<String> = raw
                .lines()
                .map(|line| {
                    let mut row: Value = serde_json::from_str(line).unwrap();
                    let mut is_changed = false;
                    if let Some(blocks) = row["message"]["blocks"].as_array_mut() {
                        for block in blocks {
                            if block["id"] == "history-input" {
                                block["input"] = if changed_input {
                                    is_changed = true;
                                    let mut changed = input.clone();
                                    changed["fraction"] = json!(0.75);
                                    changed
                                } else {
                                    json!(raw_input)
                                };
                            }
                        }
                    }
                    if is_changed {
                        changed_message = Some(row["message"].clone());
                        line.to_owned()
                    } else {
                        row.to_string()
                    }
                })
                .collect();
            if changed_input {
                let mut checkpoint = rewritten
                    .iter()
                    .rev()
                    .find_map(|line| {
                        let row: Value = serde_json::from_str(line).unwrap();
                        (row["type"] == "history_checkpoint").then_some(row)
                    })
                    .unwrap();
                checkpoint["parent_id"] = checkpoint["id"].clone();
                checkpoint["id"] = json!("changed-input-checkpoint");
                checkpoint["messages"] = json!([changed_message.expect("stored tool message")]);
                rewritten.push(checkpoint.to_string());
            }
            std::fs::write(&path, rewritten.join("\n") + "\n").unwrap();
        }
        if changed_input {
            let mut cli = env.spawn(&["--resume", path.to_str().unwrap()]);
            cli.set_default_timeout(WAIT);
            cli.expect("checkpoint changes an existing message")
                .unwrap();
            cli.expect_eof().unwrap();
            assert!(capture.bodies().is_empty());
            continue;
        }
        let mut cli = spawn(&env, &path);
        turn(&mut cli, "Continue from the stored sample.");
        exit(&mut cli);
        let mut cli = spawn(&env, &path);
        turn(&mut cli, "Continue after process restart.");
        exit(&mut cli);
        let requests = capture.bodies();
        assert_eq!(requests.len(), 2);
        history_prefix(&requests[0], &requests[1]);
        for request in requests {
            let block = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|message| message["content"].as_array().unwrap())
                .find(|block| block["id"] == "history-input")
                .unwrap();
            assert_eq!(block["input"], input);
        }
    }
}

#[test]
fn model_picker_selects_effort_and_cancel_keeps_the_original_choice() {
    for mode in ["off", "queue"] {
        let env = TestEnv::new_mock("model-effort-picker");
        let capture = Capture::new();
        capture.thinking_blocks.store(true, Ordering::Relaxed);
        let path = fixture(&env, &capture);
        let config_path = env.config_home().join("sudocode.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        for model in ["claude-opus-4-8", "claude-sonnet-4-6", "claude-opus-4-5"] {
            let mut entry = config["models"]["claude-sonnet"].clone();
            entry["alias"] = json!(model);
            entry["providers"]["api-key"]["model"] = json!(model);
            config["models"][model] = entry;
        }
        std::fs::write(config_path, config.to_string()).unwrap();
        let mut cli = spawn_with_env(&env, &path, &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)]);
        turn(&mut cli, "Continue before adjusting the model.");
        let before = capture.bodies()[0].clone();
        cli.send("/model\r").unwrap();
        cli.expect("Select model").unwrap();
        cli.send("claude-opus-4-8\r").unwrap();
        cli.expect("Select effort").unwrap_or_else(|error| {
            panic!(
                "effort picker: {error}\n{}",
                cli.render(|screen| screen.contents())
            )
        });
        cli.send("\x1b").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "effort selection cancelled");
        turn(&mut cli, "Continue after cancelling the selection.");
        let cancelled = capture.bodies()[1].clone();
        fixed_prefix(&before, &cancelled);
        history_prefix(&before, &cancelled);

        cli.send("/model\r").unwrap();
        cli.expect("Select model").unwrap();
        cli.send("claude-opus-4-8\r").unwrap();
        cli.expect("Select effort").unwrap();
        cli.send("xhigh\r").unwrap();
        cli.expect("Effective: xhigh").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "model and effort selected");
        turn(&mut cli, "Continue using the selected model and effort.");
        let selected = capture.bodies()[2].clone();
        assert_eq!(selected["model"], "claude-opus-4-8");
        assert_eq!(selected["output_config"]["effort"], "xhigh");
        assert_thinking_replayed(&selected, 1);
        history_prefix(&cancelled, &selected);
        cli.send("/model claude-sonnet-4-6\r").unwrap();
        cli.expect("Model updated").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "legacy model selected");
        turn(
            &mut cli,
            "Continue with a model that does not support xhigh.",
        );
        assert_eq!(capture.bodies()[3]["output_config"]["effort"], "high");
        cli.send("/model claude-opus-4-5\r").unwrap();
        cli.expect("Model updated").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "Opus 4.5 selected");
        turn(&mut cli, "Continue with Opus 4.5's effort ladder.");
        assert_eq!(capture.bodies()[4]["output_config"]["effort"], "high");
        cli.send("/model claude-opus-4-8\r").unwrap();
        cli.expect("Model updated").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "Opus 4.8 restored");
        turn(&mut cli, "Restore the original effort preference.");
        assert_eq!(capture.bodies()[5]["output_config"]["effort"], "xhigh");
        exit(&mut cli);
    }
}

fn response_content(request: &Value, number: usize) -> Value {
    let compact = request.to_string().contains(COMPACT);
    let last = request["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
    let searching = !compact && !last.contains("tool_result") && last.contains("discover ");
    let saving = !compact && !last.contains("tool_result") && last.contains("SAVE_GLOBAL_MEMORY");
    if !compact && !last.contains("tool_result") && last.contains("LIST_PEER_DESCRIPTIONS") {
        json!({"type":"tool_use", "id":format!("peer-list-{number}"), "name":"agent_list", "input":{}})
    } else if saving {
        let system = prompt_text(request);
        let directory = system
            .split("Global memory: `")
            .nth(1)
            .unwrap()
            .split('`')
            .next()
            .unwrap();
        json!({"type":"tool_use", "id":format!("save_{number}"), "name":"write_file",
            "input":{"path": std::path::Path::new(directory).join("saved-by-tool.md"),
                "content":"GLOBAL_FACT_WRITTEN_THROUGH_TOOL"}})
    } else if searching {
        let name = if last.contains("CronCreate") {
            "CronCreate"
        } else {
            "CronList"
        };
        json!({"type":"tool_use","id":format!("search_{number}"),"name":"ToolSearch","input":{"query":format!("select:{name}")}})
    } else {
        json!({"type":"text","text": if compact {
            let checkpoint = if last.contains("PEER_DESCRIPTION_PLAIN_INVALID") {
                mock_anthropic_service::CANNED_COMPACTION_SUMMARY.replace("<summary>", "").replace("</summary>", "")
            } else {
                mock_anthropic_service::CANNED_COMPACTION_SUMMARY.to_owned()
            };
            if !last.contains("Optional checkpoint metadata:") || last.contains("PEER_DESCRIPTION_MISSING") {
                checkpoint
            } else if last.contains("PEER_DESCRIPTION_INVALID") || last.contains("PEER_DESCRIPTION_PLAIN_INVALID") {
                format!("{checkpoint}\n<agent-description>{{\"unexpected\":true}}</agent-description>")
            } else if last.contains("PEER_DESCRIPTION_UNTERMINATED") {
                format!("{checkpoint}\n<agent-description>broken optional metadata")
            } else {
                assert!(last.contains("CURRENT_PEER_WORK"), "description must receive the latest preserved intent");
                let description = if last.contains("PEER_DESCRIPTION_RAW") {
                    "Investigating peer discovery"
                } else {
                    "\"Investigating peer discovery\""
                };
                format!("{checkpoint}\n<agent-description>{description}</agent-description>\n")
            }
        } else { format!("PREFIX_REPLY_{number}") }})
    }
}

fn peer_description_settings(env: &TestEnv, agent: &str) {
    std::fs::write(
        env.config_home().join("settings.json"),
        json!({
            "agentName": agent,
            "experimental": {"peerDescriptions": true},
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn peer_descriptions_survive_compaction_restart_and_real_discovery_without_changing_prefix() {
    for raw_metadata in [false, true] {
        assert_peer_description_lifecycle(raw_metadata);
    }
}

fn assert_peer_description_lifecycle(raw_metadata: bool) {
    let env = TestEnv::new_mock("peer-description-lifecycle");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    peer_description_settings(&env, "description-author");
    let namespace = runtime::mailbox::local_pair_root_in(env.config_home());
    let metadata = namespace.join("agents/description-author/description.json");
    let empty = runtime::mailbox::Mailbox::workspace_local(&namespace, "empty-peer".into());
    empty.ensure_presence().unwrap();
    let mut cli = spawn_with_env(
        &env,
        &path,
        &[("SUDOCODE_EXPERIMENT_PEER_DESCRIPTIONS", "0")],
    );
    turn(
        &mut cli,
        &format!(
            "CURRENT_PEER_WORK: investigate peer discovery after changing tasks. {}",
            if raw_metadata {
                "PEER_DESCRIPTION_RAW"
            } else {
                ""
            }
        ),
    );
    turn(&mut cli, "Continue with this assignment.");
    assert!(
        !metadata.exists(),
        "ordinary turns leave a new peer undescribed"
    );
    let ordinary = capture.bodies().pop().unwrap();
    let original = std::fs::read(&path).unwrap();
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "baseline checkpoint committed");
    let baseline = capture.bodies().pop().unwrap();
    exit(&mut cli);
    assert!(
        !metadata.exists(),
        "disabled compaction leaves metadata absent"
    );
    std::fs::write(&path, original).unwrap();
    let mut cli = spawn(&env, &path);
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "peer description committed");
    let published = std::fs::read(&metadata).unwrap();
    let description: Value = serde_json::from_slice(&published).unwrap();
    assert_eq!(description["description"], "Investigating peer discovery");
    assert!(description["updated_at"].as_str().is_some());
    let compact = capture.bodies().pop().unwrap();
    assert_peer_compaction_prefix(&ordinary, &baseline, &compact);
    turn(&mut cli, "Continue after the description checkpoint.");
    exit(&mut cli);
    let saved = Session::load_from_path(&path).unwrap();
    assert_eq!(description["source_session_id"], saved.session_id);
    let continuation = checkpoint_text(&saved);
    assert!(!continuation.contains("agent-description"));
    assert!(!continuation.contains("Optional checkpoint metadata:"));
    drop(saved);
    let mut resumed = spawn(&env, &path);
    turn(&mut resumed, "Continue after process restart.");
    exit(&mut resumed);
    assert_eq!(
        published,
        std::fs::read(&metadata).unwrap(),
        "resume preserves metadata and its actual update time"
    );
    for request in capture.bodies() {
        fixed_prefix(&ordinary, &request);
        if !request.to_string().contains(COMPACT) {
            assert!(!request.to_string().contains("<agent-description>"));
            assert!(!request
                .to_string()
                .contains("Optional checkpoint metadata:"));
        }
    }

    assert_peer_description_discovery(&env, &capture, &description);
}

fn assert_peer_compaction_prefix(ordinary: &Value, baseline: &Value, compact: &Value) {
    fixed_prefix(ordinary, compact);
    fixed_prefix(baseline, compact);
    let baseline_messages = baseline["messages"].as_array().unwrap();
    let compact_messages = compact["messages"].as_array().unwrap();
    assert_eq!(
        &baseline_messages[..baseline_messages.len() - 1],
        &compact_messages[..compact_messages.len() - 1],
        "enabling descriptions preserves every historical cache marker"
    );
    let mut summarized_prefix = compact.clone();
    let instruction = summarized_prefix["messages"]
        .as_array_mut()
        .unwrap()
        .pop()
        .unwrap();
    assert!(instruction.to_string().contains("CURRENT_PEER_WORK"));
    assert!(!instruction
        .to_string()
        .contains("Output only a <summary> block"));
    assert!(!instruction
        .to_string()
        .contains("plain text: a <summary> block."));
    assert!(!instruction
        .to_string()
        .contains("plain text only — a <summary> block."));
    history_prefix(&summarized_prefix, ordinary);
}

fn assert_peer_description_discovery(env: &TestEnv, capture: &Capture, description: &Value) {
    // A new peer, driven through its real model/tool loop, reads the same namespace.
    peer_description_settings(env, "description-observer");
    let mut observer = env.spawn(&["--permission-mode", "read-only"]);
    observer.resize(60, 160).unwrap();
    common::expect_input_line_cleared(&observer, WAIT, "description observer ready");
    turn(&mut observer, "LIST_PEER_DESCRIPTIONS");
    let listing = capture
        .bodies()
        .iter()
        .rev()
        .find_map(|request| {
            request["messages"]
                .as_array()?
                .iter()
                .rev()
                .find_map(|message| {
                    message["content"].as_array()?.iter().find_map(|block| {
                        (block["type"] == "tool_result")
                            .then(|| block["content"].as_array())
                            .flatten()
                            .map(|content| {
                                content
                                    .iter()
                                    .filter_map(|block| block["text"].as_str())
                                    .collect::<String>()
                            })
                            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                            .filter(|value| value["agents"].is_array())
                    })
                })
        })
        .expect("real agent_list result reaches the provider");
    let peers = listing["agents"].as_array().unwrap();
    let author = peers
        .iter()
        .find(|row| row["name"] == "description-author")
        .unwrap();
    assert_eq!(author["description"], description["description"]);
    assert_eq!(author["description_updated_at"], description["updated_at"]);
    assert!(
        author.get("active").is_none(),
        "description freshness does not assert runtime liveness"
    );
    assert!(peers
        .iter()
        .any(|row| row["name"] == "empty-peer" && row.get("description").is_none()));
    common::expect_screen_settled(
        &observer,
        |screen| {
            screen.contains("Investigating peer discovery") && screen.contains("updated just now")
        },
        WAIT,
        "peer description and age are visible",
    );
    exit(&mut observer);
}

#[test]
fn optional_peer_metadata_never_causes_a_checkpoint_repair_request() {
    for mode in ["MISSING", "INVALID", "PLAIN_INVALID", "UNTERMINATED"] {
        let env = TestEnv::new_mock("peer-description-optional");
        let capture = Capture::new();
        let path = fixture(&env, &capture);
        peer_description_settings(&env, "description-author");
        let directory = runtime::mailbox::local_pair_root_in(env.config_home())
            .join("agents/description-author");
        std::fs::create_dir_all(&directory).unwrap();
        let metadata = directory.join("description.json");
        let previous = json!({"description":"Earlier ongoing work", "updated_at":"2026-10-01T00:00:00Z", "source_session_id":"previous-session"}).to_string();
        std::fs::write(&metadata, &previous).unwrap();
        let mut cli = spawn(&env, &path);
        turn(
            &mut cli,
            &format!("PEER_DESCRIPTION_{mode}: current assignment changed."),
        );
        cli.send("/compact\r").unwrap();
        cli.expect("Messages removed").unwrap();
        common::expect_input_line_cleared(
            &cli,
            WAIT,
            "valid checkpoint with optional metadata omitted",
        );
        exit(&mut cli);
        assert_eq!(previous.as_bytes(), std::fs::read(metadata).unwrap());
        assert_eq!(
            capture
                .bodies()
                .iter()
                .filter(|request| request.to_string().contains(COMPACT))
                .count(),
            1,
            "optional metadata must not spend another model call"
        );
        let saved = Session::load_from_path(&path).unwrap();
        assert!(!checkpoint_text(&saved).contains("agent-description"));
    }
}

#[test]
fn peer_description_survives_failed_checkpoint_and_pruning_without_a_model_call() {
    let env = TestEnv::new_mock("peer-description-maintenance");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    peer_description_settings(&env, "description-author");
    let directory =
        runtime::mailbox::local_pair_root_in(env.config_home()).join("agents/description-author");
    std::fs::create_dir_all(&directory).unwrap();
    let metadata = directory.join("description.json");
    let previous = json!({"description":"Earlier ongoing work", "updated_at":"2026-10-01T00:00:00Z", "source_session_id":"previous-session"}).to_string();
    std::fs::write(&metadata, &previous).unwrap();
    capture
        .rejections
        .lock()
        .unwrap()
        .push_back("invalid max_tokens recovery-test-rejection".into());
    let mut cli = spawn(&env, &path);
    cli.send("/compact\r").unwrap();
    cli.expect("recovery-test-rejection").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "failed checkpoint preserves description");
    exit(&mut cli);
    assert_eq!(previous.as_bytes(), std::fs::read(&metadata).unwrap());

    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(50_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(1024);
    std::fs::write(config_path, config.to_string()).unwrap();
    let mut session = Session::load_from_path(&path).unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: "large-diagnostic".into(),
                name: "bash".into(),
                input: "{}".into(),
                thought_signature: None,
            },
        ]))
        .unwrap();
    session
        .push_message(ConversationMessage::tool_result(
            "large-diagnostic",
            "bash",
            format!("HEAD_DIAGNOSTIC {} TAIL_DIAGNOSTIC", "x".repeat(160_000)),
            false,
        ))
        .unwrap();
    session.save_to_path(&path).unwrap();
    drop(session);
    let count = capture.bodies().len();
    let mut cli = spawn(&env, &path);
    turn(
        &mut cli,
        "CURRENT_PEER_WORK: continue after pruning diagnostic output.",
    );
    exit(&mut cli);
    let requests = capture.bodies();
    assert_eq!(requests.len(), count + 1, "pruning adds no summary request");
    assert!(requests.last().unwrap()["messages"]
        .to_string()
        .contains("middle pruned"));
    assert_eq!(previous.as_bytes(), std::fs::read(metadata).unwrap());
    let saved = Session::load_from_path(&path).unwrap();
    assert_eq!(
        saved.last_compaction_report.unwrap().method.as_deref(),
        Some("tool_pruning")
    );
}

fn checkpoint_text(session: &Session) -> String {
    session.messages()[0]
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn respond(socket: &mut TcpStream, status: &str, mime: &str, body: &str) {
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{body}", body.len());
    // A cancellation may close the client before this write.
    let _ = socket.write_all(response.as_bytes());
}

fn fixture(env: &TestEnv, capture: &Capture) -> std::path::PathBuf {
    let sample = runtime::SAMPLE_SUDOCODE_JSON
        .replace("https://api.anthropic.com", &capture.url)
        .replace("<YOUR_ANTHROPIC_API_KEY>", "test-pty-key");
    std::fs::write(env.config_home().join("sudocode.json"), sample).unwrap();
    std::fs::write(
        env.workspace_root().join("AGENTS.md"),
        "Keep PREFIX_ORIGINAL instructions.",
    )
    .unwrap();
    let mut session = Session::new().with_workspace_root(env.workspace_root());
    for index in 0..16 {
        session
            .push_user_text(format!(
                "Decision {index}: {}",
                "preserve project requirements ".repeat(150)
            ))
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::Thinking {
                    thinking: format!("Consider decision {index}."),
                    signature: Some(format!("signed-prefix-{index}")),
                },
                ContentBlock::RedactedThinking {
                    data: format!("\"opaque-prefix-{index}\""),
                },
                ContentBlock::Text {
                    text: format!("Recorded decision {index}."),
                },
            ]))
            .unwrap();
    }
    let path = env.workspace_root().join("prefix.jsonl");
    session.save_to_path(&path).unwrap();
    path
}

fn spawn(env: &TestEnv, path: &std::path::Path) -> PtySession {
    spawn_with_env(env, path, &[])
}

fn spawn_with_env(env: &TestEnv, path: &std::path::Path, extra: &[(&str, &str)]) -> PtySession {
    let log = env.workspace_root().join("cache-prefix-requests.jsonl");
    let mut variables = vec![("SCODE_LOG_PATH", log.to_str().unwrap())];
    variables.extend_from_slice(extra);
    let cli = env.spawn_with_env(
        &[
            "--resume",
            path.to_str().unwrap(),
            "--permission-mode",
            "danger-full-access",
        ],
        &variables,
    );
    common::expect_input_line_cleared(&cli, WAIT, "resume ready");
    cli
}

#[test]
fn cache_diagnostics_survive_resume_without_crossing_session_boundaries() {
    for shape in [DIAGNOSTIC_SSE, DIAGNOSTIC_JSON] {
        let env = TestEnv::new_mock("cache-diagnosis-resume");
        let capture = Capture::new();
        capture.diagnostic_shape.store(shape, Ordering::Relaxed);
        let path = fixture(&env, &capture);
        let dump = env.workspace_root().join("diagnosis-dumps");
        let dump = dump.to_str().unwrap();
        let run = |path: &std::path::Path, enabled| {
            let mut cli = spawn_with_env(
                &env,
                path,
                &[
                    (
                        "SUDOCODE_CACHE_DIAGNOSTICS",
                        if enabled { "1" } else { "0" },
                    ),
                    ("SUDOCODE_DUMP_REQUESTS", dump),
                ],
            );
            turn(&mut cli, "Continue the recorded decisions.");
            exit(&mut cli);
        };
        run(&path, false);
        let cache_root = env.config_home().join("cache/prompt-cache");
        let diagnosis_paths = || {
            std::fs::read_dir(&cache_root)
                .unwrap()
                .map(|entry| entry.unwrap().path().join("cache-diagnostics.json"))
                .filter(|path| path.is_file())
                .collect::<Vec<_>>()
        };
        assert!(
            diagnosis_paths().is_empty(),
            "disabled diagnostics must not write state"
        );
        run(&path, true);
        run(&path, true); // A fresh process, not merely another request.
        let other_path = env.workspace_root().join("independent.jsonl");
        Session::new()
            .with_workspace_root(env.workspace_root())
            .save_to_path(&other_path)
            .unwrap();
        run(&other_path, true);
        run(&path, false);
        run(&path, true);
        let requests = capture.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            6,
            "diagnostics must not add or retry requests"
        );
        for index in [0, 4] {
            assert!(requests[index].body.get("diagnostics").is_none());
            assert!(requests[index]
                .beta
                .iter()
                .all(|header| !header.contains(DIAGNOSTIC_BETA)));
        }
        for (index, previous) in [
            (1, Value::Null),
            (2, json!("reply_2")),
            (3, Value::Null),
            (5, json!("reply_3")),
        ] {
            assert_eq!(
                requests[index].body["diagnostics"],
                json!({"previous_message_id":previous})
            );
            assert_eq!(
                requests[index].beta.len(),
                1,
                "beta values must be merged, not duplicate headers"
            );
            assert_eq!(
                requests[index].beta[0]
                    .split(',')
                    .filter(|value| value.trim() == DIAGNOSTIC_BETA)
                    .count(),
                1
            );
        }
        fixed_prefix(&requests[1].body, &requests[2].body);
        history_prefix(&requests[1].body, &requests[2].body);
        let mut ids: Vec<_> = diagnosis_paths()
            .iter()
            .map(|path| {
                serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap()
                    ["message_id"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        ids.sort();
        assert_eq!(ids, ["reply_4", "reply_6"]);
        let diagnoses: Vec<_> = std::fs::read_dir(dump)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("-cache-diagnosis.json")
            })
            .map(|path| serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap())
            .collect();
        assert_eq!(diagnoses.len(), 4);
        for diagnosis in diagnoses {
            assert_eq!(
                diagnosis["diagnostics"]["marker"], "缓存诊断 → café",
                "UTF-8 must survive arbitrary HTTP chunks"
            );
        }
    }
}

fn turn(cli: &mut PtySession, text: &str) {
    let marker = common::turn_status_marker(cli);
    cli.send(&format!("{text}\r")).unwrap();
    common::expect_turn_complete_after(cli, &marker, WAIT, text);
    common::expect_input_line_cleared(cli, WAIT, "turn completed");
}

fn cache_rows(directory: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn ttl_advisor_records_wire_timing_and_decides_once_after_compact() {
    let env = TestEnv::new_mock("ttl-advisor-boundary");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let id = Session::load_from_path(&path).unwrap().session_id;
    let directory = env.config_home().join("cache/prompt-cache").join(id);
    let enabled = [
        ("SUDOCODE_EXPERIMENT_CACHE_TTL_ADVISOR", "1"),
        ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
    ];
    let mut cli = spawn_with_env(&env, &path, &enabled);
    turn(&mut cli, "First observed request.");
    let rows = cache_rows(&directory);
    let template = rows.last().unwrap();
    let observation = &template["observation"];
    let start = observation["request_started_ms"].as_u64().unwrap();
    assert!(start <= observation["response_started_ms"].as_u64().unwrap());
    assert!(
        observation["response_started_ms"].as_u64().unwrap()
            <= observation["response_completed_ms"].as_u64().unwrap()
    );
    let ttls = observation["requested_ttls_seconds"].as_array().unwrap();
    assert!(!ttls.is_empty() && ttls.iter().all(|ttl| *ttl == 300));
    assert!(observation["upstream_ttl_seconds"].is_null());
    let original: Value =
        serde_json::from_str(&std::fs::read_to_string(directory.join("ttl-advice.json")).unwrap())
            .unwrap();
    assert_eq!(original["decisions"].as_array().unwrap().len(), 1);
    assert_eq!(original["decisions"][0]["reason"], "insufficient_evidence");

    // Synthetic history, not a measurement of the fake server's cache. It is
    // written before the boundary, so the next decision cannot see the future.
    let mut history = String::new();
    for index in 0..10u64 {
        let mut row = template.clone();
        row["gateway_request_id"] = json!(format!("fixture-{index}"));
        row["provider_request_id"] = Value::Null;
        let at = start - (11 - index) * 600_000;
        row["observation"]["request_started_ms"] = json!(at);
        row["observation"]["response_started_ms"] = json!(at + 1);
        row["observation"]["response_completed_ms"] = json!(at + 2);
        row["observation"]["client_prefix_compatible"] = json!(true);
        row["cache_creation_input_tokens"] = json!(100_000);
        row["cache_read_input_tokens"] = json!(0);
        writeln!(history, "{row}").unwrap();
        if index == 4 {
            writeln!(history, "{{\"model\":\"claude-opus-5-5\"}}").unwrap();
        }
    }
    for row in rows {
        writeln!(history, "{row}").unwrap();
    }
    std::fs::write(directory.join("requests.jsonl"), history).unwrap();
    turn(
        &mut cli,
        "History alone must not re-decide inside this context.",
    );
    let unchanged: Value =
        serde_json::from_str(&std::fs::read_to_string(directory.join("ttl-advice.json")).unwrap())
            .unwrap();
    assert_eq!(unchanged, original);
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "advisor compaction committed");
    common::expect_screen(
        &cli,
        |screen| {
            screen
                .rsplit_once('─')
                .is_some_and(|(_, footer)| footer.contains("Cache TTL: 1h ≈"))
        },
        WAIT,
        "boundary advice in the shared footer",
    );
    turn(&mut cli, "Continue with the configured policy.");
    exit(&mut cli);
    let state_path = directory.join("ttl-advice.json");
    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(after["decisions"].as_array().unwrap().len(), 2);
    assert_eq!(after["decisions"][1]["context_segment"], 1);
    assert_eq!(after["decisions"][1]["suggested_ttl_seconds"], 3600);
    assert_eq!(after["decisions"][1]["configured_ttl_seconds"], 300);
    assert_eq!(after["decisions"][1]["reason"], "hypothetical_1h_saving");
    assert_eq!(after["decisions"][1]["training"]["eligible_intervals"], 8);
    let rows = cache_rows(&directory);
    assert!(rows
        .iter()
        .any(|r| r["observation"]["request_kind"] == "compaction"));
    assert_eq!(rows.last().unwrap()["observation"]["context_segment"], 1);
    for body in capture.bodies() {
        assert!(
            !body.to_string().contains("\"ttl\":\"1h\""),
            "advice must never change the wire policy"
        );
    }
    let metadata = rows.last().unwrap()["observation"].to_string();
    for secret in ["test-pty-key", "PREFIX_ORIGINAL", capture.url.as_str()] {
        assert!(!metadata.contains(secret));
    }
    let mut cli = spawn_with_env(&env, &path, &enabled);
    exit(&mut cli);
    let resumed: Value =
        serde_json::from_str(&std::fs::read_to_string(state_path).unwrap()).unwrap();
    assert_eq!(
        resumed, after,
        "resume must preserve the decision and its training cutoff"
    );
}

#[test]
fn ttl_advisor_is_opt_in_and_marks_retried_or_restarted_streams() {
    for mode in ["off", "retry", "restarted", "interrupted"] {
        let env = TestEnv::new_mock("ttl-advisor-coverage");
        let capture = Capture::new();
        let path = fixture(&env, &capture);
        let id = Session::load_from_path(&path).unwrap().session_id;
        let directory = env.config_home().join("cache/prompt-cache").join(id);
        if mode == "retry" {
            capture.retry_next.store(true, Ordering::Relaxed);
        }
        if mode == "restarted" {
            capture
                .diagnostic_shape
                .store(RESTARTED_MESSAGE, Ordering::Relaxed);
        }
        if mode == "interrupted" {
            capture
                .diagnostic_shape
                .store(INTERRUPTED_MESSAGE, Ordering::Relaxed);
        }
        let mut cli = spawn_with_env(
            &env,
            &path,
            &[
                (
                    "SUDOCODE_EXPERIMENT_CACHE_TTL_ADVISOR",
                    if mode == "off" { "0" } else { "1" },
                ),
                (
                    "SUDOCODE_CACHE_DIAGNOSTICS",
                    if mode == "restarted" { "1" } else { "0" },
                ),
            ],
        );
        if mode == "restarted" {
            cli.send("Observe an invalid upstream restart.\r").unwrap();
            common::expect_screen(
                &cli,
                |screen| common::screen_contains(screen, "multiple message_start"),
                WAIT,
                "a restarted message is a protocol failure",
            );
            common::expect_input_line_cleared(
                &cli,
                WAIT,
                "protocol failure returned to the prompt",
            );
        } else {
            turn(&mut cli, "Observe a normal reply.");
        }
        exit(&mut cli);
        let rows = cache_rows(&directory);
        let o = &rows.last().unwrap()["observation"];
        match mode {
            "off" => {
                assert!(o.is_null());
                assert!(!directory.join("ttl-advice.json").exists());
            }
            "retry" => assert!(o["attempts"].as_u64().unwrap() >= 2),
            "interrupted" => {
                assert!(
                    rows.iter().any(|r| r["observation"]["completed"] == false
                        && r["input_tokens"].as_u64().unwrap_or(0) > 0),
                    "interrupted observed usage must survive separately from the retry"
                );
                assert_eq!(o["completed"], true);
                assert_eq!(
                    o["attempts"], 1,
                    "empty-response fallback opens a distinct HTTP request scope"
                );
                assert_eq!(capture.bodies().len(), 2);
                let partial = &rows.first().unwrap()["observation"];
                assert!(
                    partial["response_completed_ms"].as_u64().unwrap()
                        <= o["request_started_ms"].as_u64().unwrap(),
                    "consumed partial usage must be recorded before the fallback"
                );
                assert!(
                    o["client_prefix_compatible"].is_null(),
                    "an incomplete response must not advance the successful prefix state"
                );
            }
            "restarted" => {
                assert_eq!(
                    o["message_starts"], 1,
                    "reject the second start before it changes usage"
                );
                assert_eq!(o["completed"], false);
                assert_eq!(rows.last().unwrap()["cache_creation_input_tokens"], 5235);
                assert_eq!(rows.last().unwrap()["cache_read_input_tokens"], 124730);
                let diagnostics: Value = serde_json::from_str(
                    &std::fs::read_to_string(directory.join("cache-diagnostics.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    diagnostics["message_id"], "abandoned-attempt",
                    "the rejected start must not replace the accepted diagnostic identity"
                );
                assert_eq!(
                    capture.bodies().len(),
                    1,
                    "never replay an invalid upstream restart"
                );
            }
            _ => unreachable!(),
        }
    }
}

/// Ten old rows ten minutes apart, which make the advisor's next boundary
/// decision suggest 1h. Written before the boundary, so it sees no future.
fn seed_ttl_history(directory: &std::path::Path) {
    let rows = cache_rows(directory);
    let template = rows.last().unwrap();
    let start = template["observation"]["request_started_ms"]
        .as_u64()
        .unwrap();
    let mut history = String::new();
    for index in 0..10u64 {
        let mut row = template.clone();
        row["gateway_request_id"] = json!(format!("fixture-{index}"));
        row["provider_request_id"] = Value::Null;
        let at = start - (11 - index) * 600_000;
        row["observation"]["request_started_ms"] = json!(at);
        row["observation"]["response_started_ms"] = json!(at + 1);
        row["observation"]["response_completed_ms"] = json!(at + 2);
        row["observation"]["client_prefix_compatible"] = json!(true);
        row["cache_creation_input_tokens"] = json!(100_000);
        row["cache_read_input_tokens"] = json!(0);
        writeln!(history, "{row}").unwrap();
    }
    for row in rows {
        writeln!(history, "{row}").unwrap();
    }
    std::fs::write(directory.join("requests.jsonl"), history).unwrap();
}

fn compact(cli: &mut PtySession, why: &str) {
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(cli, WAIT, why);
}

fn footer_shows(cli: &PtySession, text: &str, why: &str) {
    common::expect_screen(
        cli,
        |screen| {
            screen
                .rsplit_once('─')
                .is_some_and(|(_, footer)| footer.contains(text))
        },
        WAIT,
        why,
    );
}

/// Whether every request sent since `from` asked for 1h.
fn sent_1h_since(capture: &Capture, from: usize) -> Vec<bool> {
    capture
        .bodies()
        .iter()
        .skip(from)
        .map(|body| body.to_string().contains("\"ttl\":\"1h\""))
        .collect()
}

fn saved_ttl(env: &TestEnv) -> Value {
    let text = std::fs::read_to_string(env.config_home().join("sudocode.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["cache_ttl_1h"].clone()
}

/// The TTL panel saves a choice, and the wire follows it at the cheapest
/// moment: a switch to 1h waits for a cold prefix (here, a compaction), a
/// switch to 5m or a forced 1h reaches the next request, and auto removes the
/// saved value. The fake server's cache is not measured; only what is sent.
#[test]
fn ttl_panel_switches_when_cheapest_and_saves_the_choice() {
    let env = TestEnv::new_mock("ttl-switch");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let id = Session::load_from_path(&path).unwrap().session_id;
    let directory = env.config_home().join("cache/prompt-cache").join(id);
    let enabled = [
        ("SUDOCODE_EXPERIMENT_CACHE_TTL_ADVISOR", "1"),
        ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
    ];
    let mut cli = spawn_with_env(&env, &path, &enabled);
    turn(&mut cli, "First observed request.");
    seed_ttl_history(&directory);
    compact(&mut cli, "first compaction");
    // The first request after a compaction rewrites the prefix anyway; this
    // one makes the new context's prefix warm before the switch is chosen.
    turn(&mut cli, "Warm up the new context.");
    footer_shows(&cli, "Cache TTL: 1h ≈", "advice in the footer");

    // ↓ from an empty prompt reaches the tip; Enter opens the panel on the
    // suggested row; Enter applies and saves it.
    cli.send("\x1b[B").unwrap();
    footer_shows(&cli, "Enter to review", "focused tip");
    cli.send("\r").unwrap();
    common::expect_screen(
        &cli,
        |screen| screen.contains("Prompt cache TTL") && screen.contains("advisor: 1h"),
        WAIT,
        "TTL panel",
    );
    cli.send("\r").unwrap();
    cli.expect("Saved · 1h from the next cold start").unwrap();
    assert_eq!(saved_ttl(&env), json!(true));

    // Warm: the prefix was just used at 5m, so a 1h request would rewrite it.
    let from = capture.bodies().len();
    turn(&mut cli, "A warm turn keeps 5m.");
    let warm = sent_1h_since(&capture, from);
    assert!(
        !warm.is_empty() && warm.iter().all(|one_hour| !one_hour),
        "{warm:?}"
    );

    // A compaction rewrites the prefix anyway: the switch lands right after it.
    compact(&mut cli, "second compaction");
    let from = capture.bodies().len();
    turn(&mut cli, "The first turn of the new context.");
    let cold = sent_1h_since(&capture, from);
    assert!(
        !cold.is_empty() && cold.iter().all(|one_hour| *one_hour),
        "{cold:?}"
    );

    // 5m keeps the cache, so it applies to the next request.
    cli.send("/cache ttl 5m\r").unwrap();
    cli.expect("Saved · 5m from the next request").unwrap();
    assert_eq!(saved_ttl(&env), json!(false));
    let from = capture.bodies().len();
    turn(&mut cli, "Back on 5m.");
    let down = sent_1h_since(&capture, from);
    assert!(
        !down.is_empty() && down.iter().all(|one_hour| !one_hour),
        "{down:?}"
    );

    // `n` in the panel forces 1h onto the next request, warm or not.
    footer_shows(&cli, "Cache TTL: 1h ≈", "advice again after 5m");
    cli.send("\x1b[B").unwrap();
    footer_shows(&cli, "Enter to review", "focused tip again");
    cli.send("\r").unwrap();
    common::expect_screen(
        &cli,
        |screen| screen.contains("Prompt cache TTL"),
        WAIT,
        "panel again",
    );
    cli.send("n").unwrap();
    cli.expect("Saved · 1h from the next request").unwrap();
    let from = capture.bodies().len();
    turn(&mut cli, "Forced 1h.");
    let forced = sent_1h_since(&capture, from);
    assert!(
        !forced.is_empty() && forced.iter().all(|one_hour| *one_hour),
        "{forced:?}"
    );

    // auto removes the saved value; the auth mode decides again.
    cli.send("/cache ttl auto\r").unwrap();
    cli.expect("Saved · auto").unwrap();
    assert_eq!(saved_ttl(&env), Value::Null);
    exit(&mut cli);
}
/// A saved 1h reaches a restarted session at the next cold point too: the
/// client is built from the setting, but the first request after a quick
/// restart still reads the prefix the previous process left warm at 5m.
#[test]
fn a_saved_1h_waits_for_a_cold_prefix_after_a_restart() {
    let env = TestEnv::new_mock("ttl-restart");
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let enabled = [
        ("SUDOCODE_EXPERIMENT_CACHE_TTL_ADVISOR", "1"),
        ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
    ];
    let mut cli = spawn_with_env(&env, &path, &enabled);
    turn(&mut cli, "Warm the prefix at 5m.");
    cli.send("/cache ttl 1h\r").unwrap();
    cli.expect("Saved · 1h from the next cold start").unwrap();
    exit(&mut cli);
    assert_eq!(saved_ttl(&env), json!(true));

    let mut cli = spawn_with_env(&env, &path, &enabled);
    let from = capture.bodies().len();
    turn(&mut cli, "A quick restart finds the prefix warm.");
    let warm = sent_1h_since(&capture, from);
    assert!(
        !warm.is_empty() && warm.iter().all(|one_hour| !one_hour),
        "{warm:?}"
    );
    compact(&mut cli, "compaction after the restart");
    let from = capture.bodies().len();
    turn(&mut cli, "Cold after the compaction.");
    let cold = sent_1h_since(&capture, from);
    assert!(
        !cold.is_empty() && cold.iter().all(|one_hour| *one_hour),
        "{cold:?}"
    );
    exit(&mut cli);
}
fn exit(cli: &mut PtySession) {
    cli.send("/exit\r").unwrap();
    let status = cli.expect_eof().unwrap_or_else(|error| {
        panic!("{error}\nexit screen:\n{}", common::screen_tail(cli, 6000))
    });
    assert_eq!(status, 0);
}

fn fixed_prefix(before: &Value, after: &Value) {
    for key in [
        "model",
        "tools",
        "system",
        "thinking",
        "metadata",
        "output_config",
    ] {
        assert_eq!(before[key], after[key], "unexpected change to {key}");
    }
}

fn install_cli_package(registry: &std::path::Path, name: &str) -> std::path::PathBuf {
    let root = registry.join(name);
    std::fs::create_dir_all(root.join("tools")).unwrap();
    // Discovery must not run an entrypoint, including --help or --schema.
    std::fs::write(
        root.join("tools/action_entrypoint.py"),
        "from pathlib import Path\nPath(__file__).with_name('executed').write_text('bad')\n",
    )
    .unwrap();
    root.join("tools").canonicalize().unwrap()
}

#[test]
fn cli_package_registries_are_scoped_and_frozen_without_tool_bindings() {
    let env = TestEnv::new("cli-package-registry");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Continue before installing a CLI package.");
    let baseline = capture.bodies()[0].clone();
    assert!(!baseline["system"]
        .to_string()
        .contains("# Available CLI packages"));

    let user = env.config_home().join("cli-tools");
    let project = env.workspace_root().join(".nexus/sudocode/cli-tools");
    let shadowed = install_cli_package(&user, "zeta-cli");
    let selected = install_cli_package(&project, "zeta-cli");
    let alpha = install_cli_package(&user, "alpha-cli");
    install_cli_package(&project, ".hidden-cli");
    install_cli_package(&user, "_internal-cli");
    std::fs::create_dir_all(user.join("incomplete-cli")).unwrap();
    // A raw project tools/ directory is not an implicit installation.
    install_cli_package(env.workspace_root(), "unregistered-cli");

    #[cfg(unix)]
    let linked = {
        let tools = install_cli_package(env.workspace_root(), "backing-package");
        std::os::unix::fs::symlink(tools.parent().unwrap(), user.join("linked-cli")).unwrap();
        std::os::unix::fs::symlink("missing-target", user.join("broken-cli")).unwrap();
        tools
    };

    turn(&mut cli, "Continue after installing packages.");
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "compaction finished");
    turn(&mut cli, "Continue after compact.");
    exit(&mut cli);
    let mut resumed = spawn(&env, &path);
    turn(&mut resumed, "Continue after restart.");
    exit(&mut resumed);
    for request in capture.bodies() {
        fixed_prefix(&baseline, &request);
    }

    let mut fresh = env.spawn(&["--permission-mode", "danger-full-access"]);
    common::expect_input_line_cleared(&fresh, WAIT, "fresh session ready");
    turn(&mut fresh, "Inspect available capabilities.");
    exit(&mut fresh);
    let requests = capture.bodies();
    let last = requests.last().unwrap();
    assert_eq!(
        baseline["tools"], last["tools"],
        "CLI packages add no schemas"
    );
    let text = last["system"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let section = text.split("# Available CLI packages\n").nth(1).unwrap();
    assert!(section.find("alpha-cli").unwrap() < section.find("zeta-cli").unwrap());
    assert!(section.contains(&serde_json::to_string(&alpha).unwrap()));
    assert!(section.contains(&serde_json::to_string(&selected).unwrap()));
    assert!(!section.contains(&serde_json::to_string(&shadowed).unwrap()));
    for omitted in [
        ".hidden-cli",
        "_internal-cli",
        "incomplete-cli",
        "unregistered-cli",
        "action_entrypoint.py",
    ] {
        assert!(
            !section.contains(omitted),
            "unexpected catalog entry: {omitted}"
        );
    }
    #[cfg(unix)]
    {
        assert!(section.contains("linked-cli"));
        assert!(section.contains(&serde_json::to_string(&linked).unwrap()));
        assert!(!section.contains("broken-cli"));
        assert!(!linked.join("executed").exists());
    }
    for tools in [alpha, selected, shadowed] {
        assert!(!tools.join("executed").exists());
    }
}

fn without_cache_markers(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("cache_control");
            for child in object.values_mut() {
                *child = without_cache_markers(child.take());
            }
        }
        Value::Array(array) => {
            for child in array {
                *child = without_cache_markers(child.take());
            }
        }
        _ => {}
    }
    value
}

fn history_prefix(before: &Value, after: &Value) {
    let old = without_cache_markers(before["messages"].clone());
    let new = without_cache_markers(after["messages"].clone());
    let old = old.as_array().unwrap();
    let new = new.as_array().unwrap();
    assert!(new.len() >= old.len());
    assert_eq!(old, &new[..old.len()], "a cached message changed");
}

#[test]
fn turns_compaction_and_resume_preserve_the_wire_prefix() {
    let env = TestEnv::new("cache-prefix-lifecycle");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Continue the project.");
    std::fs::write(
        env.workspace_root().join("AGENTS.md"),
        "Use PREFIX_CHANGED instructions.",
    )
    .unwrap();
    turn(&mut cli, "Continue the next step.");
    let ordinary = capture.bodies();
    assert_eq!(ordinary.len(), 2);
    fixed_prefix(&ordinary[0], &ordinary[1]);
    history_prefix(&ordinary[0], &ordinary[1]);
    assert!(ordinary[0]["system"]
        .to_string()
        .contains("PREFIX_ORIGINAL"));
    let history = ordinary[0]["messages"].to_string();
    assert!(history.contains("signed-prefix-0"));
    assert!(history.contains("opaque-prefix-0"));

    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "compaction finished");
    turn(&mut cli, "Continue after compact.");
    exit(&mut cli);
    let requests = capture.bodies();
    let summary = requests
        .iter()
        .find(|r| r.to_string().contains(COMPACT))
        .unwrap();
    fixed_prefix(&ordinary[0], summary);
    // Compaction summarizes only the older prefix; its retained tail is not
    // sent to the summarizer. Every included historical message stays exact.
    let mut summarized_prefix = summary.clone();
    summarized_prefix["messages"].as_array_mut().unwrap().pop();
    history_prefix(&summarized_prefix, &ordinary[1]);
    fixed_prefix(&ordinary[0], requests.last().unwrap());
    assert!(
        requests.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .len()
            < ordinary[1]["messages"].as_array().unwrap().len()
    );

    let mut resumed = spawn(&env, &path);
    turn(&mut resumed, "Continue after process restart.");
    exit(&mut resumed);
    let requests = capture.bodies();
    fixed_prefix(&ordinary[0], requests.last().unwrap());
    assert!(!requests.last().unwrap()["system"]
        .to_string()
        .contains("PREFIX_CHANGED"));
}

#[test]
fn retry_and_automatic_compaction_preserve_session_fields() {
    let env = TestEnv::new("cache-prefix-auto");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Begin the next step.");
    capture.retry_next.store(true, Ordering::Relaxed);
    turn(&mut cli, "Continue the retry step.");
    let accepted = request_evidence::accepted_messages(
        &env.workspace_root().join("cache-prefix-requests.jsonl"),
    );
    assert_eq!(
        accepted.len(),
        2,
        "a failed HTTP attempt is not an extra completed model request"
    );
    assert_eq!(
        accepted,
        vec![capture.bodies()[0].clone(), capture.bodies()[2].clone()]
    );
    capture.pressure_next.store(true, Ordering::Relaxed);
    turn(&mut cli, "Continue and checkpoint if needed.");
    turn(&mut cli, "Continue after the checkpoint.");
    exit(&mut cli);
    let requests = capture.bodies();
    assert!(
        requests.iter().any(|r| r.to_string().contains(COMPACT)),
        "must exercise automatic compaction"
    );
    assert_eq!(
        requests[1], requests[2],
        "HTTP retry must replay the same request"
    );
    for request in &requests[1..] {
        fixed_prefix(&requests[0], request);
    }
}

#[test]
fn discovery_changes_tools_once_and_compaction_keeps_them_revealed() {
    let env = TestEnv::new("cache-prefix-discovery");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "discover CronList");
    turn(&mut cli, "discover CronCreate");
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "compaction finished");
    turn(&mut cli, "Continue with the discovered tools.");
    exit(&mut cli);
    let requests = capture.bodies();
    assert_ne!(
        requests[0]["tools"], requests[1]["tools"],
        "first discovery must reveal tools"
    );
    for request in &requests[1..] {
        fixed_prefix(&requests[1], request);
        assert_eq!(requests[0]["system"], request["system"]);
    }
}

#[test]
fn fork_preserves_context_and_explicit_override_replaces_it() {
    let env = TestEnv::new("cache-prefix-fork");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let mut cli = spawn(&env, &path);
    turn(&mut cli, "Continue before the fork.");
    std::fs::write(
        env.workspace_root().join("AGENTS.md"),
        "Use PREFIX_CHANGED instructions.",
    )
    .unwrap();
    cli.send("/session fork\r").unwrap();
    cli.expect("Session forked").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "fork ready");
    turn(&mut cli, "Continue on the fork.");
    exit(&mut cli);
    let requests = capture.bodies();
    for key in ["tools", "system", "thinking"] {
        assert_eq!(requests[0][key], requests[1][key]);
    }
    history_prefix(&requests[0], &requests[1]);

    let mut changed = env.spawn(&[
        "--resume",
        path.to_str().unwrap(),
        "--system-prompt",
        "PREFIX_OVERRIDE",
    ]);
    common::expect_input_line_cleared(&changed, WAIT, "override ready");
    turn(&mut changed, "Continue with the explicit override.");
    exit(&mut changed);
    let requests = capture.bodies();
    let last = requests.last().unwrap();
    assert_ne!(requests[0]["system"], last["system"]);
    assert!(last["system"].to_string().contains("PREFIX_OVERRIDE"));
    assert!(last["system"].to_string().contains("PREFIX_CHANGED"));
    assert_eq!(requests[0]["tools"], last["tools"]);

    let mut resumed = spawn(&env, &path);
    turn(
        &mut resumed,
        "Continue without repeating the override flag.",
    );
    exit(&mut resumed);
    let resumed_requests = capture.bodies();
    fixed_prefix(last, resumed_requests.last().unwrap());
}

fn seed_memory(dir: &std::path::Path, filename: &str, fact: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(filename),
        format!(
        "---\nname: {filename}\ndescription: scoped memory fixture\ntype: feedback\n---\n{fact}\n"
    ),
    )
    .unwrap();
}

fn prompt_text(body: &Value) -> String {
    body["system"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn config_roots_control_settings_packages_and_layered_memory() {
    let env = TestEnv::new("config-roots-memory");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    let path = fixture(&env, &capture);
    let global = env.workspace_root().join("relocated-global");
    let project = env.workspace_root().join("relocated-project");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::copy(
        env.config_home().join("sudocode.json"),
        global.join("sudocode.json"),
    )
    .unwrap();
    // A successful request proves the new spelling wins over the old one.
    std::fs::write(env.config_home().join("sudocode.json"), "{}").unwrap();
    std::fs::write(
        project.join("sudocode.json"),
        r#"{"models":{"claude-sonnet":{"maxOutputTokens":24000}}}"#,
    )
    .unwrap();
    std::fs::write(
        global.join("settings.json"),
        r#"{"agentName":"global-fixture"}"#,
    )
    .unwrap();
    std::fs::write(
        project.join("settings.json"),
        r#"{"agentName":"project-fixture"}"#,
    )
    .unwrap();
    std::fs::write(project.join("AGENTS.md"), "RELOCATED_PROJECT_INSTRUCTIONS").unwrap();
    install_cli_package(&global.join("cli-tools"), "relocated-global-cli");
    install_cli_package(&project.join("cli-tools"), "relocated-project-cli");
    install_cli_package(
        &env.config_home().join("cli-tools"),
        "OLD_ROOT_MUST_NOT_LOAD",
    );
    seed_memory(&global.join("memory"), "shared.md", "GLOBAL_MEMORY_FACT");
    seed_memory(
        &global.join("memory"),
        "collision.md",
        "SHADOWED_GLOBAL_FACT",
    );
    seed_memory(
        &project.join("memory"),
        "collision.md",
        "PROJECT_MEMORY_FACT",
    );
    let vars = [
        ("SCODE_GLOBAL_CONFIG_DIR", global.to_str().unwrap()),
        ("SCODE_PROJECT_CONFIG_DIR", "relocated-project"),
    ];
    let mut cli = env.spawn_with_env(&["--resume", path.to_str().unwrap()], &vars);
    common::expect_input_line_cleared(&cli, WAIT, "relocated roots ready");
    turn(&mut cli, "Continue with configured context.");
    let before = capture.bodies()[0].clone();
    assert_eq!(before["max_tokens"], 24000);
    let text = prompt_text(&before);
    for marker in [
        "RELOCATED_PROJECT_INSTRUCTIONS",
        "relocated-global-cli",
        "relocated-project-cli",
        "GLOBAL_MEMORY_FACT",
        "PROJECT_MEMORY_FACT",
    ] {
        assert!(text.contains(marker), "missing {marker}");
    }
    assert!(!text.contains("SHADOWED_GLOBAL_FACT"));
    assert!(!text.contains("OLD_ROOT_MUST_NOT_LOAD"));
    seed_memory(
        &global.join("memory"),
        "shared.md",
        "GLOBAL_CHANGED_AFTER_START",
    );
    seed_memory(
        &project.join("memory"),
        "collision.md",
        "PROJECT_CHANGED_AFTER_START",
    );
    cli.send("/compact\r").unwrap();
    cli.expect("Messages removed").unwrap();
    common::expect_input_line_cleared(&cli, WAIT, "compact with memory ready");
    turn(&mut cli, "Continue after compact.");
    exit(&mut cli);
    let mut resumed = env.spawn_with_env(&["--resume", path.to_str().unwrap()], &vars);
    common::expect_input_line_cleared(&resumed, WAIT, "resumed memory ready");
    turn(&mut resumed, "Continue after restart.");
    exit(&mut resumed);
    for request in capture.bodies() {
        fixed_prefix(&before, &request);
    }
    let saved = Session::load_from_path(&path).unwrap();
    assert_eq!(
        saved.prompt_snapshot().unwrap().agent_name,
        "project-fixture"
    );
}

#[test]
fn memory_defaults_preserve_legacy_facts_and_explicit_override_is_isolated() {
    let env = TestEnv::new("memory-compatibility");
    if env.is_live() {
        return;
    }
    let capture = Capture::new();
    fixture(&env, &capture);
    let project_memory = env.workspace_root().join(".nexus/sudocode/memory");
    seed_memory(&project_memory, "collision.md", "CURRENT_PROJECT_FACT");
    seed_memory(
        &env.config_home().join("memory"),
        "global.md",
        "GLOBAL_DEFAULT_FACT",
    );
    // Use the actual repository root spelling, just as existing memory does.
    // A temp path can use Windows short names or a macOS /var alias, while the
    // child cwd and canonicalize() can each return a different spelling.
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(env.workspace_root())
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "--quiet"]);
    let git_root = git(&["rev-parse", "--show-toplevel"]);
    let slug: String = git_root
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let legacy_dir = env
        .workspace_root()
        .join("home/.scode/projects")
        .join(slug)
        .join("memory");
    seed_memory(&legacy_dir, "legacy.md", "LEGACY_PROJECT_FACT");
    seed_memory(&legacy_dir, "collision.md", "SHADOWED_LEGACY_FACT");
    let mut cli = env.spawn(&[]);
    common::expect_input_line_cleared(&cli, WAIT, "default memory ready");
    turn(&mut cli, "Continue with existing memory.");
    exit(&mut cli);
    let text = prompt_text(capture.bodies().last().unwrap());
    for marker in [
        "CURRENT_PROJECT_FACT",
        "GLOBAL_DEFAULT_FACT",
        "LEGACY_PROJECT_FACT",
    ] {
        assert!(text.contains(marker), "missing {marker}");
    }
    assert!(!text.contains("SHADOWED_LEGACY_FACT"));
    assert!(std::fs::read_to_string(legacy_dir.join("collision.md"))
        .unwrap()
        .contains("SHADOWED_LEGACY_FACT"));
    let isolated = env.workspace_root().join("isolated-memory");
    seed_memory(&isolated, "only.md", "ISOLATED_MEMORY_FACT");
    let mut cli = env.spawn_with_env(&[], &[("SUDOCODE_MEMORY_DIR", isolated.to_str().unwrap())]);
    common::expect_input_line_cleared(&cli, WAIT, "isolated memory ready");
    turn(&mut cli, "Continue with isolated memory.");
    exit(&mut cli);
    let text = prompt_text(capture.bodies().last().unwrap());
    assert!(text.contains("ISOLATED_MEMORY_FACT"));
    for marker in [
        "CURRENT_PROJECT_FACT",
        "GLOBAL_DEFAULT_FACT",
        "LEGACY_PROJECT_FACT",
    ] {
        assert!(!text.contains(marker), "unexpected {marker}");
    }
}

#[test]
fn global_memory_writes_respect_read_only_and_explicit_deny() {
    for (mode, deny, allowed) in [
        ("workspace-write", false, true),
        ("read-only", false, false),
        ("workspace-write", true, false),
    ] {
        let env = TestEnv::new("global-memory-permissions");
        if env.is_live() {
            return;
        }
        let capture = Capture::new();
        fixture(&env, &capture);
        if deny {
            std::fs::write(
                env.config_home().join("settings.json"),
                r#"{"permissions":{"deny":["write_file"]}}"#,
            )
            .unwrap();
        }
        let mut cli = env.spawn(&["--permission-mode", mode]);
        common::expect_input_line_cleared(&cli, WAIT, "memory permission ready");
        turn(&mut cli, "SAVE_GLOBAL_MEMORY");
        let destination = env.config_home().join("memory/saved-by-tool.md");
        assert_eq!(destination.exists(), allowed, "mode={mode}, deny={deny}");
        if allowed {
            assert_eq!(
                std::fs::read_to_string(&destination).unwrap(),
                "GLOBAL_FACT_WRITTEN_THROUGH_TOOL"
            );
        }
        if allowed {
            std::fs::remove_file(&destination).unwrap();
            cli.send("/permissions read-only\r").unwrap();
            cli.expect("read-only").unwrap();
            common::expect_input_line_cleared(&cli, WAIT, "read-only switch ready");
            turn(&mut cli, "SAVE_GLOBAL_MEMORY after switching to read-only");
            assert!(
                !destination.exists(),
                "live read-only must remove synthetic memory access"
            );
            cli.send("/permissions workspace-write\r").unwrap();
            cli.expect("workspace-write").unwrap();
            common::expect_input_line_cleared(&cli, WAIT, "write switch ready");
            turn(&mut cli, "SAVE_GLOBAL_MEMORY after restoring write mode");
            assert!(destination.exists(), "write mode restores memory access");
        }
        exit(&mut cli);
        let bodies = capture.bodies();
        for body in &bodies {
            assert_eq!(
                body["system"], bodies[0]["system"],
                "permission switches keep the prompt snapshot"
            );
        }
        assert!(capture
            .bodies()
            .iter()
            .any(|body| body["messages"].to_string().contains("tool_result")));
    }
}
