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

/// Fail loudly when a `0o500` directory does not actually refuse writes.
///
/// Probes the behaviour rather than asking `geteuid() == 0`. The question that
/// decides whether the injection works is "does this environment enforce the
/// permission bit", and running as root is only one of the ways the answer is
/// no — a filesystem mounted without unix modes is another. Asking the
/// behaviour covers both, and cannot drift from what the injection relies on.
#[cfg(unix)]
fn assert_permission_injection_is_armed(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    // Probe INSIDE the directory the injection will disarm, so the answer comes
    // from the filesystem that will carry it. Probing the process-wide temp dir
    // answers about whatever is mounted THERE — the same tree today only
    // because `HarnessWorkspace` happens to build under it too, a coupling
    // nothing declares and nothing would catch if it moved.
    let probe = dir.join(".permission-probe");
    std::fs::create_dir_all(&probe).expect("probe dir");
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o500)).expect("probe chmod");
    let wrote = std::fs::write(probe.join("canary"), b"x").is_ok();
    // Restore before removing: a directory left at 0o500 cannot be emptied.
    let _ = std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::remove_dir_all(&probe);
    assert!(
        !wrote,
        "this test injects a persistence failure by making the workspace \
         read-only (0o500), but this environment let the write through — most \
         often because the suite is running as root, which bypasses permission \
         bits. Run it as a non-root user. As root the injected failure never \
         happens, compaction SUCCEEDS, and the miss surfaces as a misleading \
         assertion about the transcript instead of naming the disarmed injection."
    );
}

impl Provider {
    fn new(mode: &'static str) -> Self {
        Self::with_commit_failure(mode, None)
    }

    /// `read_only_dir` makes the workspace unwritable once the model has
    /// answered, so the CLI's persistence step fails.
    ///
    /// The injection proves itself here rather than in the test: a silently
    /// disarmed injection turns "the product refused to lose the transcript"
    /// into "the product compacted successfully", which reads as a product
    /// regression and points nowhere near the environment that caused it.
    fn with_commit_failure(mode: &'static str, read_only_dir: Option<PathBuf>) -> Self {
        #[cfg(unix)]
        if let Some(dir) = read_only_dir.as_deref() {
            assert_permission_injection_is_armed(dir);
        }
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
    // Back to blocking first, then the timeout.
    //
    // The listener is non-blocking so the accept loop can poll `stopped`, and on
    // WINDOWS an accepted socket inherits that mode — POSIX explicitly does not,
    // which is why this only bites one platform. Inherited, `set_read_timeout`
    // buys nothing: every read with no byte already buffered returns
    // `WouldBlock` (WSAEWOULDBLOCK, os error 10035) immediately, the `unwrap`s
    // below kill this thread, and the test fails 30s later as
    // `timeout waiting for pattern: history preserved` — a server that died
    // looks exactly like a client that never asked.
    socket.set_nonblocking(false).unwrap();
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
        requests.push(request.clone());
        requests.len()
    } else {
        0
    };
    if streaming && mode == "delegate" {
        serve_delegation(&mut socket, &request);
        return;
    }
    if streaming && (matches!(mode, "success" | "post-error") || mode.starts_with("long-summary")) {
        serve_success_stream(&mut socket);
        return;
    }
    let (status, response) = if counting {
        ("200 OK", json!({"input_tokens": 1000}))
    } else if !is_post {
        ("200 OK", json!({"data": []}))
    } else if mode.starts_with("openai-") {
        openai_completion_response(&first, mode, number)
    } else if mode.starts_with("internal")
        && (request["model"]
            != if mode == "internal-prefixed" {
                "intranet/apeiron-v1"
            } else {
                "apeiron-v1"
            }
            || request["max_tokens"].as_u64().unwrap_or(u64::MAX) > 1024)
    {
        (
            "400 Bad Request",
            json!({"type":"error","error":{"type":"invalid_request_error","message":format!("internal deployment rejected model={} max_tokens={}", request["model"], request["max_tokens"])}}),
        )
    } else if mode == "long-summary-fallback" && number == 1 {
        (
            "400 Bad Request",
            json!({"type":"error", "error":{"type":"invalid_request_error", "message":"fixture cache-safe compaction unavailable"}}),
        )
    } else if matches!(mode, "error" | "post-error") {
        (
            "400 Bad Request",
            json!({"type":"error", "error":{"type":"invalid_request_error", "message":"fixture summarizer unavailable"}}),
        )
    } else {
        // A checkpoint that needs more than the former 8192-token ceiling.
        // The provider truncates it when the actual request budget is too small.
        let long_summary = mode.starts_with("long-summary");
        let truncated = mode == "truncated"
            || (long_summary && request["max_tokens"].as_u64().unwrap_or(0) < 10_000);
        let text = match mode {
            "empty" => String::new(),
            "growing" => "unhelpful repetition ".repeat(20_000),
            _ if long_summary && truncated => "<summary>Incomplete checkpoint".into(),
            _ if long_summary => format!("<summary>PROJECT_ALPHA {}CHECKPOINT_COMPLETE</summary>", "fact ".repeat(7_200)),
            _ => format!("<summary>Consolidated checkpoint {number}: preserve project ALPHA and pending deployment. Next: verify tests.</summary>"),
        };
        (
            "200 OK",
            json!({"id":"compact-fixture", "type":"message", "role":"assistant", "model":"claude-sonnet-4-6", "content":[{"type":"text", "text":text}], "stop_reason":if truncated {"max_tokens"} else {"end_turn"}, "usage":{"input_tokens":1000,"output_tokens":if long_summary {10_000} else {50}}}),
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

fn serve_success_stream(socket: &mut TcpStream) {
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
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":1000,"output_tokens":10}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ];
    let mut body = String::new();
    for (event, data) in &events {
        write!(&mut body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    let _ = socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
}

fn openai_completion_response(first: &str, mode: &str, number: usize) -> (&'static str, Value) {
    assert!(first.contains("/chat/completions"), "{first}");
    if mode == "openai-fallback" && number == 1 {
        (
            "400 Bad Request",
            json!({"error":{"message":"cached checkpoint rejected","type":"invalid_request_error"}}),
        )
    } else {
        let mut message = json!({"role":"assistant","content":"<summary>Preserve PROJECT_ALPHA and pending deployment. Next: verify tests.</summary>"});
        if mode == "openai-empty" {
            message["content"] = json!("");
        }
        if mode == "openai-tool" {
            message["tool_calls"] = json!([{"id":"unexpected-tool","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"touch SHOULD_NOT_EXIST\"}"}}]);
        }
        (
            "200 OK",
            json!({"id":"checkpoint","object":"chat.completion","created":0,"model":"provider-response-label","choices":[{"index":0,"message":message,"finish_reason":match mode { "openai-truncated" => "length", "openai-tool" => "tool_calls", _ => "stop" }}],"usage":{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050}}),
        )
    }
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

fn compact(workspace: &HarnessWorkspace, path: &std::path::Path, expected: &str) -> u32 {
    compact_with_model(workspace, path, "sonnet", expected)
}

// render requires a closure with a higher-ranked Screen lifetime.
#[allow(clippy::redundant_closure_for_method_calls)]
fn compact_with_model(
    workspace: &HarnessWorkspace,
    path: &std::path::Path,
    model: &str,
    expected: &str,
) -> u32 {
    let mut cli = spawn_scode_in_dir_with_env(
        &workspace.root,
        &[
            "--auth",
            "api-key",
            "--model",
            model,
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
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let screen = cli.render(|screen| screen.contents());
        if common::screen_contains(&screen, expected) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected {expected}\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
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
fn compaction_uses_fixed_output_ceiling_and_summary_length_guidance() {
    for (mode, configured_limit, expected_limit) in [
        ("long-summary", None, 12_000),
        ("long-summary", Some(16_384), 12_000),
        ("long-summary", Some(10_000), 10_000),
        ("long-summary-fallback", Some(16_384), 12_000),
    ] {
        let provider = Provider::new(mode);
        let workspace = HarnessWorkspace::new(mode);
        workspace.write_mock_config(&provider.url);
        if let Some(limit) = configured_limit {
            let config_path = workspace.config_home.join("sudocode.json");
            let mut config: Value =
                serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
            config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(limit);
            std::fs::write(config_path, config.to_string()).unwrap();
        }
        let path = fixture(&workspace);
        assert_eq!(compact(&workspace, &path, "Messages removed"), 0);
        let restored = Session::load_from_path(&path).unwrap();
        let summary = &restored.compaction.as_ref().unwrap().summary;
        assert!(summary.len() > 8_192 * 4);
        assert!(summary.ends_with("CHECKPOINT_COMPLETE</summary>"));
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            if mode.ends_with("fallback") { 2 } else { 1 }
        );
        assert!(requests.iter().all(|r| r["max_tokens"] == expected_limit));
        assert!(requests.iter().all(|r| {
            r["messages"].as_array().unwrap().last().unwrap()["content"]
                .to_string()
                .contains("Aim to keep the entire summary within 8,000 tokens")
        }));
        assert!(requests[0]["tools"].is_array());
        if mode.ends_with("fallback") {
            assert!(requests[1]["tools"].is_null());
        }
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

/// TodoWrite is whole-list-replace with no read tool, so the model needs the
/// exact list back in context after compaction discards the old TodoWrite
/// messages. Seed a todo store, compact, and assert the compacted history
/// carries the structured list forward (CC todo-continuity parity).
#[test]
fn compaction_carries_the_todo_list_forward() {
    let provider = Provider::new("success");
    let workspace = HarnessWorkspace::new("todo-continuity");
    workspace.write_mock_config(&provider.url);

    // Seed the todo store the running CLI will resolve (`<cwd>/.sudocode-todos.json`).
    std::fs::write(
        workspace.root.join(".sudocode-todos.json"),
        json!([
            {"content": "Wire the parser", "status": "completed", "activeForm": "Wiring the parser"},
            {"content": "TODO_SENTINEL_XYZ finish the reducer", "status": "in_progress", "activeForm": "Finishing the reducer"},
            {"content": "Ship the release", "status": "pending", "activeForm": "Shipping the release"}
        ])
        .to_string(),
    )
    .unwrap();

    let path = fixture(&workspace);
    assert_eq!(compact(&workspace, &path, "Messages removed"), 0);

    let restored = Session::load_from_path(&path).unwrap();
    let transcript = restored
        .messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        transcript.contains("TODO_SENTINEL_XYZ finish the reducer"),
        "compacted history must carry the exact todo content forward:\n{transcript}"
    );
    assert!(
        transcript.contains("[in_progress]") && transcript.contains("[completed]"),
        "todo statuses must survive the compaction boundary:\n{transcript}"
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
fn automatic_compaction_continues_after_a_long_summary() {
    let provider = Provider::new("long-summary");
    let workspace = HarnessWorkspace::new("automatic-long-summary");
    workspace.write_mock_config(&provider.url);
    let config_path = workspace.config_home.join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    // Trigger on the original history, leaving enough room below the pressure
    // threshold for a 10K checkpoint plus the preserved recent messages.
    config["models"]["claude-sonnet"]["contextWindow"] = json!(64_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(16_384);
    std::fs::write(config_path, config.to_string()).unwrap();
    let path = fixture(&workspace);
    let mut session = Session::load_from_path(&path).unwrap();
    for message in &mut session.messages {
        for block in &mut message.blocks {
            if let ContentBlock::Text { text } = block {
                *text = text.repeat(2);
            }
        }
    }
    session.save_to_path(&path).unwrap();

    let mut cli = resume(&workspace, &path);
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA using all earlier decisions.")
        .unwrap();
    common::expect_input_line(
        &cli,
        "Continue PROJECT_ALPHA",
        Duration::from_secs(30),
        "typed turn landed",
    );
    cli.send("\r").unwrap();
    cli.expect("ALPHA_CONTEXT_OK").unwrap();
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let restored = Session::load_from_path(&path).unwrap();
    assert!(restored
        .compaction
        .as_ref()
        .unwrap()
        .summary
        .ends_with("CHECKPOINT_COMPLETE</summary>"));
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|r| r["stream"] != true).count(), 1);
    assert!(requests
        .iter()
        .all(|r| { r["max_tokens"] == if r["stream"] == true { 16_384 } else { 12_000 } }));
    assert!(requests
        .iter()
        .any(|r| r["stream"] == true && r["messages"].to_string().contains("CHECKPOINT_COMPLETE")));
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
    // Pruning happens on a staged clone before the failing model call. The
    // remaining text is still over budget, so failure must restore this output too.
    original
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: "staged-read".into(),
                name: "Read".into(),
                input: "{}".into(),
                thought_signature: None,
            },
        ]))
        .unwrap();
    original
        .push_message(ConversationMessage::tool_result(
            "staged-read",
            "Read",
            "preserve the original tool output ".repeat(2000),
            false,
        ))
        .unwrap();
    original.save_to_path(&path).unwrap();
    let mut cli = resume(&workspace, &path);
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA using all earlier decisions.")
        .unwrap();
    common::expect_input_line(
        &cli,
        "Continue PROJECT_ALPHA",
        Duration::from_secs(30),
        "typed turn landed",
    );
    cli.send("\r").unwrap();
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
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA.").unwrap();
    common::expect_input_line(
        &cli,
        "Continue PROJECT_ALPHA",
        Duration::from_secs(30),
        "typed turn landed",
    );
    cli.send("\r").unwrap();
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
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA.").unwrap();
    common::expect_input_line(
        &cli,
        "Continue PROJECT_ALPHA",
        Duration::from_secs(30),
        "typed turn landed",
    );
    cli.send("\r").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let screen = cli.render(|s| s.contents());
        if common::screen_contains(&screen, "no active history") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected no active history\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    cli.send("/exit").unwrap();
    common::expect_input_line(&cli, "/exit", Duration::from_secs(30), "exit typed");
    cli.send("\r").unwrap();
    cli.expect_eof().unwrap();
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "damaged history must never reach the model"
    );
    assert!(Session::load_from_path(&path).unwrap().messages.is_empty());
}

// Drive both runtime clients through the real Agent tool. Two closed read
// exchanges leave a compactable prefix; only the child's final response reports
// pressure, so its post-turn checkpoint must use the shared text transport.
fn serve_delegation(socket: &mut TcpStream, request: &Value) {
    let messages = request["messages"].as_array().unwrap();
    let child = messages.iter().any(|message| {
        message["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("CHILD_COMPACTION"))
            })
        })
    });
    let steps = messages
        .iter()
        .filter(|message| message["role"] == "assistant")
        .count();
    let (block, usage) = if child && steps < 2 {
        (
            json!({"type":"tool_use","id":format!("read-{steps}"),"name":"read_file","input":{"path":"child-read.txt"}}),
            1000,
        )
    } else if child {
        (json!({"type":"text","text":"CHILD_DONE"}), 49_000)
    } else if steps == 0 {
        (
            json!({"type":"tool_use","id":"delegate","name":"Agent","input":{
                "description":"validate child checkpoint",
                "run_in_background":false,
                "prompt":format!("CHILD_COMPACTION PROJECT_ALPHA {}", "Preserve the migration constraints. ".repeat(300))
            }}),
            1000,
        )
    } else {
        (json!({"type":"text","text":"PARENT_DONE"}), 1000)
    };
    let is_tool = block["type"] == "tool_use";
    let mut start = block.clone();
    let delta = if is_tool {
        start["input"] = json!({});
        json!({"type":"input_json_delta","partial_json":block["input"].to_string()})
    } else {
        start["text"] = json!("");
        json!({"type":"text_delta","text":block["text"]})
    };
    let events = [
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"delegate-reply","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":usage,"output_tokens":0}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":start}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":delta}),
        ),
        (
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":if is_tool {"tool_use"} else {"end_turn"}},"usage":{"input_tokens":usage,"output_tokens":10}}),
        ),
        ("message_stop", json!({"type":"message_stop"})),
    ];
    let mut body = String::new();
    for (event, data) in events {
        write!(&mut body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
}

#[test]
fn subagent_compaction_uses_shared_text_transport() {
    let provider = Provider::new("delegate");
    let workspace = HarnessWorkspace::new("shared-subagent-compact");
    workspace.write_mock_config(&provider.url);
    set_small_window(&workspace);
    let config_path = workspace.config_home.join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["providers"]["api-key"]["model"] = json!("intranet/child-v1");
    config["models"]["claude-sonnet"]["providers"]["api-key"]["api"] = json!("anthropic-messages");
    std::fs::write(config_path, config.to_string()).unwrap();
    std::fs::write(
        workspace.root.join("child-read.txt"),
        "alpha migration fixture\n".repeat(120),
    )
    .unwrap();
    let mut cli = spawn_scode_in_dir_with_env(
        &workspace.root,
        &[
            "--auth",
            "api-key",
            "--model",
            "claude-sonnet",
            "--permission-mode",
            "danger-full-access",
            "Delegate the checkpoint validation.",
        ],
        Duration::from_secs(30),
        &[
            ("SUDO_CODE_CONFIG_HOME", &workspace.config_home),
            ("HOME", &workspace.home),
        ],
    )
    .unwrap();
    cli.expect("PARENT_DONE").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let requests = provider.requests.lock().unwrap();
    let checkpoints = requests
        .iter()
        .filter(|r| r["stream"] != true)
        .collect::<Vec<_>>();
    assert_eq!(
        checkpoints.len(),
        1,
        "child should complete cache-preserving compaction in one request; requests: {:?}",
        requests
            .iter()
            .map(|r| (
                r["stream"].clone(),
                r["model"].clone(),
                r["messages"].as_array().map(Vec::len),
                r["messages"].as_array().and_then(|m| m.last()).map(|m| m
                    .to_string()
                    .chars()
                    .take(1200)
                    .collect::<String>())
            ))
            .collect::<Vec<_>>()
    );
    // No model was specified in the spawn. Both the child and its compaction
    // must use the parent's configured route, not the response's model label.
    assert!(requests.iter().all(|r| r["model"] == "intranet/child-v1"));
    let manifests = std::fs::read_dir(workspace.root.join(".sudocode-agents"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| {
            serde_json::from_slice::<Value>(&std::fs::read(entry.path()).unwrap()).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["model"], "claude-sonnet");
    assert_eq!(manifests[0]["status"], "completed");
    let checkpoint = checkpoints[0];
    assert!(checkpoint["messages"]
        .to_string()
        .contains("CHILD_COMPACTION"));
    assert_eq!(checkpoint["model"], "intranet/child-v1");
    assert_eq!(checkpoint["max_tokens"], 1024);
    assert!(checkpoint["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty()));
    assert!(
        checkpoint["system"].to_string().contains("cache_control"),
        "child should use the shared cache hints"
    );
    assert!(
        requests
            .iter()
            .filter(|r| r["stream"] == true)
            .any(|r| r["messages"].to_string().contains("CHILD_DONE")),
        "parent must receive the child result"
    );
}

#[test]
fn internal_model_alias_and_resumed_model_use_active_wire_identity() {
    for (mode, wire_model, saved_model) in [
        ("internal", "apeiron-v1", "apeiron-alias"),
        (
            "internal-prefixed",
            "intranet/apeiron-v1",
            "claude-sonnet-4-6",
        ),
    ] {
        let provider = Provider::new(mode);
        let workspace = HarnessWorkspace::new(mode);
        workspace.write_mock_config(&provider.url);
        let config_path = workspace.config_home.join("sudocode.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let mut model = config["models"]["claude-sonnet"].clone();
        model["alias"] = json!("apeiron-alias");
        model["name"] = json!("Apeiron display label, not a routing ID");
        model["contextWindow"] = json!(50_000);
        model["maxOutputTokens"] = json!(1024);
        for mapping in model["providers"].as_object_mut().unwrap().values_mut() {
            mapping["model"] = json!(wire_model);
            mapping["api"] = json!("anthropic-messages");
        }
        config["models"]["apeiron-alias"] = model;
        std::fs::write(config_path, config.to_string()).unwrap();
        let path = fixture(&workspace);
        let mut session = Session::load_from_path(&path).unwrap();
        session.model = Some(saved_model.into());
        session.save_to_path(&path).unwrap();
        assert_eq!(
            compact_with_model(&workspace, &path, "apeiron-alias", "Messages removed"),
            0
        );
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "configured checkpoint should succeed on its first attempt"
        );
        assert_eq!(requests[0]["model"], wire_model);
        assert_eq!(requests[0]["max_tokens"], 1024);
        assert!(Session::load_from_path(&path).unwrap().compaction.is_some());
    }
}

#[test]
fn openai_compaction_validates_responses_and_preserves_history_on_failure() {
    for mode in [
        "openai-success",
        "openai-fallback",
        "openai-empty",
        "openai-truncated",
        "openai-tool",
    ] {
        let provider = Provider::new(mode);
        let workspace = HarnessWorkspace::new(mode);
        workspace.write_mock_config(&provider.url);
        let config_path = workspace.config_home.join("sudocode.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let model = &mut config["models"]["claude-sonnet"];
        model["contextWindow"] = json!(50_000);
        model["maxOutputTokens"] = json!(1024);
        model["providers"]["api-key"]["model"] = json!("intranet/apeiron-openai");
        model["providers"]["api-key"]["api"] = json!("openai-completions");
        std::fs::write(config_path, config.to_string()).unwrap();
        let path = fixture(&workspace);
        let original = std::fs::read(&path).unwrap();
        let succeeds = matches!(mode, "openai-success" | "openai-fallback");
        let exit = compact_with_model(
            &workspace,
            &path,
            "claude-sonnet",
            if succeeds {
                "Messages removed"
            } else {
                "history preserved"
            },
        );
        assert_eq!(exit == 0, succeeds, "{mode}");
        if succeeds {
            let restored = Session::load_from_path(&path).unwrap();
            assert!(restored
                .compaction
                .unwrap()
                .summary
                .contains("PROJECT_ALPHA"));
            assert!(restored.messages.len() < 32);
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), original, "{mode}");
        }
        assert!(!workspace.root.join("SHOULD_NOT_EXIST").exists());
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            if mode == "openai-success" { 1 } else { 2 },
            "{mode}"
        );
        for request in requests.iter() {
            assert_eq!(request["model"], "intranet/apeiron-openai");
            assert_eq!(request["max_tokens"], 1024);
            assert_ne!(request["stream"], true);
            assert!(request["messages"].to_string().contains("PROJECT_ALPHA"));
        }
        if mode == "openai-fallback" {
            assert!(requests[0]["tools"].is_array());
            assert!(requests[1]["tools"].is_null());
        }
    }
}

#[test]
fn post_turn_compaction_failure_stops_the_cli_turn() {
    let provider = Provider::new("post-error");
    let workspace = HarnessWorkspace::new("post-turn-compact-failure");
    workspace.write_mock_config(&provider.url);
    let path = fixture(&workspace);
    let before = Session::load_from_path(&path).unwrap();
    let mut cli = spawn_scode_in_dir_with_env(
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
            (
                "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS",
                std::path::Path::new("100"),
            ),
        ],
    )
    .unwrap();
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA").unwrap();
    common::expect_input_line(
        &cli,
        "Continue PROJECT_ALPHA",
        Duration::from_secs(30),
        "typed turn landed",
    );
    cli.send("\r").unwrap();
    cli.expect("Context compaction failed")
        .unwrap_or_else(|error| {
            panic!(
                "{error}\n{}\nrequests: {:?}",
                cli.render(|screen| screen.contents()),
                provider
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|request| request["stream"].clone())
                    .collect::<Vec<_>>()
            );
        });
    cli.send("/exit\r").unwrap();
    cli.expect_eof().unwrap();
    let after = Session::load_from_path(&path).unwrap();
    assert_eq!(&after.messages[..before.messages.len()], &before.messages);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.iter().filter(|r| r["stream"] == true).count(),
        1,
        "no further task request after compaction fails"
    );
    assert!(requests.iter().any(|r| r["stream"] != true));
}
