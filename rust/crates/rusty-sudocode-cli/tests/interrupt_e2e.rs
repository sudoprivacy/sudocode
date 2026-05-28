#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn sigint_during_bash_tool_returns_interrupted_result_without_continuing_turn() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let workspace = unique_temp_dir("interrupt-e2e");
    let config_home = workspace.join("config-home");
    let home = workspace.join("home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&home).expect("home should exist");
    write_config(&config_home, &base_url);

    let prompt = format!("{SCENARIO_PREFIX}bash_interrupt_long_running");
    let mut child = Command::new(env!("CARGO_BIN_EXE_scode"))
        .current_dir(&workspace)
        .env_clear()
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("NO_COLOR", "1")
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
        )
        .args([
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
            "--output-format",
            "json",
            &prompt,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("scode should launch");

    wait_for_message_request(&runtime, &server, 1, Duration::from_secs(10));
    thread::sleep(Duration::from_millis(1500));
    send_sigint(child.id());

    let status =
        wait_for_exit(&mut child, Duration::from_secs(10)).expect("scode should exit after SIGINT");
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout should be piped")
        .read_to_string(&mut stdout)
        .expect("stdout should read");
    child
        .stderr
        .take()
        .expect("stderr should be piped")
        .read_to_string(&mut stderr)
        .expect("stderr should read");

    assert!(
        status.success(),
        "scode should exit cleanly after interrupt\nstdout:\n{stdout}\n\nstderr:\n{stderr}"
    );

    let captured = runtime.block_on(server.captured_requests());
    let message_request_count = captured
        .iter()
        .filter(|request| request.path == "/v1/messages")
        .count();
    assert_eq!(
        message_request_count, 1,
        "interrupt should cancel the turn without a follow-up model call"
    );

    let parsed: Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    let tool_results = parsed["tool_results"]
        .as_array()
        .expect("tool_results should be an array");
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0]["tool_name"], "bash");
    assert_eq!(tool_results[0]["is_error"], true);

    let tool_output: Value = serde_json::from_str(
        tool_results[0]["output"]
            .as_str()
            .expect("tool output should be a JSON string"),
    )
    .expect("bash output should parse as JSON");
    assert_eq!(tool_output["interrupted"], true);
    assert_eq!(tool_output["returnCodeInterpretation"], "interrupted");
    assert_eq!(tool_output["stderr"], "Command interrupted by user");

    fs::remove_dir_all(&workspace).expect("workspace cleanup should succeed");
}

fn wait_for_message_request(
    runtime: &tokio::runtime::Runtime,
    server: &MockAnthropicService,
    expected: usize,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let captured = runtime.block_on(server.captured_requests());
        let count = captured
            .iter()
            .filter(|request| request.path == "/v1/messages")
            .count();
        if count >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} /v1/messages request(s); saw {count}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(error) => panic!("failed to wait for scode: {error}"),
        }
    }
}

fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .expect("kill should launch");
    assert!(status.success(), "kill -INT should succeed");
}

fn write_config(config_home: &std::path::Path, base_url: &str) {
    let sample = runtime::SAMPLE_SUDOCODE_JSON
        .replace("https://api.anthropic.com", base_url)
        .replace("<YOUR_ANTHROPIC_API_KEY>", "test-interrupt-key");
    fs::write(config_home.join("sudocode.json"), sample).expect("sudocode.json should be written");
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
