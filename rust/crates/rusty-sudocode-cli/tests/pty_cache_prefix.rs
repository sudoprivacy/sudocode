//! Capture the actual HTTP payload after provider conversion while driving the
//! real CLI through a PTY. No assertion treats mock usage as a real cache hit.
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::TestEnv;
use pty_expect::PtySession;
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};
use std::fmt::Write as _;

const COMPACT: &str = "Create a concise checkpoint";
const WAIT: Duration = Duration::from_secs(30);

struct Capture {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    retry_next: Arc<AtomicBool>,
    pressure_next: Arc<AtomicBool>,
    thinking_blocks: Arc<AtomicBool>,
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
                    Ok((socket, _)) => serve(socket, &captured, &retry, &pressure, &thinking),
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
            stop,
            thread: Some(worker),
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn read_request(socket: &TcpStream) -> Option<(String, Vec<u8>)> {
    let mut reader = BufReader::new(socket.try_clone().unwrap());
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return None;
    }
    let mut length = 0;
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
        }
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).unwrap();
    Some((first, bytes))
}

fn serve(
    mut socket: TcpStream,
    captured: &Mutex<Vec<Value>>,
    retry: &AtomicBool,
    pressure: &AtomicBool,
    thinking: &AtomicBool,
) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let Some((first, bytes)) = read_request(&socket) else {
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
    let number = {
        let mut all = captured.lock().unwrap();
        all.push(request.clone());
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
    let content = response_content(&request, number);
    let usage = json!({"input_tokens": if pressure.swap(false, Ordering::Relaxed) { 990_000 } else { 2_000 },"output_tokens":100});
    let tool_use = content["type"] == "tool_use";
    let stop_reason = if tool_use { "tool_use" } else { "end_turn" };
    let start = if tool_use {
        json!({"type":"tool_use","id":content["id"],"name":content["name"],"input":{}})
    } else {
        json!({"type":"text","text":""})
    };
    let delta = if tool_use {
        json!({"type":"input_json_delta","partial_json":content["input"].to_string()})
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
    let mut body = String::new();
    for (event, data) in events {
        write!(body, "event: {event}\ndata: {data}\n\n").unwrap();
    }
    respond(&mut socket, "200 OK", "text/event-stream", &body);
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
}

#[test]
fn adaptive_thinking_and_effort_stay_fixed_across_compaction_and_resume() {
    for model in ["claude-opus-5-5", "vendor/claude-opus-5-5"] {
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
                "--reasoning-effort",
                "high",
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
        assert_eq!(baseline["output_config"]["effort"], "high");
        assert!(baseline.get("reasoning_effort").is_none());
        turn(&mut cli, "Continue the next step.");
        assert_thinking_replayed(&capture.bodies()[1], 1);
        cli.send("/compact\r").unwrap();
        cli.expect("Messages removed").unwrap();
        common::expect_input_line_cleared(&cli, WAIT, "adaptive compaction finished");
        turn(&mut cli, "Continue after compaction.");
        exit(&mut cli);
        let mut cli = start();
        turn(&mut cli, "Continue after restart.");
        exit(&mut cli);
        let requests = capture.bodies();
        assert!(requests.iter().any(|r| r.to_string().contains(COMPACT)));
        for request in &requests[1..] {
            fixed_prefix(&baseline, request);
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
        assert!(request.get("output_config").is_none());
        assert!(request.get("reasoning_effort").is_none());
    }
    assert_thinking_replayed(&requests[1], 1);
}

fn response_content(request: &Value, number: usize) -> Value {
    let compact = request.to_string().contains(COMPACT);
    let last = request["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
    let searching = !compact && !last.contains("tool_result") && last.contains("discover ");
    let saving = !compact && !last.contains("tool_result") && last.contains("SAVE_GLOBAL_MEMORY");
    if saving {
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
            mock_anthropic_service::CANNED_COMPACTION_SUMMARY.to_string()
        } else { format!("PREFIX_REPLY_{number}") }})
    }
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
    let cli = env.spawn(&[
        "--resume",
        path.to_str().unwrap(),
        "--permission-mode",
        "danger-full-access",
    ]);
    common::expect_input_line_cleared(&cli, WAIT, "resume ready");
    cli
}

fn turn(cli: &mut PtySession, text: &str) {
    let marker = common::turn_status_marker(cli);
    cli.send(&format!("{text}\r")).unwrap();
    common::expect_turn_complete_after(cli, &marker, WAIT, text);
    common::expect_input_line_cleared(cli, WAIT, "turn completed");
}

fn exit(cli: &mut PtySession) {
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
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
