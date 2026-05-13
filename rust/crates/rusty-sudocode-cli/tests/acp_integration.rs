//! ACP integration tests exercising both stdio and WebSocket transports.
//!
//! Each transport runs the same suite of scenarios to verify protocol parity
//! between the SDK-based stdio server and the axum-based WebSocket server.

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const RECV_TIMEOUT: Duration = Duration::from_secs(30);
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Workspace setup (mirrors mock_parity_harness.rs HarnessWorkspace)
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
            "scode-acp-{label}-{}-{millis}-{counter}",
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

    fn write_sudocode_json(&self, base_url: &str) {
        let sample = runtime::SAMPLE_SUDOCODE_JSON
            .replace("https://api.anthropic.com", base_url)
            .replace("<YOUR_ANTHROPIC_API_KEY>", "test-acp-key");
        fs::write(self.config_home.join("sudocode.json"), sample)
            .expect("test sudocode.json should be written");
    }

    fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Transport abstraction
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
    /// Send a JSON-RPC request and collect all messages until the matching
    /// response arrives. Returns `(notifications, response)`.
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
            // A response has a matching numeric id.
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
            .expect("recv timed out after 30s")
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
                    // Try parsing as JSON; skip non-JSON lines (e.g. log output).
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

fn base_command(workspace: &TestWorkspace) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.current_dir(&workspace.root)
        .env_clear()
        .env("SUDO_CODE_CONFIG_HOME", &workspace.config_home)
        .env("HOME", &workspace.home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
        ]);
    cmd
}

fn spawn_stdio_client(workspace: &TestWorkspace) -> AcpTestClient {
    let mut cmd = base_command(workspace);
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

async fn spawn_ws_client(workspace: &TestWorkspace) -> AcpTestClient {
    // Find a free port.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind free port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);

    let mut cmd = base_command(workspace);
    cmd.args(["acp", "serve", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn scode acp serve");

    // Wait for "[acp-ws] listening on" in stderr before connecting.
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

    // Spawn a task to drain stderr so the child doesn't block.
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
// Test scenarios (transport-agnostic)
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
    let agent_info = &result["agentInfo"];
    assert_eq!(agent_info["name"], "scode");
    assert!(
        agent_info.get("version").is_some(),
        "agentInfo should include version"
    );
    assert!(
        result.get("agentCapabilities").is_some(),
        "response should include agentCapabilities"
    );
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
    let result = &resp["result"];
    let session_id = result["sessionId"]
        .as_str()
        .expect("sessionId should be a string");
    assert!(!session_id.is_empty(), "sessionId should not be empty");
    session_id.to_string()
}

async fn scenario_session_prompt_streaming(client: &mut AcpTestClient, session_id: &str) {
    let prompt_text = format!("{SCENARIO_PREFIX}streaming_text");
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": prompt_text }]
            }),
        )
        .await;

    // Streaming prompt should produce session/update notifications with text chunks.
    assert!(
        !notifs.is_empty(),
        "streaming prompt should produce at least one notification"
    );

    let update_count = notifs
        .iter()
        .filter(|n| {
            n.get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.contains("session"))
        })
        .count();
    assert!(
        update_count > 0,
        "should have session update notifications (got {update_count} of {} total)",
        notifs.len()
    );

    // The response should have a stop reason.
    let result = &resp["result"];
    assert!(
        result.get("stopReason").is_some(),
        "prompt response should include stopReason"
    );

    // The response should include token usage data.
    let usage = result.get("usage");
    assert!(
        usage.is_some(),
        "prompt response should include usage (got result: {result})"
    );
    let usage = usage.unwrap();
    assert!(
        usage.get("totalTokens").is_some(),
        "usage should include totalTokens"
    );
    assert!(
        usage["totalTokens"].as_u64().unwrap_or(0) > 0,
        "totalTokens should be > 0"
    );
    assert!(
        usage.get("inputTokens").is_some(),
        "usage should include inputTokens"
    );
    assert!(
        usage.get("outputTokens").is_some(),
        "usage should include outputTokens"
    );
}

async fn scenario_session_list(client: &mut AcpTestClient, expected_session_id: &str) {
    let (notifs, resp) = client.send_request("session/list", json!({})).await;

    assert!(
        notifs.is_empty(),
        "session/list should not produce notifications"
    );
    let result = &resp["result"];
    let sessions = result["sessions"]
        .as_array()
        .expect("sessions should be an array");
    assert!(!sessions.is_empty(), "should have at least one session");

    let found = sessions
        .iter()
        .any(|s| s["sessionId"].as_str() == Some(expected_session_id));
    assert!(
        found,
        "created session {expected_session_id} should appear in session/list"
    );
}

async fn scenario_session_load_not_supported(client: &mut AcpTestClient) {
    let (notifs, resp) = client
        .send_request("session/load", json!({ "sessionId": "nonexistent" }))
        .await;

    assert!(
        notifs.is_empty(),
        "session/load should not produce notifications"
    );
    assert!(
        resp.get("error").is_some(),
        "session/load should return an error response"
    );
}

async fn scenario_unknown_method(client: &mut AcpTestClient) {
    let (notifs, resp) = client.send_request("nonexistent/method", json!({})).await;

    assert!(
        notifs.is_empty(),
        "unknown method should not produce notifications"
    );
    let error = resp.get("error").expect("should have error field");
    assert_eq!(
        error["code"], -32601,
        "unknown method should return -32601 Method not found"
    );
}

async fn scenario_slash_command_model(client: &mut AcpTestClient, session_id: &str) {
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "/model" }]
            }),
        )
        .await;

    // /model should produce text delta notifications.
    let has_updates = notifs.iter().any(|n| {
        n.get("method")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains("session"))
    });
    assert!(
        has_updates,
        "/model should produce session update notifications"
    );

    // Should get a successful response with stop reason.
    let result = &resp["result"];
    assert!(
        result.get("stopReason").is_some(),
        "/model should complete with a stopReason"
    );
}

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

async fn run_all_scenarios(client: &mut AcpTestClient, workspace: &TestWorkspace) {
    scenario_initialize(client).await;
    let session_id = scenario_session_new(client, &workspace.root).await;
    scenario_session_prompt_streaming(client, &session_id).await;
    scenario_session_list(client, &session_id).await;
    scenario_session_load_not_supported(client).await;
    scenario_unknown_method(client).await;
    scenario_slash_command_model(client, &session_id).await;
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

#[tokio::test]
async fn acp_stdio_integration() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    run_all_scenarios(&mut client, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn acp_ws_integration() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("ws");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_ws_client(&workspace).await;
    run_all_scenarios(&mut client, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}
