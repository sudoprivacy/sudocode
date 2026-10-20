//! Real CLI -> local file tool -> compatible reasoning -> persisted answer.
//! Live acceptance also runs `pty_model_compat` against deepseek-v3.2-azure.
mod common;

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::{ContentBlock, MessageRole, Session, SessionStore};
use serde_json::{json, Value};

struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Provider {
    fn new(model: &str, case: &str, nonce: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let captured = Arc::clone(&requests);
        let stop = Arc::clone(&stopped);
        let (model, case, nonce) = (model.to_owned(), case.to_owned(), nonce.to_owned());
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // Accepted sockets inherit the listener's mode on Windows.
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let (path, request) = read_request(&mut socket);
                        let (mime, body) = if path.contains("models") {
                            (
                                "application/json",
                                json!({"data":[{"id":model,"tool_calling_supported":true}]})
                                    .to_string(),
                            )
                        } else if path.ends_with("/count_tokens") {
                            ("application/json", json!({"input_tokens":1}).to_string())
                        } else {
                            let request = request.unwrap();
                            let has_result =
                                request["messages"].as_array().unwrap().iter().any(|m| {
                                    m["role"] == "tool"
                                        || m["content"].as_array().is_some_and(|blocks| {
                                            blocks.iter().any(|b| b["type"] == "tool_result")
                                        })
                                });
                            let mut capture = request.clone();
                            capture["_fixture_path"] = json!(path);
                            captured.lock().unwrap().push(capture);
                            if has_result {
                                assert!(
                                    request.to_string().contains(&nonce),
                                    "actual local file contents must reach the provider"
                                );
                                answer(&model, &case, &nonce)
                            } else if case == "anthropic-error" {
                                let frames = [
                                    json!({"type":"message_start","message":{"id":"read-fixture","type":"message","model":model,"role":"assistant","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
                                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"read-local","name":"read_file","input":{}}}),
                                    json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"fixture.txt\"}"}}),
                                    json!({"type":"content_block_stop","index":0}),
                                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":1}}),
                                    json!({"type":"message_stop"}),
                                ];
                                (
                                    "text/event-stream",
                                    frames
                                        .iter()
                                        .map(|f| {
                                            format!(
                                                "event: {}\ndata: {f}\n\n",
                                                f["type"].as_str().unwrap()
                                            )
                                        })
                                        .collect(),
                                )
                            } else {
                                ("application/json", json!({"id":"read-fixture","model":model,"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"read-local","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"fixture.txt\"}"}}]},"finish_reason":"tool_calls"}]}).to_string())
                            }
                        };
                        write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            stopped,
            thread: Some(thread),
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if self.thread.take().unwrap().join().is_err() && !std::thread::panicking() {
            panic!("provider fixture failed");
        }
    }
}

fn read_request(socket: &mut TcpStream) -> (String, Option<Value>) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let end = loop {
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0, "incomplete fixture HTTP headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header = String::from_utf8_lossy(&bytes[..end]);
    let path = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let length = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|n| n.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + length {
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0, "incomplete fixture HTTP body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        path,
        (length > 0).then(|| serde_json::from_slice(&bytes[end..end + length]).unwrap()),
    )
}

fn answer(model: &str, case: &str, nonce: &str) -> (&'static str, String) {
    if let Some(case) = case.strip_prefix("reasoning-") {
        let (is_json, case) = case
            .strip_prefix("json-")
            .map_or((false, case), |case| (true, case));
        if is_json {
            let mut message = reasoning_fields(case, "校验 fixture\n最后一步");
            message["role"] = json!("assistant");
            message["content"] = json!(nonce);
            return (
                "application/json",
                json!({"id":"final","model":model,"choices":[{"index":0,"message":message,"finish_reason":"stop"}]}).to_string(),
            );
        }
        let mut body = String::new();
        for text in ["校验 ", "fixture\n", "最后一步"] {
            let delta = reasoning_fields(case, text);
            let chunk = json!({"id":"final","model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            writeln!(body, "data: {chunk}\n").unwrap();
        }
        let chunk = json!({"id":"final","model":model,"choices":[{"index":0,"delta":{"content":nonce},"finish_reason":"stop"}]});
        write!(body, "data: {chunk}\n\ndata: [DONE]\n\n").unwrap();
        return ("text/event-stream", body);
    }
    if case == "anthropic-error" {
        return (
            "text/event-stream",
            format!(
                "event: error\ndata: {}\n\n",
                json!({"type":"error","error":{"type":"overloaded_error","message":"fixture upstream unavailable"}})
            ),
        );
    }
    let text = match case {
        "truncated" => "<think>fixture checked".to_owned(),
        "literal-prefix" => format!("<thimble>{nonce}"),
        "prefaced" => format!("The exact file contents are:\n\n{nonce}"),
        "empty" => format!("<think></think>{nonce}"),
        _ => format!("<think>校验 fixture</think>\n{nonce}"),
    };
    if matches!(case, "json" | "prefaced") {
        return ("application/json", json!({"id":"final","model":model,"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}).to_string());
    }
    let mut body = String::new();
    if matches!(case, "structured" | "structured-reasoning") {
        let field = if case == "structured" {
            "reasoning_content"
        } else {
            "reasoning"
        };
        let delta = json!({field: "structured fixture check"});
        let chunk = json!({"id":"final","model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    // A character per SSE frame splits both tag boundaries and exercises UTF-8.
    for ch in text.chars() {
        let chunk = json!({"id":"final","model":model,"choices":[{"index":0,"delta":{"content":ch.to_string()},"finish_reason":null}]});
        body.push_str(&format!("data: {chunk}\n\n"));
    }
    let stop = json!({"id":"final","model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    body.push_str(&format!("data: {stop}\n\ndata: [DONE]\n\n"));
    ("text/event-stream", body)
}

fn reasoning_fields(case: &str, text: &str) -> Value {
    match case {
        "current" => json!({"reasoning":text}),
        "legacy" => json!({"reasoning_content":text}),
        "both" => json!({"reasoning":text,"reasoning_content":"must not be duplicated"}),
        "null-current" => json!({"reasoning":null,"reasoning_content":text}),
        "empty-current" => json!({"reasoning":"","reasoning_content":text}),
        "null-legacy" => json!({"reasoning":text,"reasoning_content":null}),
        "none" => json!({}),
        "empty" => json!({"reasoning":"","reasoning_content":null}),
        _ => panic!("unknown reasoning fixture: {case}"),
    }
}

/// Real PTY -> local file tool -> OpenAI-compatible HTTP -> display and disk.
/// These deterministic wire-format cases run in CI; they do not claim live
/// vLLM model acceptance.
#[test]
fn openai_reasoning_fields_reach_the_terminal_and_session() {
    for case in [
        "current",
        "legacy",
        "both",
        "null-current",
        "empty-current",
        "null-legacy",
        "none",
        "empty",
        "json-current",
        "json-legacy",
        "json-both",
        "json-null-current",
        "json-empty-current",
    ] {
        let env = common::TestEnv::new("openai-reasoning-fields");
        if env.is_live() {
            return;
        }
        let nonce = format!("REASONING_FILE_{case}");
        std::fs::write(env.workspace_root().join("fixture.txt"), &nonce).unwrap();
        let model = "glm-fixture";
        let provider = Provider::new(model, &format!("reasoning-{case}"), &nonce);
        let path = env.config_home().join("sudocode.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["auth_modes"]["api-key"]["anthropic"]["baseUrl"] = json!(provider.url);
        config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!(model);
        config["models"]["claude-sonnet"]["providers"]["api-key"]["api"] =
            json!("openai-completions");
        std::fs::write(path, config.to_string()).unwrap();

        let mut cli = env.spawn(&["--permission-mode", "read-only"]);
        cli.expect("❯").unwrap();
        cli.send("Read fixture.txt and report its contents.\r")
            .unwrap();
        let has_thinking = !matches!(case, "none" | "empty");
        if has_thinking {
            cli.expect("✻ Thinking").expect(case);
            cli.expect("校验 fixture").unwrap();
            cli.expect("最后一步").unwrap();
        }
        cli.expect(&nonce).unwrap();
        cli.expect("❯").unwrap();
        cli.send("/exit\r").unwrap();
        assert_eq!(cli.expect_eof().unwrap(), 0, "{case}: CLI exit");

        let store = SessionStore::from_cwd(env.workspace_root()).unwrap();
        let sessions = store.list_sessions().unwrap();
        let session = Session::load_from_path(&sessions[0].path).unwrap();
        assert!(session
            .messages
            .iter()
            .flat_map(|m| &m.blocks)
            .any(|b| matches!(b,
            ContentBlock::ToolResult {output,is_error:false,..} if output.contains(&nonce))));
        let blocks = &session.messages.last().unwrap().blocks;
        let thinking: Vec<_> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking,
            if has_thinking {
                vec!["校验 fixture\n最后一步"]
            } else {
                vec![]
            },
            "{case}: preserve reasoning once, separately from the answer",
        );
        let text: String = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, nonce, "{case}: preserve the actual file answer");
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "{case}: no hidden retries");
        assert!(requests
            .iter()
            .all(|r| r["model"] == model && r["stream"] == true));
    }
}

#[test]
fn azure_envelopes_preserve_file_answers_and_literal_text() {
    for case in [
        "stream",
        "json",
        "prefaced",
        "structured",
        "structured-reasoning",
        "ordinary",
        "literal-prefix",
        "empty",
        "truncated",
        "anthropic-error",
    ] {
        let env = common::TestEnv::new("azure-inline-reasoning");
        if env.is_live() {
            return;
        } // Real provider acceptance uses pty_model_compat.
        let nonce = format!(
            "AZURE_FILE_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::fs::write(env.workspace_root().join("fixture.txt"), &nonce).unwrap();
        let model = if case == "ordinary" {
            "gpt-fixture"
        } else {
            "deepseek-v3.2-azure"
        };
        let provider = Provider::new(model, case, &nonce);
        let path = env.config_home().join("sudocode.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["auth_modes"]["api-key"]["anthropic"]["baseUrl"] = json!(provider.url);
        config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!(model);
        config["models"]["claude-sonnet"]["providers"]["api-key"]["api"] =
            json!(if case == "anthropic-error" {
                "anthropic-messages"
            } else {
                "openai-completions"
            });
        std::fs::write(path, config.to_string()).unwrap();
        let mut cli = env.spawn(&[
            "-p",
            "Read fixture.txt and report its contents.",
            "--permission-mode",
            "read-only",
            "--output-format",
            "json",
        ]);
        if case == "truncated" {
            cli.expect("missing its closing tag").unwrap();
        } else if case == "anthropic-error" {
            cli.expect("fixture upstream unavailable").unwrap();
        }
        let code = cli.expect_eof().unwrap();
        assert_eq!(
            code == 0,
            !matches!(case, "truncated" | "anthropic-error"),
            "{case}: unexpected CLI result"
        );
        let store = SessionStore::from_cwd(env.workspace_root()).unwrap();
        let sessions = store.list_sessions().unwrap();
        let session = Session::load_from_path(&sessions[0].path).unwrap();
        assert!(session.messages.iter().flat_map(|m| &m.blocks).any(|b| matches!(b,ContentBlock::ToolResult {output,is_error:false,..} if output.contains(&nonce))));
        let final_message = session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
            .unwrap();
        let text: String = final_message
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let thinking: String = final_message
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();
        match case {
            "stream" | "json" => assert_eq!(thinking, "校验 fixture"),
            "structured" | "structured-reasoning" => {
                assert_eq!(thinking, "structured fixture check");
            }
            "truncated" | "anthropic-error" => {} // The runtime discards the failed response.
            _ => assert!(thinking.is_empty(), "{case}: unexpected reasoning"),
        }
        if case == "anthropic-error" {
            assert!(
                text.is_empty(),
                "an upstream error must not produce an answer"
            );
        } else if case == "truncated" {
            assert_eq!(
                text.trim(),
                "[Provider stream failed; unfinished tool calls were cancelled.]"
            );
            assert!(!text.contains("fixture checked") && !text.contains(&nonce));
        } else if case == "prefaced" {
            assert_eq!(text, format!("The exact file contents are:\n\n{nonce}"));
            assert_final_file_answer(&text, &nonce);
        } else if matches!(case, "structured" | "structured-reasoning" | "ordinary") {
            assert_eq!(text, format!("<think>校验 fixture</think>\n{nonce}"));
        } else if case == "literal-prefix" {
            assert_eq!(text, format!("<thimble>{nonce}"));
        } else {
            assert_eq!(text.trim(), nonce);
        }
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "{case}: no hidden retries; routes={:?}",
            requests
                .iter()
                .map(|r| &r["_fixture_path"])
                .collect::<Vec<_>>()
        );
        assert!(requests.iter().all(|r| r["model"] == model));
    }
}

#[test]
fn azure_live_file_roundtrip_has_a_final_answer() {
    let env = common::TestEnv::new("azure-live-file");
    if env.is_mock() {
        eprintln!("SKIP: real Azure provider acceptance requires SCODE_TEST_BACKEND=live");
        return;
    }
    let nonce = format!(
        "AZURE_LIVE_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    std::fs::write(env.workspace_root().join("fixture.txt"), &nonce).unwrap();
    let mut cli = env.spawn(&[
        "-p",
        "Read fixture.txt with read_file. Reply with the exact file contents and nothing else.",
        "--permission-mode",
        "read-only",
        "--output-format",
        "json",
    ]);
    cli.set_default_timeout(Duration::from_secs(120));
    cli.expect(&nonce).unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let store = SessionStore::from_cwd(env.workspace_root()).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let session = Session::load_from_path(&sessions[0].path).unwrap();
    let results: Vec<_> = session
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                output, is_error, ..
            } => Some((output, *is_error)),
            _ => None,
        })
        .collect();
    assert!(
        results
            .iter()
            .any(|(output, error)| !error && output.contains(&nonce)),
        "no actual file result"
    );
    assert!(
        results.iter().all(|(_, error)| !error),
        "provider recovered from a failed tool"
    );
    let final_message = session
        .messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .unwrap();
    let text: String = final_message
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_final_file_answer(&text, &nonce);
    let thinking = final_message
        .blocks
        .iter()
        .filter(|b| matches!(b, ContentBlock::Thinking { .. }))
        .count();
    eprintln!("LIVE AZURE FILE PASS: persisted contents verified; thinking blocks={thinking}");
}

fn assert_final_file_answer(text: &str, nonce: &str) {
    // Live models can add an introduction. Require the fresh file answer and
    // keep reasoning envelopes out of the text that users receive.
    assert_eq!(
        text.matches(nonce).count(),
        1,
        "final answer must include the fresh file contents exactly once"
    );
    let answer = text.to_ascii_lowercase();
    for envelope in ["<think>", "</think>", "<thinking>", "</thinking>"] {
        assert!(
            !answer.contains(envelope),
            "final answer must not include a reasoning envelope"
        );
    }
}
