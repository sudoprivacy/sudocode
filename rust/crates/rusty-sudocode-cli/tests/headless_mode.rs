#[path = "common/isolated_env.rs"]
mod isolated_env;

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

// macOS pipe creation + CLOEXEC is not atomic. Serialize fixture processes so
// a concurrent spawn cannot inherit another test's stdin writer and hide EOF.
static PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Fixture {
    _process_guard: std::sync::MutexGuard<'static, ()>,
    runtime: tokio::runtime::Runtime,
    server: MockAnthropicService,
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let guard = PROCESS_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(MockAnthropicService::spawn()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("config")).unwrap();
        fs::create_dir_all(dir.path().join("home")).unwrap();
        fs::write(dir.path().join("fixture.txt"), "alpha parity line\n").unwrap();
        fs::write(
            dir.path().join("config/sudocode.json"),
            runtime::SAMPLE_SUDOCODE_JSON
                .replace("https://api.anthropic.com", &server.base_url())
                .replace("<YOUR_ANTHROPIC_API_KEY>", "test-headless-key"),
        )
        .unwrap();
        Self {
            _process_guard: guard,
            runtime: rt,
            server,
            dir,
        }
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
        cmd.current_dir(self.dir.path())
            .env_clear()
            .env("HOME", self.dir.path().join("home"))
            .env("SUDO_CODE_CONFIG_HOME", self.dir.path().join("config"))
            .env("NO_COLOR", "1")
            .args(["--auth", "api-key"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in isolated_env::inherited_env() {
            cmd.env(key, value);
        }
        cmd
    }
    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Output {
        let mut cmd = self.command();
        cmd.args(args);
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.spawn().unwrap();
        if let Some(bytes) = input {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let out = child.wait_with_output().unwrap();
                panic!("headless hung: {}", String::from_utf8_lossy(&out.stderr));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
fn json_output(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}
#[test]
fn text_tools_and_flags_after_prompt() {
    let f = Fixture::new();
    for flag in ["-p", "--print"] {
        let out = f.run(
            &[
                flag,
                &format!("{SCENARIO_PREFIX}read_file_roundtrip"),
                "--model",
                "sonnet",
                "--permission-mode",
                "read-only",
            ],
            None,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            "read_file roundtrip complete: alpha parity line"
        );
    }
}
#[test]
fn stdin_json_and_resume() {
    let f = Fixture::new();
    let task = format!("{SCENARIO_PREFIX}read_file_roundtrip");
    let out = f.run(
        &["-p", "--output-format", "json", "--model", "sonnet"],
        Some(task.as_bytes()),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = json_output(&out);
    assert_eq!(value["type"], "result");
    assert_eq!(value["num_turns"], 1);
    assert_eq!(value["model_round_trips"], 2);
    let out = f.run(
        &[
            "-p",
            "继续",
            "--resume",
            value["session_id"].as_str().unwrap(),
            "--model",
            "sonnet",
            "--output-format=json",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json_output(&out)["session_id"], value["session_id"]);
}
#[test]
fn stream_has_message_boundaries_and_object_tool_inputs() {
    let f = Fixture::new();
    let out = f.run(
        &[
            "-p",
            &format!("{SCENARIO_PREFIX}multi_tool_turn_roundtrip"),
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--output-format",
            "stream-json",
            "--verbose",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.first().unwrap()["type"], "system");
    assert_eq!(events.last().unwrap()["type"], "result");
    assert_eq!(events.iter().filter(|e| e["type"] == "result").count(), 1);
    let assistants: Vec<_> = events.iter().filter(|e| e["type"] == "assistant").collect();
    assert!(assistants.len() >= 2);
    let mut calls = Vec::new();
    for assistant in &assistants {
        for block in assistant["message"]["content"].as_array().unwrap() {
            if block["type"] == "tool_use" {
                assert!(block["input"].is_object());
                calls.push(block["id"].clone());
            }
        }
    }
    assert!(calls.len() >= 2);
    for event in events.iter().filter(|e| e["type"] == "user") {
        assert!(calls.contains(&event["message"]["content"][0]["tool_use_id"]));
    }
    assert_ne!(
        assistants[0]["message"]["id"],
        assistants[1]["message"]["id"]
    );
}
#[test]
fn invalid_inputs_and_unsupported_options_fail_once() {
    let f = Fixture::new();
    for (args, input) in [
        (vec!["-p", "--output-format=json"], Some(&b""[..])),
        (vec!["-p", "--output-format=json"], Some(&b"\xff"[..])),
        (
            vec![
                "-p",
                "task",
                "--output-format=json",
                "--include-partial-messages",
            ],
            None,
        ),
        (vec!["-p", "status", "--output-format=json"], None),
    ] {
        let out = f.run(&args, input);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(json_output(&out)["is_error"], true);
    }
}
#[test]
fn permissions_never_prompt_and_questions_require_input() {
    let f = Fixture::new();
    for mode in ["read-only", "workspace-write"] {
        let out = f.run(
            &[
                "-p",
                &format!("{SCENARIO_PREFIX}bash_permission_prompt_denied"),
                "--model",
                "sonnet",
                "--permission-mode",
                mode,
                "--output-format=json",
            ],
            None,
        );
        let value = json_output(&out);
        assert!(!value["permission_denials"].as_array().unwrap().is_empty());
        assert!(!String::from_utf8_lossy(&out.stdout).contains("Approve this tool call?"));
    }
    let out = f.run(
        &[
            "-p",
            &format!("{SCENARIO_PREFIX}ask_user_question_roundtrip"),
            "--model",
            "sonnet",
            "--output-format=json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_output(&out)["subtype"], "needs_input");
}

#[test]
fn input_pipe_without_task_has_a_deadline() {
    let f = Fixture::new();
    let mut child = f
        .command()
        .args(["-p", "--output-format=json"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let held = child.stdin.take().unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            panic!("stdin deadline was not enforced");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    drop(held);
    assert_eq!(out.status.code(), Some(2));
    assert!(json_output(&out)["error"]
        .as_str()
        .unwrap()
        .contains("no stdin data received"));
    assert!(f.runtime.block_on(f.server.captured_requests()).is_empty());
}

#[test]
fn prompt_plus_utf8_stdin_reaches_provider_with_separator() {
    let f = Fixture::new();
    let task = format!("{SCENARIO_PREFIX}single_turn_text");
    let out = f.run(
        &["-p", &task, "--model", "sonnet", "--output-format=json"],
        Some("供应商甲,100\n供应商乙,200\n".as_bytes()),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let requests = f.runtime.block_on(f.server.captured_requests());
    assert!(requests[0].raw_body.contains("<stdin>"));
    assert!(requests[0].raw_body.contains("供应商甲"));
}

#[cfg(unix)]
#[test]
fn signals_cancel_tools_and_return_matching_exit_codes() {
    use nix::{
        sys::signal::{kill, Signal},
        unistd::Pid,
    };
    let f = Fixture::new();
    for (signal, code) in [(Signal::SIGINT, 130), (Signal::SIGTERM, 143)] {
        let request_count = f.runtime.block_on(f.server.captured_requests()).len();
        let mut child = f
            .command()
            .args([
                "-p",
                &format!("{SCENARIO_PREFIX}bash_interrupt_long_running"),
                "--model",
                "sonnet",
                "--permission-mode",
                "danger-full-access",
                "--output-format=json",
            ])
            .spawn()
            .unwrap();
        let before = Instant::now();
        loop {
            // Wait for the model request, then allow the tool to enter sleep.
            if f.runtime.block_on(f.server.captured_requests()).len() > request_count {
                break;
            }
            if before.elapsed() > Duration::from_secs(15) {
                child.kill().unwrap();
                panic!("provider never called");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_secs(1));
        kill(Pid::from_raw(i32::try_from(child.id()).unwrap()), signal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("signal did not stop task");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(code),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let result = json_output(&out);
        assert_eq!(result["subtype"], "cancelled");
        assert_eq!(result["is_error"], true);
    }
}

#[test]
fn broken_stdout_exits_without_waiting_for_long_tool() {
    let f = Fixture::new();
    let mut child = f
        .command()
        .args([
            "-p",
            &format!("{SCENARIO_PREFIX}bash_interrupt_long_running"),
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--output-format=stream-json",
        ])
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("broken stdout leaked engine");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(child.wait().unwrap().code(), Some(1));
}

#[test]
fn background_agents_are_refused_before_spawning() {
    let f = Fixture::new();
    let out = f.run(
        &[
            "-p",
            &format!("{SCENARIO_PREFIX}subagent_events_background"),
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--output-format=stream-json",
        ],
        None,
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("background jobs are unavailable in print mode"),
        "{stdout}"
    );
    let requests = f.runtime.block_on(f.server.captured_requests());
    assert!(requests.iter().all(|r| r.scenario != "subagent_slow_child"));
}

#[cfg(unix)]
#[test]
fn mcp_tool_results_are_streamed_and_server_is_reaped() {
    let f = Fixture::new();
    let script = f.dir.path().join("mcp.py");
    fs::write(&script, r"import json, sys, os
open('mcp.pid', 'w').write(str(os.getpid()))
for line in sys.stdin:
    req = json.loads(line)
    method = req.get('method')
    if 'id' not in req: continue
    if method == 'initialize':
        result = {'protocolVersion':req['params']['protocolVersion'], 'capabilities':{'tools':{}}, 'serverInfo':{'name':'headless-mock','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'echo', 'description':'Echo', 'inputSchema':{'type':'object', 'properties':{'text':{'type':'string'}}}}]}
    elif method == 'tools/call':
        result = {'content':[{'type':'text', 'text':'echo:'+req['params']['arguments'].get('text','')}], 'isError':False}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0', 'id':req['id'], 'result':result}), flush=True)
").unwrap();
    let settings_dir = f.dir.path().join(".nexus/sudocode");
    fs::create_dir_all(&settings_dir).unwrap();
    fs::write(settings_dir.join("settings.json"), serde_json::json!({"experimental":{"mcpConfigServers":true},"mcpServers":{"parity":{"command":"/usr/bin/python3", "args":[script]}}}).to_string()).unwrap();
    let out = f.run(
        &[
            "-p",
            &format!("{SCENARIO_PREFIX}mcp_tool_roundtrip"),
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--output-format=stream-json",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("echo:"), "{stdout}");
    let pid: i32 = fs::read_to_string(f.dir.path().join("mcp.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
        "MCP process leaked"
    );
}

#[test]
fn oversized_input_is_rejected_before_the_provider_is_called() {
    let f = Fixture::new();
    let input = vec![b'x'; 16 * 1024 * 1024 + 1];
    let out = f.run(&["-p", "--output-format=json"], Some(&input));
    assert_eq!(out.status.code(), Some(2));
    assert!(json_output(&out)["error"]
        .as_str()
        .unwrap()
        .contains("16 MiB"));
    assert!(f.runtime.block_on(f.server.captured_requests()).is_empty());
}

#[test]
fn double_dash_escapes_prompt_and_unknown_flags_fail() {
    let f = Fixture::new();
    let out = f.run(
        &[
            "-p",
            "--model",
            "sonnet",
            "--output-format=json",
            "--",
            &format!("--task {SCENARIO_PREFIX}single_turn_text"),
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = f.run(
        &["-p", "task", "--made-up-option", "--output-format=json"],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json_output(&out)["subtype"], "invalid_input");
}

#[test]
fn project_skill_is_discovered_and_really_read() {
    let f = Fixture::new();
    let skill = f.dir.path().join(".agents/skills/headless-probe/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(skill, "---\nname: headless-probe\ndescription: Headless discovery probe\n---\nHEADLESS_SKILL_LOADED\n").unwrap();
    let out = f.run(
        &[
            "-p",
            &format!("{SCENARIO_PREFIX}skill_read_roundtrip"),
            "--model",
            "sonnet",
            "--output-format=stream-json",
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let events: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let read = events.iter().find(|e| e["type"] == "user").unwrap();
    assert_eq!(read["message"]["content"][0]["is_error"], false);
    assert!(read["message"]["content"][0]["content"]
        .as_str()
        .unwrap()
        .contains("HEADLESS_SKILL_LOADED"));
    let requests = f.runtime.block_on(f.server.captured_requests());
    assert!(requests[0].raw_body.contains("Headless discovery probe"));
}

#[test]
fn provider_errors_return_one_failed_result() {
    let f = Fixture::new();
    let out = f.run(
        &[
            "-p",
            "no mock scenario exists for this task",
            "--model",
            "sonnet",
            "--output-format=json",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let value = json_output(&out);
    assert_eq!(value["type"], "result");
    assert_eq!(value["subtype"], "runtime_error");
    assert_eq!(value["kind"], "api_http_error");
    assert_eq!(value["is_error"], true);
}

#[test]
fn input_that_starts_but_never_closes_is_rejected() {
    let f = Fixture::new();
    let mut child = f
        .command()
        .args(["-p", "--output-format=json"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut held = child.stdin.take().unwrap();
    held.write_all(b"unfinished task").unwrap();
    let deadline = Instant::now() + Duration::from_secs(35);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("stdin EOF deadline was not enforced");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    drop(held);
    assert_eq!(out.status.code(), Some(2));
    assert!(json_output(&out)["error"]
        .as_str()
        .unwrap()
        .contains("EOF within 30s"));
    assert!(f.runtime.block_on(f.server.captured_requests()).is_empty());
}
