//! Live API smoke tests that verify real backends accept our ACP payloads.
//!
//! These tests are gated on the `CLAUDE_CODE_OAUTH_TOKEN` environment variable.
//! When the token is absent or empty the tests silently pass (return early).
//! On CI they run only on main merges where the GitHub secret is available.

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const RECV_TIMEOUT: Duration = Duration::from_secs(120);
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Token guard
// ---------------------------------------------------------------------------

fn oauth_token() -> Option<String> {
    std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
        .ok()
        .filter(|k| !k.is_empty())
}

// ---------------------------------------------------------------------------
// Workspace setup
// ---------------------------------------------------------------------------

struct TestWorkspace {
    root: PathBuf,
    config_home: PathBuf,
    home: PathBuf,
}

impl TestWorkspace {
    fn new(label: &str) -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "scode-live-{label}-{}-{millis}-{counter}",
            std::process::id()
        ));
        Self {
            config_home: root.join("config-home"),
            home: root.join("home"),
            root,
        }
    }

    fn create(&self) {
        fs::create_dir_all(&self.root).expect("workspace root should exist");
        fs::create_dir_all(&self.config_home).expect("config home should exist");
        fs::create_dir_all(&self.home).expect("home should exist");
    }

    /// Write sudocode.json with the real Anthropic base URL (no mock replacement).
    fn write_sudocode_json(&self) {
        fs::write(
            self.config_home.join("sudocode.json"),
            runtime::SAMPLE_SUDOCODE_JSON,
        )
        .expect("test sudocode.json should be written");
    }

    fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Transport abstraction (mirrored from acp_integration.rs)
// ---------------------------------------------------------------------------

enum Transport {
    Stdio {
        child: Child,
        stdin: tokio::process::ChildStdin,
        stdout: BufReader<tokio::process::ChildStdout>,
    },
    WebSocket {
        child: Child,
        ws_stream: Box<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    },
}

struct AcpTestClient {
    transport: Transport,
    next_id: u64,
}

impl AcpTestClient {
    async fn send_request(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        self.send_raw(&request).await;

        let mut notifications = Vec::new();
        loop {
            let msg = self.recv().await;
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                return (notifications, msg);
            }
            notifications.push(msg);
        }
    }

    async fn send_raw(&mut self, value: &Value) {
        match &mut self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut line = serde_json::to_string(value).expect("serialize json");
                line.push('\n');
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .expect("write to stdin");
                stdin.flush().await.expect("flush stdin");
            }
            Transport::WebSocket { ws_stream, .. } => {
                let text = serde_json::to_string(value).expect("serialize json");
                ws_stream
                    .send(Message::Text(text.into()))
                    .await
                    .expect("send ws message");
            }
        }
    }

    async fn recv(&mut self) -> Value {
        timeout(RECV_TIMEOUT, self.recv_inner())
            .await
            .expect("recv timed out after 120s")
    }

    async fn recv_inner(&mut self) -> Value {
        match &mut self.transport {
            Transport::Stdio { stdout, .. } => {
                let mut line = String::new();
                loop {
                    line.clear();
                    let n = stdout.read_line(&mut line).await.expect("read from stdout");
                    assert!(n != 0, "stdio stdout closed unexpectedly");
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
                        return val;
                    }
                }
            }
            Transport::WebSocket { ws_stream, .. } => loop {
                let msg = ws_stream
                    .next()
                    .await
                    .expect("ws stream ended unexpectedly")
                    .expect("ws read error");
                match msg {
                    Message::Text(text) => {
                        return serde_json::from_str(&text).expect("parse ws json");
                    }
                    Message::Close(_) => panic!("ws closed unexpectedly"),
                    _ => {}
                }
            },
        }
    }

    async fn shutdown(self) {
        match self.transport {
            Transport::Stdio { mut child, .. } => {
                let _ = child.kill().await;
            }
            Transport::WebSocket {
                mut child,
                ws_stream,
                ..
            } => {
                drop(ws_stream);
                let _ = child.kill().await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Transport constructors
// ---------------------------------------------------------------------------

fn base_command(workspace: &TestWorkspace, token: &str) -> Command {
    base_command_with_mode(workspace, token, "read-only")
}

fn base_command_with_mode(
    workspace: &TestWorkspace,
    token: &str,
    permission_mode: &str,
) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.current_dir(&workspace.root)
        .env_clear()
        .env("SUDO_CODE_CONFIG_HOME", &workspace.config_home)
        .env("HOME", &workspace.home)
        .env("CLAUDE_CODE_OAUTH_TOKEN", token)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--auth",
            "subscription",
            "--model",
            "claude-sonnet",
            "--permission-mode",
            permission_mode,
        ]);
    cmd
}

fn spawn_stdio_client(workspace: &TestWorkspace, token: &str) -> AcpTestClient {
    spawn_stdio_client_with_mode(workspace, token, "read-only")
}

fn spawn_stdio_client_with_mode(
    workspace: &TestWorkspace,
    token: &str,
    permission_mode: &str,
) -> AcpTestClient {
    let mut cmd = base_command_with_mode(workspace, token, permission_mode);
    cmd.arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn scode acp stdio");
    let stdin = child.stdin.take().expect("stdin should be piped");
    let stdout = child.stdout.take().expect("stdout should be piped");

    AcpTestClient {
        transport: Transport::Stdio {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        },
        next_id: 1,
    }
}

async fn spawn_ws_client(workspace: &TestWorkspace, token: &str) -> AcpTestClient {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind free port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);

    let mut cmd = base_command(workspace, token);
    cmd.args(["acp", "serve", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn scode acp serve");

    let stderr = child.stderr.take().expect("stderr should be piped");
    let mut stderr_reader = BufReader::new(stderr);
    timeout(SERVER_STARTUP_TIMEOUT, async {
        let mut line = String::new();
        loop {
            line.clear();
            let n = stderr_reader
                .read_line(&mut line)
                .await
                .expect("read stderr");
            assert!(n != 0, "stderr closed before server ready");
            if line.contains("[acp-ws] listening on") {
                break;
            }
        }
    })
    .await
    .expect("ws server should be ready within timeout");

    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match stderr_reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                _ => {}
            }
        }
    });

    let url = format!("ws://127.0.0.1:{port}/ws");
    let (ws_stream, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    AcpTestClient {
        transport: Transport::WebSocket {
            child,
            ws_stream: Box::new(ws_stream),
        },
        next_id: 1,
    }
}

// ---------------------------------------------------------------------------
// Scenarios (live API — minimal)
// ---------------------------------------------------------------------------

async fn scenario_initialize(client: &mut AcpTestClient) {
    let (notifs, resp) = client
        .send_request("initialize", json!({ "protocolVersion": 1 }))
        .await;

    assert!(
        notifs.is_empty(),
        "initialize should not produce notifications"
    );
    let result = &resp["result"];
    assert!(
        result.get("protocolVersion").is_some(),
        "response should include protocolVersion"
    );
    assert_eq!(result["agentInfo"]["name"], "scode");
}

async fn scenario_session_new(client: &mut AcpTestClient, cwd: &std::path::Path) -> String {
    let (notifs, resp) = client
        .send_request(
            "session/new",
            json!({
                "cwd": cwd.to_string_lossy().to_string(),
                "mcpServers": []
            }),
        )
        .await;

    assert!(
        notifs.is_empty(),
        "session/new should not produce notifications"
    );
    let session_id = resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId should be a string");
    assert!(!session_id.is_empty(), "sessionId should not be empty");
    session_id.to_string()
}

async fn scenario_session_prompt(client: &mut AcpTestClient, session_id: &str) {
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "Respond with exactly: pong" }]
            }),
        )
        .await;

    // When the API token is invalid or expired the prompt completes
    // instantly with zero notifications — fail loudly so we notice.
    assert!(
        !notifs.is_empty(),
        "prompt produced no notifications — auth/API failure. \
         Check CLAUDE_CODE_OAUTH_TOKEN is valid."
    );

    // Every notification should be a session/update.
    for n in &notifs {
        assert_eq!(
            n["method"], "session/update",
            "unexpected notification method: {}",
            n["method"]
        );
    }

    // Collect text deltas from agent_message_chunk notifications.
    let response_text: String = notifs
        .iter()
        .filter_map(|n| {
            let update = &n["params"]["update"];
            if update["sessionUpdate"] == "agent_message_chunk" {
                update["content"]["text"].as_str().map(String::from)
            } else {
                None
            }
        })
        .collect();
    assert!(
        response_text.to_lowercase().contains("pong"),
        "expected 'pong' in model response but got: {response_text:?}"
    );

    let result = &resp["result"];
    assert_eq!(
        result["stopReason"], "end_turn",
        "prompt response stopReason should be end_turn"
    );
    assert!(
        result.get("usage").is_some(),
        "prompt response should include usage"
    );
}

async fn scenario_subagent_calculations(client: &mut AcpTestClient, session_id: &str) {
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": concat!(
                    "You MUST use the Agent tool to create exactly 3 separate agents, ",
                    "one for each calculation below. Do NOT compute them yourself. ",
                    "Each agent prompt should be: \"What is <expr>? Reply with ONLY the number.\"\n\n",
                    "Calculations:\n",
                    "1. 101 + 102\n",
                    "2. 201 + 202\n",
                    "3. 301 + 302\n\n",
                    "After all 3 agents return, output exactly this JSON and nothing else:\n",
                    "{\"results\": [<agent1_answer>, <agent2_answer>, <agent3_answer>]}"
                )}]
            }),
        )
        .await;

    assert!(
        !notifs.is_empty(),
        "subagent prompt produced no notifications — auth/API failure. \
         Check CLAUDE_CODE_OAUTH_TOKEN is valid."
    );

    // Count Agent tool_call starts (title == "Agent", status == "in_progress").
    let agent_starts: Vec<_> = notifs
        .iter()
        .filter(|n| {
            let update = &n["params"]["update"];
            update["sessionUpdate"] == "tool_call"
                && update["title"] == "Agent"
                && update["status"] == "in_progress"
        })
        .collect();
    assert_eq!(
        agent_starts.len(),
        3,
        "expected exactly 3 Agent tool_call starts, got {}",
        agent_starts.len()
    );

    // Extract completed tool_call_update notifications.
    let completed_updates: Vec<_> = notifs
        .iter()
        .filter(|n| {
            let update = &n["params"]["update"];
            update["sessionUpdate"] == "tool_call_update" && update["status"] == "completed"
        })
        .collect();

    // Extract subagent results from rawOutput.result (TaskOutput results).
    let mut agent_results: Vec<String> = completed_updates
        .iter()
        .filter_map(|n| {
            n["params"]["update"]["rawOutput"]["result"]
                .as_str()
                .map(String::from)
        })
        .collect();
    agent_results.sort();

    // Count failed tool_call_update notifications for diagnostics.
    let failed_updates: Vec<_> = notifs
        .iter()
        .filter(|n| {
            let update = &n["params"]["update"];
            update["sessionUpdate"] == "tool_call_update" && update["status"] == "failed"
        })
        .collect();

    // Log all notifications for diagnostics on CI failures.
    eprintln!(
        "subagent test: {} notifications, {} Agent starts, {} completed updates, {} failed updates, results: {:?}",
        notifs.len(),
        agent_starts.len(),
        completed_updates.len(),
        failed_updates.len(),
        agent_results,
    );
    for (i, n) in notifs.iter().enumerate() {
        let update = &n["params"]["update"];
        let session_update = update["sessionUpdate"].as_str().unwrap_or("unknown");
        eprintln!("  notif[{i}]: sessionUpdate={session_update} status={}", update["status"]);
    }
    for (i, u) in completed_updates.iter().enumerate() {
        eprintln!("  completed[{i}]: {}", serde_json::to_string(u).unwrap());
    }
    for (i, u) in failed_updates.iter().enumerate() {
        eprintln!("  failed[{i}]: rawOutput={}", u["params"]["update"]["rawOutput"]);
    }

    assert!(
        agent_results.contains(&"203".to_string())
            && agent_results.contains(&"403".to_string())
            && agent_results.contains(&"603".to_string()),
        "expected agent results to contain 203, 403, 603 but got: {agent_results:?}"
    );

    let result = &resp["result"];
    assert_eq!(
        result["stopReason"], "end_turn",
        "subagent prompt stopReason should be end_turn"
    );
    assert!(
        result.get("usage").is_some(),
        "subagent prompt response should include usage"
    );
}

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

async fn run_live_scenarios(client: &mut AcpTestClient, workspace: &TestWorkspace) {
    scenario_initialize(client).await;
    let session_id = scenario_session_new(client, &workspace.root).await;
    scenario_session_prompt(client, &session_id).await;
    verify_session_model_in_jsonl(workspace);
}

/// After a prompt completes, find the persisted session JSONL and verify
/// that assistant messages carry a `model` field.
fn verify_session_model_in_jsonl(workspace: &TestWorkspace) {
    let sessions_dir = workspace.root.join(".scode").join("sessions");
    if !sessions_dir.exists() {
        eprintln!("WARN: session directory does not exist, skipping JSONL model check");
        return;
    }

    let Some(jsonl_path) = find_jsonl_file(&sessions_dir) else {
        eprintln!("WARN: no session .jsonl file found, skipping model check");
        return;
    };

    let contents = fs::read_to_string(&jsonl_path).expect("should read session jsonl");

    let mut found_assistant = false;
    for line in contents.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record["type"] != "message" {
            continue;
        }
        let msg = &record["message"];
        if msg["role"] != "assistant" {
            continue;
        }
        found_assistant = true;
        let model = msg.get("model").and_then(Value::as_str);
        assert!(
            model.is_some_and(|m| !m.is_empty()),
            "assistant message in session JSONL should have a non-empty model field, \
             but got: {:?}",
            model
        );
        eprintln!(
            "session JSONL: assistant message model = {:?}",
            model.unwrap()
        );
    }
    assert!(
        found_assistant,
        "session JSONL should contain at least one assistant message"
    );
}

async fn run_subagent_scenarios(client: &mut AcpTestClient, workspace: &TestWorkspace) {
    scenario_initialize(client).await;
    let session_id = scenario_session_new(client, &workspace.root).await;
    scenario_subagent_calculations(client, &session_id).await;
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_anthropic_smoke_stdio() {
    let Some(token) = oauth_token() else {
        return;
    };

    let workspace = TestWorkspace::new("live-stdio");
    workspace.create();
    workspace.write_sudocode_json();

    let mut client = spawn_stdio_client(&workspace, &token);
    run_live_scenarios(&mut client, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn live_subagent_smoke_stdio() {
    let Some(token) = oauth_token() else {
        return;
    };

    let workspace = TestWorkspace::new("live-subagent-stdio");
    workspace.create();
    workspace.write_sudocode_json();

    let mut client = spawn_stdio_client_with_mode(&workspace, &token, "danger-full-access");
    run_subagent_scenarios(&mut client, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn live_anthropic_smoke_ws() {
    let Some(token) = oauth_token() else {
        return;
    };

    let workspace = TestWorkspace::new("live-ws");
    workspace.create();
    workspace.write_sudocode_json();

    let mut client = spawn_ws_client(&workspace, &token).await;
    run_live_scenarios(&mut client, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively find the first `.jsonl` file under `dir`.
fn find_jsonl_file(dir: &std::path::Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_jsonl_file(&path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            return Some(path);
        }
    }
    None
}
