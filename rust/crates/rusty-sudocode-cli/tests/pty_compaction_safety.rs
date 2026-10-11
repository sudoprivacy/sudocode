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

use common::{spawn_scode_in_dir_with_env, HarnessWorkspace, TestEnv};
use runtime::{
    estimate_session_tokens, get_compact_continuation_message, ContentBlock, ConversationMessage,
    Session,
};
use serde_json::{json, Value};

struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stopped: Arc<AtomicBool>,
    response_released: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

/// The opening words of the checkpoint instructions in
/// `runtime::compact::BASE_COMPACT_PROMPT`.
const COMPACTION_PROMPT: &str = "Create a concise checkpoint";

/// Is this captured request a compaction call rather than a task turn?
///
/// It used to be enough to ask whether the request was non-streaming, because
/// compaction was the only non-streaming request the CLI made. Compaction is
/// streamed now — it is the largest and slowest request in a session, and a
/// non-streaming one has its connection closed with no HTTP response at all
/// after ~50s — so the transport no longer separates the two. Key on the
/// checkpoint instructions, which is what actually makes a request a
/// compaction, and which no task turn carries.
fn is_compaction_request(request: &Value) -> bool {
    request.to_string().contains(COMPACTION_PROMPT)
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
        let response_released = Arc::new(AtomicBool::new(mode != "budget-revision-conflict"));
        let release = Arc::clone(&response_released);
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((socket, _)) => serve(
                        socket,
                        mode,
                        &captured,
                        read_only_dir.as_deref(),
                        &release,
                        &stop,
                    ),
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
            response_released,
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
    response_released: &AtomicBool,
    stopped: &AtomicBool,
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
    let streaming = request["stream"] == true || first.contains(":streamGenerateContent");
    let compaction = is_compaction_request(&request);
    let number = if is_post {
        let mut requests = captured.lock().unwrap();
        requests.push(request.clone());
        requests.len()
    } else {
        0
    };
    if is_post && compaction && mode == "budget-revision-conflict" {
        while !response_released.load(Ordering::Relaxed) {
            if stopped.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    // A task turn gets the generic reply; the canned checkpoint bodies below are
    // for compaction. Both arrive streamed now, so the checkpoint instructions
    // are what tells them apart.
    if !compaction && streaming && mode == "delegate" {
        serve_delegation(&mut socket, &request);
        return;
    }
    if !compaction
        && streaming
        && (matches!(mode, "success" | "post-error")
            || mode.starts_with("long-summary")
            || mode.starts_with("budget-"))
    {
        serve_success_stream(&mut socket);
        return;
    }
    let (status, response) = if counting {
        ("200 OK", json!({"input_tokens": 1000}))
    } else if !is_post {
        ("200 OK", json!({"data": []}))
    } else if mode == "budget-transport-exhausted"
        || (mode == "budget-transport-fallback" && number <= 2)
    {
        (
            "503 Service Unavailable",
            json!({"type":"error", "error":{"type":"overloaded_error", "message":"fixture transient overload"}}),
        )
    } else if mode.starts_with("gemini-") {
        let mut candidate = json!({"content":{"role":"model","parts":[{"text":"<summary>PROJECT_ALPHA complete checkpoint.</summary>"}]}});
        if mode != "gemini-missing-terminal" {
            candidate["finishReason"] = json!(if mode == "gemini-truncated" {
                "MAX_TOKENS"
            } else {
                "STOP"
            });
        }
        (
            "200 OK",
            json!({"candidates":[candidate],"usageMetadata":{"promptTokenCount":1000,"candidatesTokenCount":50}}),
        )
    } else if mode.starts_with("codex-") {
        (
            "200 OK",
            json!({"usage":{"input_tokens":1000,"output_tokens":50}}),
        )
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
            json!({"type":"error", "error":{"type":"invalid_request_error", "message":"fixture cache-safe compaction not supported"}}),
        )
    } else if mode == "auth-unsupported" {
        (
            "401 Unauthorized",
            json!({"type":"error", "error":{"type":"authentication_error", "message":"fixture auth scheme not supported; cache-safe compaction not supported"}}),
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
            // The transport deliberately supplies complete but oversized
            // checkpoints. The runtime must validate the installed history,
            // even when a gateway ignores the requested visible-text budget.
            "budget-weak-then-target" if number == 1 => framed_fixture_summary(8_999),
            "budget-transport-fallback" if number == 3 => framed_fixture_summary(8_999),
            "budget-weak-then-target" | "budget-transport-fallback" => {
                framed_fixture_summary(1_000)
            }
            "budget-over-target" => framed_fixture_summary(6_000),
            "budget-small-output" => framed_fixture_summary(400),
            "budget-delayed" => {
                std::thread::sleep(Duration::from_secs(4));
                framed_fixture_summary(1_000)
            }
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
    // A 200 answering a streamed request has to be a stream. Reaching the
    // client through the "gateway ignored `stream: true`" fallback instead would
    // make every assertion here depend on that fallback staying in place.
    // Error statuses stay JSON: that is what a real gateway sends for them,
    // streamed request or not.
    let (content_type, body) = if is_post && streaming && status == "200 OK" {
        let stream = if mode == "budget-usage-then-malformed" {
            usage_then_malformed_sse()
        } else if mode.starts_with("gemini-") {
            format!("data: {response}\n\n")
        } else if mode.starts_with("codex-") {
            codex_checkpoint_sse(mode, &response)
        } else if mode.starts_with("openai-") {
            openai_sse_body(&response)
        } else {
            anthropic_sse_body(&response)
        };
        ("text/event-stream", stream)
    } else {
        ("application/json", response.to_string())
    };
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    let _ = socket.write_all(response.as_bytes());
}

/// Re-shape a canned `/v1/messages` body into the SSE stream a gateway would
/// have sent for it: no content on `message_start`, one block per text block,
/// and `stop_reason`/`usage` on `message_delta`.
fn anthropic_sse_body(message: &Value) -> String {
    let blocks = message["content"].as_array().cloned().unwrap_or_default();
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    let mut events = vec![(
        "message_start",
        json!({"type":"message_start","message":start}),
    )];
    for (index, block) in blocks.iter().enumerate() {
        events.push((
            "content_block_start",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        ));
        events.push((
            "content_block_delta",
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}),
        ));
        events.push((
            "content_block_stop",
            json!({"type":"content_block_stop","index":index}),
        ));
    }
    events.push((
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":message["stop_reason"],"stop_sequence":null},"usage":message["usage"]}),
    ));
    events.push(("message_stop", json!({"type":"message_stop"})));
    let mut body = String::new();
    for (event, data) in &events {
        write!(&mut body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    body
}

fn codex_checkpoint_sse(mode: &str, response: &Value) -> String {
    let mut body = String::new();
    for (event, data) in [
        (
            "response.created",
            json!({"id":"codex-checkpoint","model":"fixture-wire"}),
        ),
        (
            "response.output_text.delta",
            json!({"delta":"<summary>PROJECT_ALPHA complete checkpoint.</summary>"}),
        ),
        ("response.output_text.done", json!({})),
    ] {
        write!(&mut body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    if mode != "codex-missing-terminal" {
        let event = if mode == "codex-truncated" {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut result = response.clone();
        if mode == "codex-truncated" {
            result["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        write!(
            &mut body,
            "event: {event}\ndata: {}\n\n",
            json!({"response":result})
        )
        .unwrap();
    }
    body
}

fn usage_then_malformed_sse() -> String {
    let mut body = String::new();
    for (event, data) in [
        (
            "message_start",
            json!({"type":"message_start","message":{"id":"usage-before-bad-frame","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1000,"output_tokens":0,"cost_units":10,"cost_currency":"sudo_point"}}}),
        ),
        (
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        (
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<summary>PROJECT_ALPHA checkpoint</summary>"}}),
        ),
        (
            "message_delta",
            json!({"type":"message_delta","delta":{},"usage":{"input_tokens":1000,"output_tokens":50,"cost_units":20,"cost_currency":"sudo_point"}}),
        ),
    ] {
        write!(&mut body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    // Send all frames in the same socket write. The parser must retain the
    // already-decoded usage even when the remainder of that chunk is invalid.
    body.push_str("event: message_delta\ndata: {malformed-json\n\n");
    body
}

/// Re-shape a canned chat completion into the `chat.completion.chunk` stream a
/// gateway would have sent for it: the whole message as one delta, then the
/// finish reason and usage, then `[DONE]`.
fn openai_sse_body(completion: &Value) -> String {
    let choice = &completion["choices"][0];
    let mut delta = json!({"role":"assistant","content":choice["message"]["content"]});
    if let Some(calls) = choice["message"]["tool_calls"].as_array() {
        delta["tool_calls"] = Value::Array(
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| {
                    let mut call = call.clone();
                    call["index"] = json!(index);
                    call
                })
                .collect(),
        );
    }
    let chunk = |choices: Value, usage: &Value| {
        json!({"id":completion["id"],"object":"chat.completion.chunk","created":0,
               "model":completion["model"],"choices":choices,"usage":usage})
    };
    let content = chunk(
        json!([{"index":0,"delta":delta,"finish_reason":Value::Null}]),
        &Value::Null,
    );
    let end = chunk(
        json!([{"index":0,"delta":{},"finish_reason":choice["finish_reason"]}]),
        &completion["usage"],
    );
    format!("data: {content}\n\ndata: {end}\n\ndata: [DONE]\n\n")
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
            json!({"error":{"message":"cache-safe compaction not supported","type":"invalid_request_error"}}),
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

/// Exact local-estimate fixture, driven through the real CLI rather than
/// invoking compaction helpers directly. These numbers are NOT a claim about
/// the provider's tokenizer: the subsequent request is checked independently.
fn budget_fixture(env: &TestEnv) -> PathBuf {
    let mut session = Session::new().with_workspace_root(env.workspace_root());
    session.model = Some("claude-sonnet-4-6".into());
    for index in 0..10 {
        let prefix = format!("PROJECT_ALPHA decision {index}: ");
        let text = prefix.clone() + &"x".repeat(5_997 - prefix.len());
        let message = if index % 2 == 0 {
            ConversationMessage::user_text(text)
        } else {
            ConversationMessage::assistant(vec![ContentBlock::Text { text }])
        };
        session.push_message(message).unwrap();
    }
    assert_eq!(estimate_session_tokens(&session), 15_000);
    let path = env.workspace_root().join("history.jsonl");
    session.save_to_path(&path).unwrap();
    path
}

fn budget_env(label: &str, provider: &Provider) -> TestEnv {
    // Exact counts and malformed/oversized provider bodies require the
    // deterministic backend; TestEnv still isolates instructions and config.
    let env = TestEnv::new_mock(label);
    let sample = runtime::SAMPLE_SUDOCODE_JSON
        .replace("https://api.anthropic.com", &provider.url)
        .replace("<YOUR_ANTHROPIC_API_KEY>", "test-compaction-budget-key");
    let mut config: Value = serde_json::from_str(&sample).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(64_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(16_384);
    std::fs::write(env.config_home().join("sudocode.json"), config.to_string()).unwrap();
    env
}

fn framed_fixture_summary(tokens: usize) -> String {
    let summary = "<summary>PROJECT_ALPHA CHECKPOINT_COMPLETE</summary>";
    let estimate = |summary: &str| {
        let mut session = Session::new();
        session.messages = vec![ConversationMessage::user_text(
            get_compact_continuation_message(summary, true, true, None),
        )];
        estimate_session_tokens(&session)
    };
    let base = estimate(summary);
    assert!(tokens >= base);
    let summary = format!(
        "<summary>PROJECT_ALPHA {}CHECKPOINT_COMPLETE</summary>",
        "x".repeat((tokens - base) * 4)
    );
    assert_eq!(estimate(&summary), tokens);
    summary
}

fn rendered_contents(screen: &pty_expect::Screen<'_>) -> String {
    screen.contents()
}

fn compact_env(env: &TestEnv, path: &std::path::Path) -> (u32, String) {
    compact_env_with_auth(env, path, "api-key")
}

fn compact_env_with_auth(env: &TestEnv, path: &std::path::Path, auth: &str) -> (u32, String) {
    let mut cli = spawn_scode_in_dir_with_env(
        env.workspace_root(),
        &[
            "--auth",
            auth,
            "--model",
            "sonnet",
            "--resume",
            path.to_str().unwrap(),
            "/compact",
        ],
        Duration::from_secs(30),
        &[
            ("SUDO_CODE_CONFIG_HOME", env.config_home()),
            ("HOME", &env.workspace_root().join("home")),
        ],
    )
    .unwrap();
    cli.set_default_timeout(Duration::from_secs(30));
    let code = cli.expect_eof().unwrap_or_else(|error| {
        panic!(
            "compact did not terminate: {error}; {}",
            cli.render(rendered_contents)
        )
    });
    (code, cli.render(rendered_contents))
}

fn compaction_request_count(provider: &Provider) -> usize {
    provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| is_compaction_request(request))
        .count()
}

fn redacted_live_screen(env: &TestEnv, mut screen: String) -> String {
    let config: Value =
        serde_json::from_slice(&std::fs::read(env.config_home().join("sudocode.json")).unwrap())
            .unwrap();
    if let Some(modes) = config["auth_modes"].as_object() {
        for profiles in modes.values().filter_map(Value::as_object) {
            for profile in profiles.values() {
                for key in ["apiKey", "token"] {
                    if let Some(secret) = profile[key].as_str().filter(|value| !value.is_empty()) {
                        screen = screen.replace(secret, "[redacted]");
                    }
                }
            }
        }
    }
    for (name, secret) in std::env::vars() {
        if (name.contains("TOKEN")
            || name.ends_with("_KEY")
            || name.contains("SECRET")
            || name.contains("PASSWORD"))
            && !secret.is_empty()
        {
            screen = screen.replace(&secret, "[redacted]");
        }
    }
    screen
}

#[test]
fn manual_compact_retries_a_one_token_reduction_before_committing_the_target() {
    let provider = Provider::new("budget-weak-then-target");
    let env = budget_env("one-token-is-not-success", &provider);
    let path = budget_fixture(&env);
    let original = std::fs::read(&path).unwrap();
    let (code, screen) = compact_env(&env, &path);
    assert_eq!(code, 0, "{screen}");
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(estimate_session_tokens(&restored), 7_000);
    assert!(estimate_session_tokens(&restored) <= 15_000 / 2);
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::TargetMet);
    assert_eq!(report.before_history, 15_000);
    assert_eq!(report.after_history, 7_000);
    assert_eq!(report.target_history, Some(7_500));
    assert_eq!(report.ideal_history, Some(4_500));
    assert_eq!(report.completed_responses, 2);
    assert_eq!(report.attempts, 2);
    assert!(report.actual_fixed_overhead > 0);
    assert_eq!(restored.maintenance_usage.len(), 2);
    assert!(
        restored.maintenance_usage.iter().all(|receipt| {
            receipt
                .usage
                .is_some_and(|usage| usage.input_tokens == 1_000 && usage.output_tokens == 50)
        }),
        "both the rejected checkpoint and accepted checkpoint remain billable"
    );
    assert_eq!(compaction_request_count(&provider), 2);
    let requests = provider.requests.lock().unwrap();
    assert!(requests.iter().all(|request| {
        is_compaction_request(request)
            && request["messages"].to_string().contains("decision 0")
            && request["messages"].to_string().contains("decision 5")
    }));
    let first_cap = requests[0]["max_tokens"].as_u64().unwrap();
    assert!(first_cap < 12_000, "small histories need a dynamic cap");
    assert_eq!(requests[0]["thinking"]["budget_tokens"], json!(8_192));
    assert!(first_cap > 8_192, "thinking must retain visible-text room");
    assert!(requests[0]["tools"].is_array());
    if requests[1]["tools"].is_array() {
        assert!(
            requests[0]["tools"] == requests[1]["tools"],
            "cached quality retry must keep tool schemas"
        );
        assert!(
            requests[0]["system"] == requests[1]["system"],
            "cached quality retry must keep the system prefix"
        );
    } else {
        assert!(requests[1]["tools"].is_null());
        assert!(requests[1]["thinking"].is_null());
    }
    let second_cap = requests[1]["max_tokens"].as_u64().unwrap();
    assert!(
        second_cap > 0 && second_cap < first_cap - 8_192,
        "quality retry must tighten the visible summary cap"
    );
    drop(requests);
    let archives: Vec<_> = std::fs::read_dir(env.workspace_root())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("before-compact")
        })
        .collect();
    assert_eq!(archives.len(), 1, "only the accepted replacement commits");
    assert_eq!(std::fs::read(archives[0].path()).unwrap(), original);
}

#[test]
fn safe_but_over_target_checkpoints_fail_and_preserve_history() {
    let provider = Provider::new("budget-over-target");
    let env = budget_env("safe-is-not-target-met", &provider);
    let path = budget_fixture(&env);
    let original = Session::load_from_path(&path).unwrap();
    let (code, screen) = compact_env(&env, &path);
    assert_ne!(
        code, 0,
        "12K is under the hard window but over the 7.5K target: {screen}"
    );
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(restored.messages, original.messages);
    assert_eq!(restored.compaction, original.compaction);
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::Failed);
    assert_eq!(report.before_history, 15_000);
    assert_eq!(report.target_history, Some(7_500));
    assert_eq!(report.completed_responses, 2);
    assert_eq!(restored.maintenance_usage.len(), 2);
    assert!(restored
        .maintenance_usage
        .iter()
        .all(|receipt| receipt.usage.is_some()));
    assert_eq!(compaction_request_count(&provider), 2);
    assert!(screen.contains("history preserved"), "{screen}");
    assert!(!std::fs::read_dir(env.workspace_root())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry
            .file_name()
            .to_string_lossy()
            .contains("before-compact")));
}

#[test]
fn immediately_repeating_a_met_compaction_skips_until_its_budget_changes() {
    let provider = Provider::new("budget-weak-then-target");
    let env = budget_env("met-compaction-fingerprint", &provider);
    let path = budget_fixture(&env);
    assert_eq!(compact_env(&env, &path).0, 0);
    let first = Session::load_from_path(&path).unwrap();
    let requests = compaction_request_count(&provider);
    let (code, screen) = compact_env(&env, &path);
    assert_eq!(code, 0, "{screen}");
    assert_eq!(compaction_request_count(&provider), requests);
    let second = Session::load_from_path(&path).unwrap();
    assert_eq!(second.messages, first.messages);
    assert_eq!(second.compaction, first.compaction);
    assert_eq!(second.maintenance_usage, first.maintenance_usage);

    // A Todo change changes the actual continuation overhead. A cached met
    // result cannot be reused solely because the messages are unchanged.
    std::fs::write(
        env.workspace_root().join(".sudocode-todos.json"),
        json!([{"content":"TODO_CHANGED_SENTINEL verify the release","status":"pending","activeForm":"Verifying the release"}]).to_string(),
    ).unwrap();
    let (code, screen) = compact_env(&env, &path);
    assert_ne!(
        code, 0,
        "the changed Todo must invalidate the met fingerprint: {screen}"
    );
    let changed = Session::load_from_path(&path).unwrap();
    let report = changed.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::Failed);
    assert_eq!(report.before_history, estimate_session_tokens(&first));
    assert_eq!(report.target_history, Some(report.before_history / 2));
    assert_eq!(changed.messages, first.messages);
    // The already compacted four-message tail cannot meet a second halving,
    // so invalidating the fingerprint fails locally before billing a request.
    assert_eq!(compaction_request_count(&provider), requests);
}

#[test]
fn transport_retries_share_the_operation_budget_and_keep_source_history() {
    let provider = Provider::new("budget-transport-fallback");
    let env = budget_env("global-compaction-attempt-budget", &provider);
    let path = budget_fixture(&env);
    let (code, screen) = compact_env(&env, &path);
    assert_eq!(code, 0, "{screen}");
    let restored = Session::load_from_path(&path).unwrap();
    assert!(estimate_session_tokens(&restored) <= 7_500);
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.attempts, 4);
    assert_eq!(report.completed_responses, 2);
    assert_eq!(restored.maintenance_usage.len(), 4);
    assert_eq!(
        restored
            .maintenance_usage
            .iter()
            .filter(|receipt| receipt.usage.is_some())
            .count(),
        2
    );
    assert_eq!(
        restored
            .maintenance_usage
            .iter()
            .filter(|receipt| receipt.usage.is_none())
            .count(),
        2,
        "transport failure bills are unknown, rather than invented zero usage"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "all fallback paths share one operation limit"
    );
    assert!(requests.iter().all(|request| {
        is_compaction_request(request) && request["messages"].to_string().contains("decision 0")
    }));
}

#[test]
fn exhausted_network_retries_do_not_restart_the_budget_in_the_fallback() {
    let provider = Provider::new("budget-transport-exhausted");
    let env = budget_env("network-retry-limit-is-global", &provider);
    let path = budget_fixture(&env);
    let old_usage = runtime::TokenUsage {
        input_tokens: 1_000,
        output_tokens: 50,
        cost_units: Some(100),
        cost_currency: Some(runtime::UsageCostCurrency::SudoPoint),
        ..runtime::TokenUsage::default()
    };
    let mut original = Session::load_from_path(&path).unwrap();
    original.messages.last_mut().unwrap().usage = Some(old_usage);
    original.save_to_path(&path).unwrap();
    assert_eq!(
        runtime::UsageTracker::from_session(&original)
            .cumulative_usage()
            .cost_units,
        Some(100)
    );
    let (code, screen) = compact_env(&env, &path);
    assert_ne!(code, 0, "{screen}");
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(restored.messages, original.messages);
    assert_eq!(restored.compaction, original.compaction);
    assert_eq!(
        compaction_request_count(&provider),
        3,
        "one initial request plus two network retries"
    );
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::Failed);
    assert_eq!(report.attempts, 3);
    assert_eq!(report.completed_responses, 0);
    assert_eq!(restored.maintenance_usage.len(), 3);
    assert!(restored
        .maintenance_usage
        .iter()
        .all(|receipt| receipt.usage.is_none()));
    let mut after = env.spawn(&["--resume", path.to_str().unwrap()]);
    common::expect_input_line_cleared(
        &after,
        Duration::from_secs(30),
        "resume unknown compaction bills",
    );
    after.send("/exit\r").unwrap();
    assert_eq!(after.expect_eof().unwrap(), 0);
    let tracker = runtime::UsageTracker::from_session(&Session::load_from_path(&path).unwrap());
    assert_eq!(tracker.current_turn_usage(), old_usage);
    assert_eq!(tracker.turns(), 1);
    assert_eq!(tracker.cumulative_usage().cost_units, None);
    assert_eq!(tracker.cumulative_usage().cost_currency, None);
}

#[test]
fn usage_preceding_a_malformed_stream_frame_is_billable_without_installing_text() {
    let provider = Provider::new("budget-usage-then-malformed");
    let env = budget_env("usage-before-invalid-frame", &provider);
    let path = budget_fixture(&env);
    let original = Session::load_from_path(&path).unwrap();
    let (code, screen) = compact_env(&env, &path);
    assert_ne!(code, 0, "{screen}");
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(restored.messages, original.messages);
    assert_eq!(restored.compaction, original.compaction);
    assert!(!restored.maintenance_usage.is_empty());
    assert!(
        restored.maintenance_usage.iter().all(|receipt| {
            receipt
                .usage
                .is_some_and(|usage| usage.input_tokens == 1_000 && usage.output_tokens == 50)
        }),
        "a later parse error cannot erase the usage decoded earlier in that response chunk"
    );
    assert!(
        restored
            .maintenance_usage
            .iter()
            .all(|receipt| receipt.usage.is_some_and(|usage| {
                usage.cost_units == Some(20)
                    && usage.cost_currency == Some(runtime::UsageCostCurrency::SudoPoint)
            })),
        "dropping older stream usage must not lower the newer cost receipt"
    );
    assert_eq!(
        restored
            .last_compaction_report
            .as_ref()
            .unwrap()
            .completed_responses,
        0
    );
    assert!(provider
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(is_compaction_request));
}

#[test]
fn an_unreachable_protected_tail_or_todo_budget_does_not_call_the_provider() {
    for oversized_todo in [false, true] {
        let provider = Provider::new("budget-weak-then-target");
        let env = budget_env("protected-context-cannot-fit", &provider);
        let path = budget_fixture(&env);
        if oversized_todo {
            std::fs::write(
                env.workspace_root().join(".sudocode-todos.json"),
                json!([{"content":format!("TODO_PROTECTED_SENTINEL {}", "x".repeat(30_000)),"status":"pending","activeForm":"Keeping protected context"}]).to_string(),
            ).unwrap();
        } else {
            let mut session = Session::load_from_path(&path).unwrap();
            for (index, message) in session.messages.iter_mut().enumerate() {
                let tokens = if index < 6 { 100 } else { 3_600 };
                message.blocks = vec![ContentBlock::Text {
                    text: "x".repeat((tokens - 1) * 4 + 1),
                }];
            }
            assert_eq!(estimate_session_tokens(&session), 15_000);
            session.save_to_path(&path).unwrap();
        }
        let original = Session::load_from_path(&path).unwrap();
        let (code, screen) = compact_env(&env, &path);
        assert_ne!(
            code, 0,
            "protected context exceeds the 7.5K target: {screen}"
        );
        let restored = Session::load_from_path(&path).unwrap();
        assert_eq!(restored.messages, original.messages);
        assert_eq!(restored.compaction, original.compaction);
        assert_eq!(compaction_request_count(&provider), 0);
    }
}

#[test]
fn empty_history_cannot_skip_when_fixed_request_overhead_exceeds_the_window() {
    let provider = Provider::new("budget-small-output");
    let env = budget_env("empty-history-negative-budget", &provider);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(1024);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(1024);
    std::fs::write(config_path, config.to_string()).unwrap();
    let source = Session::new().with_workspace_root(env.workspace_root());
    let path = env.workspace_root().join("history.jsonl");
    source.save_to_path(&path).unwrap();
    let (code, screen) = compact_env(&env, &path);
    assert_ne!(
        code, 0,
        "fixed overhead cannot fit an empty history: {screen}"
    );
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(restored.messages, source.messages);
    assert_eq!(restored.compaction, source.compaction);
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::Failed);
    assert_eq!(report.before_history, 0);
    assert_eq!(report.safe_history_budget, 0);
    assert!(report.actual_fixed_overhead > 0);
    assert_eq!(report.attempts, 0);
    assert!(restored.maintenance_usage.is_empty());
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn automatic_compaction_keeps_the_pending_prompt_once_in_the_task_request() {
    let provider = Provider::new("budget-small-output");
    let env = budget_env("pending-prompt-once", &provider);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(31_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(1024);
    std::fs::write(config_path, config.to_string()).unwrap();
    let path = budget_fixture(&env);
    let mut cli = env.spawn(&["--resume", path.to_str().unwrap()]);
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA. PROJECT_PENDING_SENTINEL")
        .unwrap();
    common::expect_input_line(
        &cli,
        "PROJECT_PENDING_SENTINEL",
        Duration::from_secs(30),
        "pending prompt entered",
    );
    let marker = common::turn_status_marker(&cli);
    cli.send("\r").unwrap();
    cli.expect("ALPHA_CONTEXT_OK").unwrap();
    common::expect_turn_complete_after(
        &cli,
        &marker,
        Duration::from_secs(30),
        "automatic compaction task completed",
    );
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let requests = provider.requests.lock().unwrap();
    let tasks = requests
        .iter()
        .filter(|request| !is_compaction_request(request))
        .collect::<Vec<_>>();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0]["messages"]
            .to_string()
            .matches("PROJECT_PENDING_SENTINEL")
            .count(),
        1,
        "the resumed task must include the pending prompt exactly once"
    );
    assert!(requests.iter().any(is_compaction_request));
    assert!(!tasks[0]["tools"].as_array().unwrap().is_empty());
}

#[test]
fn pruning_below_the_request_budget_still_summarizes_until_the_ratio_is_met() {
    let provider = Provider::new("budget-prune-then-summary");
    let env = budget_env("pruning-is-not-ratio-success", &provider);
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(45_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(1024);
    std::fs::write(config_path, config.to_string()).unwrap();
    let path = budget_fixture(&env);
    let mut session = Session::load_from_path(&path).unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::ToolUse {
                id: "ratio-output".into(),
                name: "bash".into(),
                input: "{}".into(),
                thought_signature: None,
            },
        ]))
        .unwrap();
    session
        .push_message(ConversationMessage::tool_result(
            "ratio-output",
            "bash",
            format!("HEAD_DIAGNOSTIC {} TAIL_DIAGNOSTIC", "x".repeat(48_000)),
            false,
        ))
        .unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "Review PROJECT_ALPHA output.".into(),
        }]))
        .unwrap();
    session.save_to_path(&path).unwrap();
    let before = estimate_session_tokens(&session);
    let mut cli = env.spawn(&["--resume", path.to_str().unwrap()]);
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "resume ready");
    cli.send("Continue PROJECT_ALPHA.").unwrap();
    common::expect_input_line(
        &cli,
        "Continue PROJECT_ALPHA",
        Duration::from_secs(30),
        "turn entered",
    );
    let marker = common::turn_status_marker(&cli);
    cli.send("\r").unwrap();
    cli.expect("ALPHA_CONTEXT_OK").unwrap();
    common::expect_turn_complete_after(
        &cli,
        &marker,
        Duration::from_secs(30),
        "pruned compaction task completed",
    );
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| is_compaction_request(request))
            .count(),
        1,
        "pruning alone leaves more than half the original history"
    );
    let task = requests
        .iter()
        .find(|request| !is_compaction_request(request))
        .unwrap();
    let input = task["messages"].to_string();
    for marker in ["HEAD_DIAGNOSTIC", "TAIL_DIAGNOSTIC", "middle pruned"] {
        assert!(
            input.contains(marker),
            "missing protected tool output: {marker}"
        );
    }
    drop(requests);
    let restored = Session::load_from_path(&path).unwrap();
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::TargetMet);
    assert!(report.after_history <= before / 2);
    assert!(
        report.completed_responses > 0,
        "target met requires the summary call in this fixture"
    );
}

#[cfg(unix)]
#[test]
fn cancelling_a_delayed_checkpoint_preserves_the_resumable_source() {
    let provider = Provider::new("budget-delayed");
    let env = budget_env("cancel-before-checkpoint-commit", &provider);
    let path = budget_fixture(&env);
    let original = Session::load_from_path(&path).unwrap();
    let mut cli = env.spawn(&["--resume", path.to_str().unwrap(), "/compact"]);
    common::expect_screen(
        &cli,
        |_| compaction_request_count(&provider) == 1,
        Duration::from_secs(30),
        "provider must start before cancellation",
    );
    cli.send("\x03").unwrap();
    cli.set_default_timeout(Duration::from_secs(15));
    let _ = cli.expect_eof().unwrap();
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(restored.messages, original.messages);
    assert_eq!(restored.compaction, original.compaction);
    assert_eq!(compaction_request_count(&provider), 1);
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::Cancelled);
    assert_eq!(report.attempts, 1);
    assert_eq!(report.completed_responses, 0);
    assert_eq!(restored.maintenance_usage.len(), 1);
    assert!(restored.maintenance_usage[0].usage.is_none());
}

#[test]
fn a_concurrent_durable_history_change_prevents_checkpoint_installation() {
    for before_run in [false, true] {
        let provider = Provider::new("budget-revision-conflict");
        let env = budget_env("durable-source-revision-conflict", &provider);
        let path = budget_fixture(&env);
        let args = if before_run {
            vec!["--resume", path.to_str().unwrap()]
        } else {
            vec!["--resume", path.to_str().unwrap(), "/compact"]
        };
        let mut cli = env.spawn(&args);
        if before_run {
            common::expect_input_line_cleared(&cli, Duration::from_secs(30), "old source loaded");
        } else {
            common::expect_screen(
                &cli,
                |_| compaction_request_count(&provider) > 0,
                Duration::from_secs(30),
                "summary request must start before the external append",
            );
        }
        let mut external = Session::load_from_path(&path).unwrap();
        external
            .push_user_text("EXTERNAL_WRITER_SENTINEL preserve this concurrent decision.")
            .unwrap();
        external.save_to_path(&path).unwrap();
        let screen = if before_run {
            cli.send("/compact\r").unwrap();
            let screen = common::expect_screen(
                &cli,
                |screen| screen.contains("source_changed") || screen.contains("source changed"),
                Duration::from_secs(30),
                "stale loaded source must fail before a model request",
            );
            common::expect_input_line_cleared(
                &cli,
                Duration::from_secs(30),
                "source conflict finished",
            );
            cli.send("/exit\r").unwrap();
            assert_eq!(cli.expect_eof().unwrap(), 0);
            screen
        } else {
            provider.response_released.store(true, Ordering::Relaxed);
            cli.set_default_timeout(Duration::from_secs(30));
            let code = cli.expect_eof().unwrap();
            let screen = cli.render(rendered_contents);
            assert_ne!(
                code, 0,
                "changed durable source must reject the checkpoint: {screen}"
            );
            screen
        };
        let restored = Session::load_from_path(&path).unwrap();
        assert_eq!(restored.messages, external.messages);
        assert_eq!(restored.compaction, external.compaction);
        assert_eq!(
            compaction_request_count(&provider),
            usize::from(!before_run)
        );
        assert!(
            screen.contains("source_changed") || screen.contains("source changed"),
            "{screen}"
        );
        assert!(!std::fs::read_dir(env.workspace_root())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("before-compact")));
    }
}

#[test]
fn compaction_preserves_external_billing_for_unchanged_history() {
    let provider = Provider::new("budget-weak-then-target");
    let env = budget_env("external-maintenance-ledger", &provider);
    let path = budget_fixture(&env);
    let mut cli = env.spawn(&["--resume", path.to_str().unwrap()]);
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "source loaded");
    let receipt = runtime::MaintenanceUsageReceipt {
        run_id: "external-maintenance-run".into(),
        attempt_id: 1,
        usage: Some(runtime::TokenUsage {
            input_tokens: 777,
            output_tokens: 33,
            cost_units: Some(73),
            cost_currency: Some(runtime::UsageCostCurrency::SudoPoint),
            ..runtime::TokenUsage::default()
        }),
    };
    let mut external = Session::load_from_path(&path).unwrap();
    external.merge_maintenance_usage(std::slice::from_ref(&receipt));
    external.persist_maintenance_metadata().unwrap();
    cli.send("/compact\r").unwrap();
    common::expect_screen(
        &cli,
        |_| {
            Session::load_from_path(&path).is_ok_and(|session| {
                session
                    .last_compaction_report
                    .as_ref()
                    .is_some_and(|report| report.outcome == runtime::CompactionOutcome::TargetMet)
            })
        },
        Duration::from_secs(30),
        "unchanged history compacted with external receipts preserved",
    );
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "compaction finished");
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let restored = Session::load_from_path(&path).unwrap();
    assert_eq!(restored.maintenance_usage.len(), 3);
    assert!(restored.maintenance_usage.contains(&receipt));
    assert_eq!(compaction_request_count(&provider), 2);
}

#[test]
fn an_unreadable_external_source_is_preserved_when_the_cli_exits() {
    const EXTERNAL_BYTES: &[u8] = b"EXTERNAL_WRITER_INCOMPLETE_RECORD do not replace\n";
    for before_run in [false, true] {
        let provider = Provider::new("budget-revision-conflict");
        let env = budget_env("unreadable-external-source", &provider);
        let path = budget_fixture(&env);
        let args = if before_run {
            vec!["--resume", path.to_str().unwrap()]
        } else {
            vec!["--resume", path.to_str().unwrap(), "/compact"]
        };
        let mut cli = env.spawn(&args);
        if before_run {
            common::expect_input_line_cleared(&cli, Duration::from_secs(30), "source loaded");
        } else {
            common::expect_screen(
                &cli,
                |_| compaction_request_count(&provider) > 0,
                Duration::from_secs(30),
                "summary request started",
            );
        }
        std::fs::write(&path, EXTERNAL_BYTES).unwrap();
        if before_run {
            cli.send("/compact\r").unwrap();
            common::expect_screen(
                &cli,
                |screen| screen.contains("source_read_failed"),
                Duration::from_secs(30),
                "unreadable source rejected before sending a summary request",
            );
            common::expect_input_line_cleared(
                &cli,
                Duration::from_secs(30),
                "failed compaction finished",
            );
            cli.send("/exit\r").unwrap();
        } else {
            provider.response_released.store(true, Ordering::Relaxed);
        }
        cli.set_default_timeout(Duration::from_secs(30));
        let _ = cli.expect_eof().unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            EXTERNAL_BYTES,
            "persist/close must preserve a source that could not be reloaded"
        );
        assert_eq!(
            compaction_request_count(&provider),
            usize::from(!before_run)
        );
    }
}

/// Live acceptance proves a useful continuation, rather than accepting a
/// checkpoint label or repeating the expected answer in the follow-up prompt.
#[test]
fn live_compaction_resumes_a_file_task_using_constraints_from_removed_history() {
    let env = TestEnv::new("live-compaction-file-continuation");
    if env.is_mock() {
        eprintln!("SKIP live compaction semantics: run this test with SCODE_TEST_BACKEND=live");
        return;
    }
    let mut source = Session::new().with_workspace_root(env.workspace_root());
    let constraints = "Task: prepare release-check.txt later, after reviewing the old implementation logs. Its exact contents must be three lines: release_channel=blue, migration_counter=73, release_gate=checks_passed, in that order, with one key=value pair per line and a final newline. Do not deploy anything or execute deployment commands. The file has not been created yet. ";
    for index in 0..10 {
        let prefix = if index == 0 {
            constraints.to_string()
        } else {
            format!("Historical implementation log {index}. ")
        };
        let text = prefix.clone() + &"x".repeat(11_997 - prefix.len());
        source
            .push_message(if index % 2 == 0 {
                ConversationMessage::user_text(text)
            } else {
                ConversationMessage::assistant(vec![ContentBlock::Text { text }])
            })
            .unwrap();
    }
    assert_eq!(estimate_session_tokens(&source), 30_000);
    let path = env.workspace_root().join("history.jsonl");
    source.save_to_path(&path).unwrap();
    let mut compact = env.spawn(&["--resume", path.to_str().unwrap(), "/compact"]);
    compact.set_default_timeout(Duration::from_secs(90));
    let code = compact.expect_eof().unwrap();
    let compacted = Session::load_from_path(&path).unwrap();
    assert_eq!(
        code,
        0,
        "live compaction report: {:?}; {}",
        compacted.last_compaction_report,
        redacted_live_screen(&env, compact.render(rendered_contents)),
    );
    assert!(estimate_session_tokens(&compacted) <= 15_000);
    assert_eq!(
        compacted.last_compaction_report.as_ref().unwrap().outcome,
        runtime::CompactionOutcome::TargetMet
    );

    let mut cli = env.spawn(&[
        "--resume",
        path.to_str().unwrap(),
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "write_file",
    ]);
    cli.set_default_timeout(Duration::from_secs(90));
    common::expect_input_line_cleared(&cli, Duration::from_secs(30), "live resume ready");
    let marker = common::turn_status_marker(&cli);
    cli.send("Create release-check.txt now, using the exact three release constraints specified before compaction. Write only the required key=value lines, in their agreed order. Do not deploy or run shell commands.\r").unwrap();
    common::expect_turn_complete_after(
        &cli,
        &marker,
        Duration::from_secs(90),
        "live file task after compaction",
    );
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("release-check.txt")).unwrap(),
        "release_channel=blue\nmigration_counter=73\nrelease_gate=checks_passed\n",
        "the real model must recover facts available only in the compacted prefix"
    );
    let resumed = Session::load_from_path(&path).unwrap();
    assert!(resumed.messages.iter().flat_map(|message| &message.blocks).any(|block| {
        matches!(block, ContentBlock::ToolUse { name, .. } if name == "write_file" || name == "Write")
    }));
    let report = compacted.last_compaction_report.as_ref().unwrap();
    eprintln!(
        "LIVE_COMPACTION_FILE_OK history={} -> {} target={} attempts={} completed={} artifact=release-check.txt contents=release_channel:blue,migration_counter:73,release_gate:checks_passed",
        report.before_history,
        report.after_history,
        report.target_history.unwrap(),
        report.attempts,
        report.completed_responses,
    );
}

#[test]
fn codex_and_gemini_require_a_complete_terminal_before_installing_a_checkpoint() {
    for mode in [
        "codex-success",
        "codex-truncated",
        "codex-missing-terminal",
        "gemini-success",
        "gemini-truncated",
        "gemini-missing-terminal",
    ] {
        let provider = Provider::new(mode);
        let env = budget_env(mode, &provider);
        let config_path = env.config_home().join("sudocode.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let codex = mode.starts_with("codex-");
        let name = if codex { "codex" } else { "gemini" };
        let auth = if codex { "subscription" } else { "api-key" };
        config["auth_modes"][auth][name] = if codex {
            json!({"baseUrl":provider.url,"token":"test-codex-checkpoint-token"})
        } else {
            json!({"baseUrl":provider.url,"apiKey":"test-gemini-checkpoint-key"})
        };
        let model = &mut config["models"]["claude-sonnet"];
        model["maxOutputTokens"] = json!(1024);
        model["providers"][auth] = json!({
            "provider":name,
            "model":if codex { "codex/fixture-wire" } else { "gemini-fixture-wire" },
        });
        std::fs::write(config_path, config.to_string()).unwrap();
        let path = budget_fixture(&env);
        let original = Session::load_from_path(&path).unwrap();
        let succeeds = mode.ends_with("success");
        let (code, screen) = compact_env_with_auth(&env, &path, auth);
        assert_eq!(code == 0, succeeds, "{mode}: {screen}");
        let restored = Session::load_from_path(&path).unwrap();
        if succeeds {
            assert!(estimate_session_tokens(&restored) <= 7_500);
            assert_eq!(compaction_request_count(&provider), 1);
        } else {
            assert_eq!(restored.messages, original.messages, "{mode}");
            assert_eq!(restored.compaction, original.compaction, "{mode}");
            let report = restored.last_compaction_report.as_ref().unwrap();
            assert_eq!(report.outcome, runtime::CompactionOutcome::Failed);
            if mode.ends_with("truncated") {
                assert_eq!(report.completed_responses, 2);
                assert_eq!(compaction_request_count(&provider), 2);
                assert!(restored
                    .maintenance_usage
                    .iter()
                    .all(|receipt| receipt.usage.is_some()));
            } else {
                assert_eq!(report.completed_responses, 0);
                assert!(compaction_request_count(&provider) > 0);
                assert!(
                    compaction_request_count(&provider) <= 3,
                    "missing terminals have a global transport retry limit"
                );
            }
        }
        assert!(
            provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(is_compaction_request),
            "invalid terminal must never run the task"
        );
    }
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
    let expected = if expected == "Messages removed" {
        "target_met"
    } else {
        expected
    };
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
    let budget = Duration::from_secs(30);
    let deadline = std::time::Instant::now() + budget;
    loop {
        let screen = cli.render(|screen| screen.contents());
        if common::screen_contains(&screen, expected) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // A child that has already exited cannot put anything more on
            // screen, so report THAT instead of the text we were waiting for.
            // Polling `render` alone cannot separate "still working" from
            // "exited without printing it" — both look like a screen that
            // stopped changing, and the wait spends its whole budget before
            // blaming the transcript. Measured under a CPU-starved container:
            // the process was already a zombie 5s into a 30s wait, and the
            // panic still read `expected history preserved`, discarding the
            // exit code that would have explained it.
            cli.set_default_timeout(Duration::from_millis(50));
            match cli.expect_eof() {
                Ok(code) => panic!(
                    "scode exited with code {code} without ever showing \
                     {expected:?}\n{screen}"
                ),
                Err(_) => panic!(
                    "timed out after {budget:?} waiting for {expected:?}; scode \
                     is still running\n{screen}"
                ),
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    cli.expect_eof().unwrap()
}

#[test]
fn failed_empty_truncated_and_growing_summaries_preserve_durable_history() {
    for mode in ["error", "auth-unsupported", "empty", "truncated", "growing"] {
        let provider = Provider::new(mode);
        let workspace = HarnessWorkspace::new(mode);
        workspace.write_mock_config(&provider.url);
        let path = fixture(&workspace);
        let original = Session::load_from_path(&path).unwrap();
        assert_ne!(compact(&workspace, &path, "history preserved"), 0, "{mode}");
        let restored = Session::load_from_path(&path).unwrap();
        assert_eq!(
            restored.messages, original.messages,
            "{mode} must preserve every source message"
        );
        assert_eq!(
            restored.compaction, original.compaction,
            "{mode} must not install a checkpoint"
        );
        assert_eq!(
            restored.last_compaction_report.as_ref().unwrap().outcome,
            runtime::CompactionOutcome::Failed,
            "{mode}"
        );
        let requests = provider.requests.lock().unwrap();
        assert!(!requests.is_empty(), "must exercise the real model path");
        if matches!(mode, "error" | "auth-unsupported") {
            assert_eq!(
                requests.len(),
                1,
                "permanent provider errors do not consume quality retries"
            );
        }
        if mode == "auth-unsupported" {
            let report = restored.last_compaction_report.as_ref().unwrap();
            assert_eq!(report.attempts, 1);
            assert_eq!(report.completed_responses, 0);
            assert_eq!(restored.maintenance_usage.len(), 1);
            assert!(restored.maintenance_usage[0].usage.is_none());
        }
        assert!(
            requests.iter().all(is_compaction_request),
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
    // `expected_limit` is the summary's own allowance (`COMPACT_MAX_OUTPUT_TOKENS`
    // capped by the model) plus the thinking budget the request has to declare to
    // match the turn whose cached prefix it replays. Anthropic requires
    // `budget_tokens < max_tokens`, so a request that must declare the turn's
    // budget cannot also cap its output at 12K: with a 12K cap the provider
    // clamped the budget to 6000 against the turn's 32000, and the one request
    // built to reuse the cache read none of it — at HTTP 200, so the only
    // symptom was the bill. What stays fixed is the *summary* allowance: it
    // still does not scale with the model's output ceiling, and the prompt still
    // carries the 8,000-token guidance that actually governs summary length.
    for (mode, configured_limit, expected_limit) in [
        ("long-summary", None, 12_000 + 32_000),
        ("long-summary", Some(32_000), 12_000 + 16_000),
        ("long-summary", Some(24_000), 24_000),
        ("long-summary-fallback", Some(32_000), 12_000 + 16_000),
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
        // A 9K visible checkpoint plus the preserved tail must fit the 50%
        // target. The smaller fixture belongs to the dynamic-cap regression;
        // it can no longer count a 10K checkpoint as ordinary success.
        let mut source = Session::load_from_path(&path).unwrap();
        for message in &mut source.messages {
            for block in &mut message.blocks {
                if let ContentBlock::Text { text } = block {
                    *text = text.repeat(4);
                }
            }
        }
        source.save_to_path(&path).unwrap();
        assert_eq!(compact(&workspace, &path, "Messages removed"), 0);
        let restored = Session::load_from_path(&path).unwrap();
        assert!(estimate_session_tokens(&restored) <= estimate_session_tokens(&source) / 2);
        let summary = &restored.compaction.as_ref().unwrap().summary;
        assert!(summary.len() > 8_192 * 4);
        assert!(summary.ends_with("CHECKPOINT_COMPLETE</summary>"));
        let requests = provider.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            if mode.ends_with("fallback") { 2 } else { 1 }
        );
        assert!(requests.iter().all(|r| {
            r["messages"].as_array().unwrap().last().unwrap()["content"]
                .to_string()
                .contains("Aim to keep the entire summary within 8,000 tokens")
        }));
        assert_eq!(requests[0]["max_tokens"], json!(expected_limit));
        assert!(requests[0]["tools"].is_array());
        if mode.ends_with("fallback") {
            // The standard-compaction fallback builds a prefix of its own — its
            // own system prompt, no tools, thinking off — so it has nothing to
            // match and keeps the summary's own ceiling.
            assert!(requests[1]["tools"].is_null());
            assert_eq!(requests[1]["max_tokens"], json!(12_000));
            assert!(requests[1]["thinking"].is_null());
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
                is_compaction_request(r),
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
    // Leave enough room for the summary AND the unchanged thinking budget;
    // still exceed the proactive threshold on the enlarged original history.
    config["models"]["claude-sonnet"]["contextWindow"] = json!(105_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(32_000);
    std::fs::write(config_path, config.to_string()).unwrap();
    let path = fixture(&workspace);
    let mut session = Session::load_from_path(&path).unwrap();
    for message in &mut session.messages {
        for block in &mut message.blocks {
            if let ContentBlock::Text { text } = block {
                *text = text.repeat(4);
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
    assert_eq!(
        requests.iter().filter(|r| is_compaction_request(r)).count(),
        1
    );
    // The cache-safe compaction request borrows the turn's cached prefix, and
    // the value of `thinking` is part of Anthropic's cache key — so it has to
    // declare the turn's budget, not one derived from its own smaller output
    // cap. That derivation made this request declare 8192 against the turn's
    // 8192 only by accident of arithmetic on other models; on a 64K model it
    // declared 6000 against 32000 and read none of the history it had just
    // sent byte-for-byte, at HTTP 200. Pin the budget rather than the cap: the
    // budget is what the cache keys on, and the API only requires the cap to
    // be large enough to hold it (`budget_tokens < max_tokens`).
    let budget = |r: &Value| r["thinking"]["budget_tokens"].clone();
    let turn = requests
        .iter()
        .find(|r| !is_compaction_request(r))
        .expect("a turn request");
    let compaction = requests
        .iter()
        .find(|r| is_compaction_request(r))
        .expect("a compaction request");
    assert_eq!(budget(turn), json!(16_000));
    assert_eq!(
        budget(compaction),
        budget(turn),
        "compaction must declare the turn's thinking budget or it cannot read \
         the turn's prefix"
    );
    assert!(
        compaction["max_tokens"].as_u64().unwrap() > 16_000,
        "the cap has to be able to hold the budget: {}",
        compaction["max_tokens"]
    );
    assert!(
        requests
            .iter()
            .all(|r| r["max_tokens"].as_u64().unwrap() <= 32_000),
        "every request stays within the configured maxOutputTokens"
    );
    assert!(requests
        .iter()
        .any(|r| !is_compaction_request(r)
            && r["messages"].to_string().contains("CHECKPOINT_COMPLETE")));
}

#[test]
fn automatic_compaction_failure_never_sends_a_historyless_task_request() {
    let provider = Provider::new("error");
    let workspace = HarnessWorkspace::new("automatic-compact-failure");
    workspace.write_mock_config(&provider.url);
    set_small_window(&workspace);
    let config_path = workspace.config_home.join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(75_000);
    std::fs::write(config_path, config.to_string()).unwrap();
    let path = fixture(&workspace);
    let mut original = Session::load_from_path(&path).unwrap();
    for message in &mut original.messages {
        for block in &mut message.blocks {
            if let ContentBlock::Text { text } = block {
                *text = text.repeat(3);
            }
        }
    }
    // Pruning happens on a staged clone before preflight or a model failure.
    // A failure must restore this output as well as the textual history.
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
    assert_eq!(
        restored.last_compaction_report.as_ref().unwrap().outcome,
        runtime::CompactionOutcome::Failed
    );
    let requests = provider.requests.lock().unwrap();
    assert!(
        requests.iter().all(is_compaction_request),
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
    assert!(
        !is_compaction_request(&requests[0]),
        "the one request must be the task, not a checkpoint call"
    );
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
        .filter(|r| is_compaction_request(r))
        .collect::<Vec<_>>();
    assert_eq!(
        checkpoints.len(),
        1,
        "child should complete cache-preserving compaction in one request; requests: {:?}",
        requests
            .iter()
            .map(|r| (
                is_compaction_request(r),
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
    let child_turns = requests.iter().filter(|request| {
        !is_compaction_request(request) && request["system"] == checkpoint["system"]
    });
    let mut child_count = 0;
    for request in child_turns {
        child_count += 1;
        for key in ["tools", "thinking", "metadata", "output_config"] {
            assert_eq!(request[key], checkpoint[key], "child prefix changed: {key}");
        }
    }
    assert!(
        child_count >= 2,
        "must compare child turns around compaction"
    );
    assert!(checkpoint["messages"]
        .to_string()
        .contains("CHILD_COMPACTION"));
    assert_eq!(checkpoint["model"], "intranet/child-v1");
    let cap = checkpoint["max_tokens"].as_u64().unwrap();
    assert!(cap > 0 && cap <= 1024, "dynamic summary cap: {cap}");
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
            .filter(|r| !is_compaction_request(r))
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
        model["contextWindow"] = json!(80_000);
        model["maxOutputTokens"] = json!(1024);
        model["providers"]["api-key"]["model"] = json!("intranet/apeiron-openai");
        model["providers"]["api-key"]["api"] = json!("openai-completions");
        std::fs::write(config_path, config.to_string()).unwrap();
        let path = fixture(&workspace);
        let original = Session::load_from_path(&path).unwrap();
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
            let restored = Session::load_from_path(&path).unwrap();
            assert_eq!(restored.messages, original.messages, "{mode}");
            assert_eq!(restored.compaction, original.compaction, "{mode}");
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
            let cap = request["max_tokens"].as_u64().unwrap();
            assert!(cap > 0 && cap <= 1024, "{mode}: dynamic summary cap {cap}");
            assert!(
                is_compaction_request(request),
                "a failed checkpoint must not dispatch a task request"
            );
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
                    .map(|request| is_compaction_request(request))
                    .collect::<Vec<_>>()
            );
        });
    cli.send("/exit\r").unwrap();
    cli.expect_eof().unwrap();
    let after = Session::load_from_path(&path).unwrap();
    assert_eq!(&after.messages[..before.messages.len()], &before.messages);
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| !is_compaction_request(r))
            .count(),
        1,
        "no further task request after compaction fails"
    );
    assert!(requests.iter().any(is_compaction_request));
}
