//! Drive real resumed CLI compaction against a controlled HTTP provider. These
//! regressions verify durable history and captured requests, not just UI text.
mod common;

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use common::{spawn_scode_in_dir_with_env, HarnessWorkspace};
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};

struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Provider {
    fn new(mode: &'static str) -> Self {
        Self::with_commit_failure(mode, None)
    }

    fn with_commit_failure(mode: &'static str, read_only_dir: Option<PathBuf>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => serve(socket, mode, &captured, read_only_dir.as_deref()),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock accept: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            stopped,
            worker: Some(worker),
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(
    mut socket: TcpStream,
    mode: &str,
    captured: &Mutex<Vec<Value>>,
    read_only_dir: Option<&std::path::Path>,
) {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut input = BufReader::new(socket.try_clone().unwrap());
    let mut first = String::new();
    if input.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).unwrap();
    let request: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let counting = first.contains("/count_tokens");
    let is_post = first.starts_with("POST ") && !counting;
    let streaming = request["stream"] == true;
    let number = if is_post {
        let mut requests = captured.lock().unwrap();
        requests.push(request);
        requests.len()
    } else {
        0
    };
    if streaming && mode == "success" {
        let events = [
            (
                "message_start",
                json!({"type":"message_start","message":{"id":"reply","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1000,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ALPHA_CONTEXT_OK"}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":10}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let mut body = String::new();
        for (event, data) in &events {
            write!(&mut body, "event: {event}\ndata: {data}\n\n").unwrap();
        }
        let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
        return;
    }
    let (status, response) = if counting {
        ("200 OK", json!({"input_tokens": 1000}))
    } else if !is_post {
        ("200 OK", json!({"data": []}))
    } else if mode == "error" {
        (
            "400 Bad Request",
            json!({"type":"error", "error":{"type":"invalid_request_error", "message":"fixture summarizer unavailable"}}),
        )
    } else {
        let text = match mode {
            "empty" => String::new(),
            "growing" => "unhelpful repetition ".repeat(20_000),
            _ => format!("<summary>Consolidated checkpoint {number}: preserve project ALPHA and pending deployment. Next: verify tests.</summary>"),
        };
        (
            "200 OK",
            json!({"id":"compact-fixture", "type":"message", "role":"assistant", "model":"claude-sonnet-4-6", "content":[{"type":"text", "text":text}], "stop_reason":if mode == "truncated" {"max_tokens"} else {"end_turn"}, "usage":{"input_tokens":1000,"output_tokens":50}}),
        )
    };
    #[cfg(unix)]
    if is_post {
        if let Some(dir) = read_only_dir {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
    }
    #[cfg(not(unix))]
    let _ = read_only_dir;
    let body = response.to_string();
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = socket.write_all(response.as_bytes());
}

fn fixture(workspace: &HarnessWorkspace) -> PathBuf {
    let mut session = Session::new().with_workspace_root(&workspace.root);
    session.model = Some("claude-sonnet-4-6".into());
    for i in 0..16 {
        session
            .push_user_text(format!(
                "PROJECT_ALPHA request {i}: {}",
                "important implementation detail ".repeat(100)
            ))
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: format!("Response {i}: {}", "confirmed decision ".repeat(50)),
            }]))
            .unwrap();
    }
    let path = workspace.root.join("history.jsonl");
    session.save_to_path(&path).unwrap();
    path
}

// The method pointer does not satisfy render's higher-ranked Screen lifetime.
#[allow(clippy::redundant_closure_for_method_calls)]
fn compact(workspace: &HarnessWorkspace, path: &std::path::Path, expected: &str) -> u32 {
    let mut cli = spawn_scode_in_dir_with_env(
        &workspace.root,
        &[
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--resume",
            path.to_str().unwrap(),
            "/compact",
        ],
        Duration::from_secs(30),
        &[
            ("SUDO_CODE_CONFIG_HOME", &workspace.config_home),
            ("HOME", &workspace.home),
        ],
    )
    .unwrap();
    cli.expect(expected).unwrap_or_else(|error| {
        panic!(
            "expected {expected}: {error}\n{}",
            cli.render(|screen| screen.contents())
        )
    });
    cli.expect_eof().unwrap()
}

#[test]
fn failed_empty_truncated_and_growing_summaries_preserve_durable_history() {
    for mode in ["error", "empty", "truncated", "growing"] {
        let provider = Provider::new(mode);
        let workspace = HarnessWorkspace::new(mode);
        workspace.write_mock_config(&provider.url);
        let path = fixture(&workspace);
        let original = std::fs::read(&path).unwrap();
        assert_ne!(compact(&workspace, &path, "history preserved"), 0, "{mode}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "{mode} must not modify the transcript"
        );
        let requests = provider.requests.lock().unwrap();
        assert!(!requests.is_empty(), "must exercise the real model path");
        assert!(
            requests.iter().all(|r| r["stream"] != true),
            "failure must not execute the task"
        );
        assert!(
            requests
                .iter()
                .all(|r| r["messages"].to_string().contains("PROJECT_ALPHA")),
            "retry must retain source history"
        );
    }
}

#[test]
fn recompaction_rewrites_checkpoint_and_archives_original_history() {
    let provider = Provider::new("success");
    let workspace = HarnessWorkspace::new("rolling-checkpoint");
    workspace.write_mock_config(&provider.url);
    let path = fixture(&workspace);
    let original = std::fs::read(&path).unwrap();
    assert_eq!(compact(&workspace, &path, "Messages removed"), 0);
    let first = Session::load_from_path(&path).unwrap();
    assert!(first.messages.len() < 32);
    assert!(
        first.messages.len() > 5,
        "retain a token-priced recent tail, not only four messages"
    );
    assert!(
        first
            .compaction
            .as_ref()
            .unwrap()
            .summary
            .contains("checkpoint 1"),
        "summary: {:?}, requests: {:?}",
        first.compaction,
        provider
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| (
                r["stream"].clone(),
                r["messages"].as_array().map(Vec::len),
                r["tools"].as_array().map(Vec::len)
            ))
            .collect::<Vec<_>>()
    );
    let archives: Vec<_> = std::fs::read_dir(&workspace.root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("before-compact")
        })
        .collect();
    assert_eq!(archives.len(), 1);
    assert_eq!(std::fs::read(archives[0].path()).unwrap(), original);

    let mut next = first;
    for _ in 0..12 {
        next.push_user_text("new still-valid requirements ".repeat(200))
            .unwrap();
    }
    next.save_to_path(&path).unwrap();
    assert_eq!(compact(&workspace, &path, "Messages removed"), 0);
    let second = Session::load_from_path(&path).unwrap();
    let summary = &second.compaction.as_ref().unwrap().summary;
    assert!(summary.contains("checkpoint 2"));
    assert!(
        !summary.contains("checkpoint 1"),
        "old checkpoint must be rewritten, not appended"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "cache-safe path should succeed without a second call"
    );
    assert!(
        requests[1]["messages"].to_string().contains("checkpoint 1"),
        "summarizer must receive the prior checkpoint"
    );
    assert!(
        requests[0]["tools"].is_array(),
        "keep tool definitions in the cacheable prefix"
    );
}

fn set_small_window(workspace: &HarnessWorkspace) {
    let path = workspace.config_home.join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(50_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(1024);
    std::fs::write(path, config.to_string()).unwrap();
}

fn resume(workspace: &HarnessWorkspace, path: &std::path::Path) -> pty_expect::PtySession {
    spawn_scode_in_dir_with_env(
        &workspace.root,
        &[
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--resume",
            path.to_str().unwrap(),
        ],
        Duration::from_secs(30),
        &[
            ("SUDO_CODE_CONFIG_HOME", &workspace.config_home),
            ("HOME", &workspace.home),
        ],
    )
    .unwrap()
}

#[test]
fn automatic_compaction_failure_never_sends_a_historyless_task_request() {
    let provider = Provider::new("error");
    let workspace = HarnessWorkspace::new("automatic-compact-failure");
    workspace.write_mock_config(&provider.url);
    set_small_window(&workspace);
    let path = fixture(&workspace);
    let mut original = Session::load_from_path(&path).unwrap();
    for message in &mut original.messages {
        for block in &mut message.blocks {
            if let ContentBlock::Text { text } = block {
                *text = text.repeat(3);
            }
        }
    }
    original.save_to_path(&path).unwrap();
    let mut cli = resume(&workspace, &path);
    cli.expect("❯").unwrap();
    cli.send("Continue PROJECT_ALPHA using all earlier decisions.\r")
        .unwrap();
    cli.expect("history preserved").unwrap();
    cli.send("/exit\r").unwrap();
    cli.expect_eof().unwrap();
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(
        &restored.messages[..original.messages.len()],
        &original.messages
    );
    let requests = provider.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(
        requests.iter().all(|r| r["stream"] != true),
        "failed compaction must not dispatch a task request"
    );
}

#[test]
fn pressure_prunes_large_tool_output_without_a_summary_call() {
    let provider = Provider::new("success");
    let workspace = HarnessWorkspace::new("prune-before-summary");
    workspace.write_mock_config(&provider.url);
    set_small_window(&workspace);
    let path = fixture(&workspace);
    let mut original = Session::load_from_path(&path).unwrap();
    original
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: "big-output".into(),
                name: "bash".into(),
                input: "{}".into(),
                thought_signature: None,
            },
        ]))
        .unwrap();
    original
        .push_message(ConversationMessage::tool_result(
            "big-output",
            "bash",
            format!("HEAD_DIAGNOSTIC {} TAIL_DIAGNOSTIC", "x".repeat(160_000)),
            false,
        ))
        .unwrap();
    original
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "Review the output next.".into(),
        }]))
        .unwrap();
    original.save_to_path(&path).unwrap();
    let mut cli = resume(&workspace, &path);
    cli.expect("❯").unwrap();
    cli.send("Continue PROJECT_ALPHA.\r").unwrap();
    cli.expect("ALPHA_CONTEXT_OK").unwrap();
    cli.send("/exit\r").unwrap();
    cli.expect_eof().unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "pruning should avoid a summary request");
    assert_eq!(requests[0]["stream"], true);
    let input = requests[0]["messages"].to_string();
    for marker in [
        "PROJECT_ALPHA",
        "HEAD_DIAGNOSTIC",
        "TAIL_DIAGNOSTIC",
        "middle pruned",
    ] {
        assert!(input.contains(marker), "missing {marker}");
    }
    assert!(input.len() < 100_000);
}

#[cfg(unix)]
#[test]
fn persistence_failure_preserves_original_transcript() {
    use std::os::unix::fs::PermissionsExt;
    struct RestorePermissions(PathBuf);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let workspace = HarnessWorkspace::new("compact-persist-failure");
    let provider = Provider::with_commit_failure("success", Some(workspace.root.clone()));
    workspace.write_mock_config(&provider.url);
    let path = fixture(&workspace);
    let original = std::fs::read(&path).unwrap();
    let _restore = RestorePermissions(workspace.root.clone());
    assert_ne!(compact(&workspace, &path, "history preserved"), 0);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1,
        "must reach persistence after a successful model call"
    );
}

#[test]
fn empty_compacted_history_cannot_continue_a_task() {
    let provider = Provider::new("success");
    let workspace = HarnessWorkspace::new("empty-compacted-history");
    workspace.write_mock_config(&provider.url);
    let path = fixture(&workspace);
    let mut damaged = Session::load_from_path(&path).unwrap();
    damaged.messages.clear();
    damaged.record_compaction("legacy checkpoint", 32);
    damaged.save_to_path(&path).unwrap();
    let mut cli = resume(&workspace, &path);
    cli.expect("❯").unwrap();
    cli.send("Continue PROJECT_ALPHA.\r").unwrap();
    cli.expect("no active history").unwrap();
    cli.send("/exit\r").unwrap();
    cli.expect_eof().unwrap();
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "damaged history must never reach the model"
    );
    assert!(Session::load_from_path(&path).unwrap().messages.is_empty());
}
