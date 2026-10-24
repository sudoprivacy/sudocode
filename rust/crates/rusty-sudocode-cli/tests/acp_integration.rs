//! ACP integration tests exercising both stdio and WebSocket transports.
//!
//! Each transport runs the same suite of scenarios to verify protocol parity
//! between the SDK-based stdio server and the axum-based WebSocket server.
//!
//! Runs on Windows too. The old `#![cfg(unix)]` gate blamed "the stderr-pipe
//! contract racing ConPTY/MinGW handles" for `stderr closed before server
//! ready` (and a ~3h windows-latest CI wedge); the real cause was
//! [`isolate_env`]'s `env_clear()` dropping `SystemRoot`, without which the
//! spawned `scode acp` cannot initialise winsock — so `acp serve` fails to bind
//! and dies before printing its ready line. See `common/isolated_env.rs`.

#[path = "support/acp_direct_fences.rs"]
mod acp_direct_fences;
#[path = "common/isolated_env.rs"]
mod isolated_env;
#[path = "support/node_home.rs"]
mod node_home;
#[path = "common/openai_compat_mock.rs"]
mod openai_compat_mock;
#[path = "common/python.rs"]
mod python;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{FutureExt, SinkExt, StreamExt};
use mock_anthropic_service::{
    MockAnthropicService, SCENARIO_PREFIX, SUBAGENT_CHILD_ANSWER, SUBAGENT_CHILD_READ_PATH,
    SUBAGENT_CHILD_TOOL_ID,
};
use openai_compat_mock::OpenAiCompatMock;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
/// Recv timeout for JSON-RPC responses. Bumped 30s → 90s so the
/// wrong-model VLM tests (which trigger `describe_image_via_vlm`'s own
/// 30s HTTP timeout when the mock or real sudorouter is unreachable) have
/// enough headroom for scode's error placeholder to bubble back through
/// the ACP response. 30s was cutting it exactly at the VLM timeout and
/// the test recv panicked before scode's push_images could finish.
const RECV_TIMEOUT: Duration = Duration::from_secs(90);
/// The `available_commands_update` follows a `session/new` / `session/load`
/// response on the same queue, so it is either there at once or the
/// broadcast regressed — fail fast instead of waiting out `RECV_TIMEOUT`.
const AVAILABLE_COMMANDS_TIMEOUT: Duration = Duration::from_secs(10);
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

    /// Variant that also overrides sudorouter's base URL — for tests where a
    /// mock openai-compat endpoint stands in for hk.sudorouter.ai (used by
    /// the VLM-route full-roundtrip test to capture describe_image_via_vlm's
    /// outgoing request). anthropic_url still points at MockAnthropicService
    /// for the LLM call that follows VLM.
    fn write_sudocode_json_with_sudorouter(&self, anthropic_url: &str, sudorouter_url: &str) {
        let sample = runtime::SAMPLE_SUDOCODE_JSON
            .replace("https://api.anthropic.com", anthropic_url)
            .replace("<YOUR_ANTHROPIC_API_KEY>", "test-acp-key")
            .replace("https://hk.sudorouter.ai/v1", sudorouter_url)
            .replace("<YOUR_SUDOROUTER_API_KEY>", "test-sudorouter-key");
        fs::write(self.config_home.join("sudocode.json"), sample)
            .expect("test sudocode.json should be written");
    }

    /// Fetch the fixture through discovery so the child loads its own endpoint's
    /// persisted catalog. A global capability cache must not override this route.
    async fn seed_model_catalog(&self, endpoint: &str) {
        use runtime::model_discovery::{DiscoverySource, ModelCatalog};
        let catalog = ModelCatalog::open(
            &self.config_home,
            DiscoverySource {
                models_url: format!("{endpoint}/v1/models"),
                headers: [
                    ("x-api-key".into(), "test-acp-key".into()),
                    ("anthropic-version".into(), "2023-06-01".into()),
                ]
                .into_iter()
                .collect(),
            },
        );
        catalog
            .refresh(true)
            .await
            .expect("fetch test model catalog");
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
    /// The `available_commands_update` the agent sent after the most recent
    /// successful `session/new` / `session/load` (consumed by
    /// [`AcpTestClient::send_request`] so it never leaks into the next
    /// request's notifications).
    last_available_commands: Option<Value>,
    /// Every stdout line received over stdio, verbatim, in order — for tests
    /// that compare the wire byte for byte.
    raw_lines: Vec<String>,
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
        let response = loop {
            let msg = self.recv().await;
            // A response has a matching numeric id.
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                break msg;
            }
            notifications.push(msg);
        };
        // A successful session/new or session/load is followed by exactly one
        // `available_commands_update` for the new session. Consume and check
        // it here so every session-creating test verifies the broadcast and
        // no test sees it as a stray notification on its next request.
        if matches!(method, "session/new" | "session/load") && response.get("error").is_none() {
            let update = timeout(AVAILABLE_COMMANDS_TIMEOUT, async {
                loop {
                    let message = self.recv_inner().await;
                    // Durable delivery is independent of the load response;
                    // retain its requests while awaiting command metadata.
                    if is_server_request(&message, "_sudocode/task/completed")
                        || is_server_request(&message, "_sudocode/subagent/completed")
                    {
                        notifications.push(message);
                        continue;
                    }
                    break message;
                }
            })
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "{method} succeeded but no available_commands_update arrived within {AVAILABLE_COMMANDS_TIMEOUT:?}"
                    )
                });
            assert_eq!(
                update["method"].as_str(),
                Some("session/update"),
                "{method} must be followed by a session/update, got: {update}"
            );
            assert_eq!(
                update["params"]["update"]["sessionUpdate"].as_str(),
                Some("available_commands_update"),
                "{method} must be followed by available_commands_update, got: {update}"
            );
            if let Some(expected) = params["sessionId"].as_str() {
                assert_eq!(
                    update["params"]["sessionId"].as_str(),
                    Some(expected),
                    "available_commands_update must target the loaded session"
                );
            }
            self.last_available_commands = Some(update);
        }
        (notifications, response)
    }

    /// Send a JSON-RPC request WITHOUT waiting for its response. Returns the
    /// request id so the caller can pick the response out of the stream later
    /// with [`AcpTestClient::recv_until`]. Used by the cross-session
    /// concurrency scenarios where one request is deliberately left pending.
    async fn send_request_no_wait(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_raw(&request).await;
        id
    }

    /// Read messages until `pred` matches one, returning it together with
    /// every message that arrived before it. Returns `Err(seen)` if `limit`
    /// elapses first (the messages seen so far are returned for diagnostics).
    async fn recv_until(
        &mut self,
        limit: Duration,
        pred: impl Fn(&Value) -> bool,
    ) -> Result<(Vec<Value>, Value), Vec<Value>> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(seen);
            }
            match timeout(remaining, self.recv_inner()).await {
                Ok(msg) => {
                    if pred(&msg) {
                        return Ok((seen, msg));
                    }
                    seen.push(msg);
                }
                Err(_) => return Err(seen),
            }
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
            .expect("recv timed out (see RECV_TIMEOUT)")
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
                        self.raw_lines.push(trimmed.to_string());
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

/// Give `cmd` a hermetic environment: `env_clear()` so the developer's real
/// config and API keys can't leak in, then the test's own config home / HOME
/// plus the per-platform vars a spawned child still needs (`isolated_env`).
fn isolate_env(cmd: &mut Command, workspace: &TestWorkspace) {
    cmd.env_clear()
        .env("SUDO_CODE_CONFIG_HOME", &workspace.config_home)
        .env("HOME", &workspace.home)
        .env("NO_COLOR", "1");
    for (key, value) in isolated_env::inherited_env() {
        cmd.env(key, value);
    }
}

/// Own the CLI for the fixture lifetime, including assertion unwinding.
fn isolated_command(workspace: &TestWorkspace, binary: &Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.current_dir(&workspace.root).kill_on_drop(true);
    isolate_env(&mut cmd, workspace);
    cmd
}

fn base_command(workspace: &TestWorkspace) -> Command {
    let mut cmd = isolated_command(workspace, Path::new(env!("CARGO_BIN_EXE_scode")));
    cmd.args([
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
    spawn_stdio_client_with_args(workspace, &[])
}

/// Like [`spawn_stdio_client`] but passes extra flags to the `scode acp`
/// process, so a test can exercise the process-wide prompt flags a session's
/// `_meta` overrides layer on top of.
fn spawn_stdio_client_with_args(workspace: &TestWorkspace, extra: &[&str]) -> AcpTestClient {
    spawn_stdio_client_with_args_and_env(workspace, extra, &[])
}

/// Like [`spawn_stdio_client_with_args`] but also sets extra environment
/// variables on the `scode acp` process (`base_command` clears the
/// environment, so runtime knobs such as the auto-compaction threshold have
/// to be passed explicitly).
fn spawn_stdio_client_with_args_and_env(
    workspace: &TestWorkspace,
    extra: &[&str],
    env: &[(&str, &str)],
) -> AcpTestClient {
    let mut cmd = base_command(workspace);
    cmd.arg("acp")
        .args(extra)
        .envs(env.iter().copied())
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
        last_available_commands: None,
        raw_lines: Vec::new(),
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
        last_available_commands: None,
        raw_lines: Vec::new(),
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
    let caps = result
        .get("agentCapabilities")
        .expect("response should include agentCapabilities");
    // `session/load` is implemented (see the resume-across-processes test
    // below); clients such as sudowork / apeiron gate reconnect-after-restart
    // on this flag, so it must be advertised.
    assert_eq!(
        caps["loadSession"].as_bool(),
        Some(true),
        "agentCapabilities.loadSession must be advertised: {caps}"
    );

    // Image-handling SSOT: assert the `_meta.sudocode.imageCapability` extension
    // is advertised on every initialize response. Per the design doc
    // `docs/design/image-handling-non-user-facing.html` (Decision 1), this is
    // how sudowork (and any other ACP client) learns what byte limits sudocode
    // accepts and whether sudocode handles oversized + wrong-model internally
    // — without it the client would have to hardcode caps or wrap fallbacks
    // unnecessarily (the original 进二 bug class).
    let img_cap = result
        .get("_meta")
        .and_then(|m| m.get("sudocode"))
        .and_then(|s| s.get("imageCapability"))
        .expect("initialize response must carry _meta.sudocode.imageCapability");
    for field in [
        "maxBytes",
        "maxDimension",
        "downsampleTargetBytes",
        "autoHandlesOversized",
        "autoHandlesWrongModel",
    ] {
        assert!(
            img_cap.get(field).is_some(),
            "_meta.sudocode.imageCapability must include `{field}` (got: {img_cap})"
        );
    }
    // Documented values from image_registry::capability() — guard against
    // drift between source-of-truth (image_registry.rs constants) and what
    // the wire actually carries.
    assert_eq!(img_cap["maxBytes"].as_u64(), Some(5 * 1024 * 1024));
    assert_eq!(img_cap["maxDimension"].as_u64(), Some(8000));
    assert_eq!(img_cap["downsampleTargetBytes"].as_u64(), Some(512 * 1024));
    assert_eq!(img_cap["autoHandlesOversized"].as_bool(), Some(true));
    assert_eq!(img_cap["autoHandlesWrongModel"].as_bool(), Some(true));

    // `_meta.sudocode.systemPromptOverride` / `systemPromptAppend` tell
    // clients (apeiron, sudowork) that `session/new` / `session/load` honour
    // `_meta.sudocode.systemPrompt` / `appendSystemPrompt` — see
    // `acp_session_new_system_prompt_override_and_append_reach_model`.
    // `sessionMemory` tells them `_meta.sudocode.memory` is honoured — see
    // `acp_session_memory_toggle_is_per_session`.
    for flag in [
        "systemPromptOverride",
        "systemPromptAppend",
        "sessionMemory",
    ] {
        assert_eq!(
            result["_meta"]["sudocode"][flag].as_bool(),
            Some(true),
            "initialize must advertise _meta.sudocode.{flag}"
        );
    }
    // Echoed only to a client that asked for it (see the sub-agent events
    // tests); this client did not, so the result must not mention it.
    assert!(
        result["_meta"]["sudocode"].get("subagentEvents").is_none(),
        "subagentEvents must not be advertised to a client that did not opt in: {result}"
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
    assert_available_commands_update(
        client
            .last_available_commands
            .as_ref()
            .expect("session/new should be followed by available_commands_update"),
        session_id,
    );
    session_id.to_string()
}

/// The `available_commands_update` broadcast: addressed to `session_id`,
/// shaped per the ACP schema (`availableCommands: [{name, description,
/// input?: {hint}}]`), and listing the commands apeiron needs.
fn assert_available_commands_update(update: &Value, session_id: &str) {
    assert_eq!(update["params"]["sessionId"].as_str(), Some(session_id));
    let commands = update["params"]["update"]["availableCommands"]
        .as_array()
        .unwrap_or_else(|| panic!("availableCommands should be an array: {update}"));
    let names: Vec<&str> = commands
        .iter()
        .map(|c| c["name"].as_str().expect("command name is a string"))
        .collect();
    for expected in ["compact", "help", "model", "status"] {
        assert!(
            names.contains(&expected),
            "available commands should include `{expected}`, got {names:?}"
        );
    }
    assert!(
        !names.contains(&"clear"),
        "/clear is not offered in ACP mode, got {names:?}"
    );
    for command in commands {
        assert!(
            !command["name"].as_str().unwrap_or("").starts_with('/'),
            "command names carry no leading slash: {command}"
        );
        assert!(
            command["description"]
                .as_str()
                .is_some_and(|d| !d.is_empty()),
            "every command has a description: {command}"
        );
        if let Some(input) = command.get("input") {
            assert!(
                input["hint"].as_str().is_some(),
                "input, when present, is {{hint}}: {command}"
            );
        }
    }
    let model = commands
        .iter()
        .find(|c| c["name"] == "model")
        .expect("model command");
    assert!(
        model["input"]["hint"].as_str().is_some(),
        "/model advertises an input hint: {model}"
    );
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

/// `session/load` of an id that was never persisted must fail cleanly (it
/// must not mint a fresh session under that id).
async fn scenario_session_load_unknown_session_errors(client: &mut AcpTestClient) {
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

async fn scenario_tool_pause(client: &mut AcpTestClient, workspace: &TestWorkspace) {
    scenario_initialize(client).await;
    let session_id = scenario_session_new(client, &workspace.root).await;
    let (_, paused) = client
        .send_request(
            "_sudocode/session/pause_tools",
            json!({ "sessionId": session_id }),
        )
        .await;
    assert_eq!(paused["result"]["isQuiescent"], true, "{paused}");
    let epoch = paused["result"]["pauseEpoch"]
        .as_u64()
        .expect("pause epoch");
    let (_, repeated) = client
        .send_request(
            "_sudocode/session/pause_tools",
            json!({ "sessionId": session_id }),
        )
        .await;
    assert_eq!(repeated["result"]["pauseEpoch"], epoch);
    let prompt = json!({"sessionId": session_id,"prompt":[{"type":"text","text":"! printf unauthorized > pause-denied.txt"}]});
    let (_, denied) = client.send_request("session/prompt", prompt.clone()).await;
    assert!(denied.get("error").is_some(), "{denied}");
    assert!(!workspace.root.join("pause-denied.txt").exists());
    let (_, stale) = client
        .send_request(
            "_sudocode/session/resume_tools",
            json!({"sessionId":session_id,"pauseEpoch":epoch+1}),
        )
        .await;
    assert!(stale.get("error").is_some(), "{stale}");
    let (_, still_denied) = client.send_request("session/prompt", prompt).await;
    assert!(still_denied.get("error").is_some(), "{still_denied}");
    let (_, reload) = client
        .send_request(
            "session/load",
            json!({"sessionId":session_id,"cwd":workspace.root.to_string_lossy(),"mcpServers":[]}),
        )
        .await;
    assert!(
        reload.get("error").is_some(),
        "reload must preserve human control: {reload}"
    );
    let (_, resumed) = client
        .send_request(
            "_sudocode/session/resume_tools",
            json!({"sessionId":session_id,"pauseEpoch":epoch}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let (_, completed) = client.send_request("session/prompt", json!({"sessionId":session_id,"prompt":[{"type":"text","text":"! printf resumed > pause-resumed.txt"}]})).await;
    assert!(completed.get("error").is_none(), "{completed}");
    assert_eq!(
        fs::read_to_string(workspace.root.join("pause-resumed.txt")).expect("actual shell receipt"),
        "resumed"
    );
    let (_, unknown) = client
        .send_request(
            "_sudocode/session/pause_tools",
            json!({"sessionId":"unknown-session"}),
        )
        .await;
    assert!(unknown.get("error").is_some(), "{unknown}");
}

#[tokio::test]
async fn acp_stdio_tool_pause_round_trip() {
    let server = MockAnthropicService::spawn().await.expect("mock service");
    let workspace = TestWorkspace::new("stdio-tool-pause");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client(&workspace);
    let outcome = std::panic::AssertUnwindSafe(scenario_tool_pause(&mut client, &workspace))
        .catch_unwind()
        .await;
    client.shutdown().await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
    workspace.cleanup();
}

#[tokio::test]
async fn acp_websocket_tool_pause_round_trip() {
    let server = MockAnthropicService::spawn().await.expect("mock service");
    let workspace = TestWorkspace::new("ws-tool-pause");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_ws_client(&workspace).await;
    let outcome = std::panic::AssertUnwindSafe(scenario_tool_pause(&mut client, &workspace))
        .catch_unwind()
        .await;
    client.shutdown().await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
    workspace.cleanup();
}

#[tokio::test]
async fn acp_tool_pause_drains_a_running_child_shell() {
    let server = MockAnthropicService::spawn().await.expect("mock service");
    let workspace = subagent_events_workspace("tool-pause-child", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    let outcome = std::panic::AssertUnwindSafe(async {
        initialize_with_subagent_events(&mut client).await;
        let session = scenario_session_new(&mut client, &workspace.root).await;
        let (during, _) = prompt_scenario(&mut client, &session, "subagent_events_cancel_tool").await;
        let (agent, _) = during.iter().find_map(lifecycle_of).expect("child started");
        let sleep_call = format!("{agent}:toolu_child_sleep");
        let is_sleep_call = |message: &Value| message["params"]["update"]["toolCallId"] == sleep_call
            && message["params"]["update"]["sessionUpdate"] == "tool_call";
        if !during.iter().any(is_sleep_call) {
            client.recv_until(Duration::from_secs(30), is_sleep_call).await.expect("child shell started");
        }
        let (_, paused) = client.send_request("_sudocode/session/pause_tools", json!({"sessionId":session})).await;
        assert_eq!(paused["result"]["isQuiescent"], true, "{paused}");
        let manifest = workspace.root.join(".sudocode-agents").join(format!("{agent}.json"));
        let state: Value = serde_json::from_slice(&fs::read(manifest).expect("child manifest")).unwrap();
        assert_eq!(state["status"], "cancelled", "pause must wait for child termination: {state}");
        let (_, resumed) = client.send_request("_sudocode/session/resume_tools", json!({"sessionId":session,"pauseEpoch":paused["result"]["pauseEpoch"]})).await;
        assert!(resumed.get("error").is_none(), "{resumed}");
        let (_, finished) = client.send_request("session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"! printf returned > pause-returned.txt"}]})).await;
        assert!(finished.get("error").is_none(), "{finished}");
        assert_eq!(fs::read_to_string(workspace.root.join("pause-returned.txt")).expect("post-handoff shell"), "returned");
    }).catch_unwind().await;
    client.shutdown().await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
    workspace.cleanup();
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

/// Test that prompt usage returns per-turn (not cumulative) values.
/// Uses a fresh session to avoid interference from other scenarios.
async fn scenario_session_prompt_per_turn_usage(
    client: &mut AcpTestClient,
    workspace: &TestWorkspace,
) {
    // Create a fresh session for this test to avoid accumulated usage from other scenarios
    let session_id = scenario_session_new(client, &workspace.root).await;

    // First prompt
    let prompt_text1 = format!("{SCENARIO_PREFIX}streaming_text");
    let (_notifs1, resp1) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": prompt_text1 }]
            }),
        )
        .await;

    let result1 = &resp1["result"];
    let usage1 = result1
        .get("usage")
        .expect("first prompt should have usage");
    let first_turn_total = usage1["totalTokens"]
        .as_u64()
        .expect("first turn should have totalTokens");
    assert!(first_turn_total > 0, "first turn totalTokens should be > 0");

    // Check _meta.sudocode.cumulativeUsage exists for first prompt
    let meta1 = result1
        .get("_meta")
        .expect("first prompt should have _meta");
    let sudocode1 = meta1.get("sudocode").expect("_meta should have sudocode");
    assert!(
        sudocode1.get("cumulativeUsage").is_some(),
        "_meta.sudocode should have cumulativeUsage"
    );
    let cumulative1 = &sudocode1["cumulativeUsage"];
    let cumulative_total1 = cumulative1["totalTokens"]
        .as_u64()
        .expect("cumulativeUsage should have totalTokens");

    // Second prompt in the same session
    let prompt_text2 = format!("{SCENARIO_PREFIX}streaming_text");
    let (_notifs2, resp2) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": prompt_text2 }]
            }),
        )
        .await;

    let result2 = &resp2["result"];
    let usage2 = result2
        .get("usage")
        .expect("second prompt should have usage");
    let second_turn_total = usage2["totalTokens"]
        .as_u64()
        .expect("second turn should have totalTokens");
    assert!(
        second_turn_total > 0,
        "second turn totalTokens should be > 0"
    );

    // Check _meta.sudocode.cumulativeUsage for second prompt
    let meta2 = result2
        .get("_meta")
        .expect("second prompt should have _meta");
    let sudocode2 = meta2.get("sudocode").expect("_meta should have sudocode");
    let cumulative2 = sudocode2
        .get("cumulativeUsage")
        .expect("should have cumulativeUsage");
    let cumulative_total2 = cumulative2["totalTokens"]
        .as_u64()
        .expect("cumulativeUsage should have totalTokens");

    // Key assertions:
    // 1. usage.totalTokens should be per-turn (NOT cumulative)
    //    So second_turn_total should NOT be the sum of both turns
    assert!(
        second_turn_total < cumulative_total2,
        "second turn usage ({}) should be per-turn (less than cumulative {})",
        second_turn_total,
        cumulative_total2
    );

    // 2. cumulative total should be greater than first turn (because it includes both turns)
    assert!(
        cumulative_total2 > cumulative_total1,
        "cumulative total after second turn ({}) should be greater than after first turn ({})",
        cumulative_total2,
        cumulative_total1
    );

    // 3. cumulative should be at least the sum of per-turn values
    let sum_of_turns = first_turn_total + second_turn_total;
    assert!(
        cumulative_total2 >= sum_of_turns,
        "cumulative ({}) should be at least the sum of per-turn values ({} + {} = {})",
        cumulative_total2,
        first_turn_total,
        second_turn_total,
        sum_of_turns
    );
}

/// Push a small image (1×1 PNG) inline in `session/prompt`. Exercises the
/// push_images path end-to-end: the cli must NOT crash on the new VLM-route
/// branch even when the active model is happily vision-capable + the image
/// is well under cap (i.e. the trivially-native path through the new
/// 3-branch decision tree at main.rs:push_images).
///
/// Per the design doc (Decision 1 graceful-degradation hard rule), the
/// response must always succeed; no image-related tip can leak through.
async fn scenario_session_prompt_with_image_attachment(
    client: &mut AcpTestClient,
    session_id: &str,
) {
    // 67-byte 1×1 transparent PNG — smaller than every conceivable cap.
    // Generated once, hardcoded for determinism.
    const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=";

    let prompt_text = format!("{SCENARIO_PREFIX}streaming_text");
    let (_notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "image",
                        "data": TINY_PNG_BASE64,
                        "mimeType": "image/png"
                    },
                    { "type": "text", "text": prompt_text }
                ]
            }),
        )
        .await;

    let result = &resp["result"];
    assert!(
        result.get("stopReason").is_some(),
        "image-attached prompt must complete with a stopReason (not error) \
         — graceful degradation invariant per design Decision 1. Got: {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "image-attached prompt must NOT return an error — sudocode must \
         handle every image internally. Got: {resp}"
    );
}

fn assert_compaction_lifecycle(notifs: &[Value], status: &str) {
    let start = notifs
        .iter()
        .position(|n| n["params"]["update"]["title"] == "context_compaction")
        .expect("compaction must announce its start");
    let started = &notifs[start]["params"]["update"];
    assert_eq!(started["status"], "in_progress");
    let end = notifs
        .iter()
        .position(|n| n["params"]["update"]["rawOutput"]["status"] == status)
        .expect("compaction must announce its terminal state before prompt response");
    assert!(start < end);
    let ended = &notifs[end]["params"]["update"];
    assert_eq!(started["toolCallId"], ended["toolCallId"]);
    assert_eq!(
        ended["status"],
        if status == "failed" {
            "failed"
        } else {
            "completed"
        }
    );
}

/// Send one slash command and return the concatenated agent text it produced
/// plus the response.
async fn run_slash_command(
    client: &mut AcpTestClient,
    session_id: &str,
    command: &str,
) -> (String, Value) {
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": command }]
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "{command} should end the turn normally: {resp}"
    );
    if command == "/compact" {
        assert_compaction_lifecycle(&notifs, "completed");
    }
    let text = notifs
        .iter()
        .filter(|m| {
            m["params"]["sessionId"].as_str() == Some(session_id)
                && m["params"]["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk")
        })
        .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    assert!(
        !text.is_empty(),
        "{command} should produce text: {notifs:?}"
    );
    (text, resp)
}

/// Run one `streaming_text` turn tagged with `marker` and return the
/// response (for `_meta`) — the marker later identifies the turn in the
/// model's request bodies and in the transcript on disk.
async fn run_marked_turn(client: &mut AcpTestClient, session_id: &str, marker: &str) -> Value {
    let (_notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text {marker}") }]
            }),
        )
        .await;
    assert!(
        resp["result"].get("stopReason").is_some(),
        "turn {marker} should complete: {resp}"
    );
    resp
}

/// Read message history from the transcript; sibling journals and locks are control data.
fn read_session_transcript(root: &std::path::Path, session_id: &str) -> String {
    fs::read_to_string(find_session_transcript(root, session_id)).expect("read transcript")
}

/// `/compact`: after three turns the long oldest exchange is summarised (the
/// mock answers compaction requests with its canned summary, so the LLM path
/// is taken), the compaction is on disk immediately, and the next model
/// request no longer carries the summarised turn.
async fn scenario_slash_compact(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
) {
    let session_id = scenario_session_new(client, &workspace.root).await;
    for marker in ["ACP_COMPACT_ONE", "ACP_COMPACT_TWO", "ACP_COMPACT_THREE"] {
        // The old exchange must dominate the four protected recent messages
        // so the summary plus continuation framing can meet the 50% target.
        let repetitions = if marker == "ACP_COMPACT_ONE" {
            800
        } else {
            100
        };
        run_marked_turn(
            client,
            &session_id,
            &format!(
                "{marker} {}",
                "migration design context. ".repeat(repetitions)
            ),
        )
        .await;
    }
    let before = read_session_transcript(&workspace.root, &session_id);
    assert!(
        !before.contains("\"type\":\"compaction\""),
        "no compaction record before /compact"
    );
    let transcript_path = find_session_transcript(&workspace.root, &session_id);
    let source = runtime::Session::load_from_path(&transcript_path)
        .expect("source transcript should load before /compact");
    let before_history = runtime::estimate_session_tokens(&source);

    let (text, _resp) = run_slash_command(client, &session_id, "/compact").await;
    assert!(
        text.contains("Result           compacted"),
        "/compact should report a compaction, got: {text}"
    );
    assert!(
        text.contains("Method           llm summary"),
        "/compact should take the LLM path against the mock, got: {text}"
    );
    let removed: usize = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Messages removed"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("report should carry the removed count: {text}"));
    assert!(removed >= 1, "/compact should remove messages: {text}");
    assert!(
        text.contains("Estimated tokens"),
        "/compact should report the token estimate: {text}"
    );

    // Persisted right away, not at the end of the next turn.
    let compacted = runtime::Session::load_from_path(&transcript_path)
        .expect("compacted transcript should load immediately after /compact");
    assert!(
        compacted.compaction.is_some(),
        "compaction metadata should be on disk right after /compact"
    );
    let report = compacted
        .last_compaction_report
        .as_ref()
        .expect("successful /compact should persist its final report");
    assert_eq!(report.outcome, runtime::CompactionOutcome::TargetMet);
    assert_eq!(report.before_history, before_history);
    assert_eq!(
        report.after_history,
        runtime::estimate_session_tokens(&compacted)
    );
    assert!(report.after_history <= report.before_history / 2);
    assert!(report.after_history <= report.safe_history_budget);

    // The model now sees the summary instead of the oldest turn.
    let body =
        last_model_request_after_prompt(client, server, &session_id, "ACP_COMPACT_FOUR").await;
    assert!(
        !body.contains("ACP_COMPACT_ONE"),
        "summarised turn must not reach the model after /compact"
    );
    assert!(
        body.contains("ACP_COMPACT_THREE"),
        "preserved recent turn must still reach the model"
    );
}

/// `session/cancel` during `/compact`: the turn ends with `cancelled`, and
/// message history remains unchanged in memory and on disk while the final
/// report and actual maintenance attempts are persisted.
///
/// The first turn uses the mock's `delayed_text` scenario. Compaction
/// requests are classified by the markers of the messages being summarised,
/// and with three turns the first one is what gets summarised — so the
/// compaction request itself is held for `DELAYED_TEXT_LATENCY`, which is the
/// window in which the cancel lands.
async fn scenario_slash_compact_cancel(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
) {
    let session_id = scenario_session_new(client, &workspace.root).await;
    let (_n, first) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": format!(
                    "{SCENARIO_PREFIX}delayed_text ACP_CANCEL_ONE {}",
                    "migration design context. ".repeat(800)
                ) }]
            }),
        )
        .await;
    assert!(first["result"].get("stopReason").is_some(), "{first}");
    for marker in ["ACP_CANCEL_TWO", "ACP_CANCEL_THREE"] {
        run_marked_turn(
            client,
            &session_id,
            &format!("{marker} {}", "migration design context. ".repeat(200)),
        )
        .await;
    }
    let transcript_path = find_session_transcript(&workspace.root, &session_id);
    let source = runtime::Session::load_from_path(&transcript_path)
        .expect("source transcript should load before cancellation");
    let requests_before = server.captured_requests().await.len();

    let compact_id = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "/compact" }]
            }),
        )
        .await;
    // The start is observable while the model is still busy, not buffered
    // until the final prompt response. Cancel only after seeing it.
    let (mut prefix, start) = client
        .recv_until(Duration::from_secs(20), |n| {
            n["params"]["update"]["title"] == "context_compaction"
        })
        .await
        .expect("live compaction start");
    prefix.push(start);
    // Wait until the compaction request is in flight at the mock.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let requests = server.captured_requests().await;
        if requests[requests_before..]
            .iter()
            .any(|r| r.scenario == "delayed_text")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "/compact never issued its compaction request"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id }
        }))
        .await;
    let (notifs, resp) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, compact_id))
        .await
        .unwrap_or_else(|seen| panic!("/compact never answered after cancel; seen: {seen:?}"));
    assert_eq!(
        resp["result"]["stopReason"].as_str(),
        Some("cancelled"),
        "cancelled /compact must end with stopReason cancelled: {resp}"
    );
    let text = notifs
        .iter()
        .filter(|m| m["params"]["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk"))
        .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    assert!(text.contains("cancelled"), "got: {text}");
    prefix.extend(notifs);
    assert_compaction_lifecycle(&prefix, "cancelled");

    // Durable message history is unchanged; the cancelled run's maintenance
    // report and receipt ledger still survive for accounting and inspection.
    assert_cancelled_compaction_persisted(&transcript_path, &source);
    // …or in memory: the next turn still carries the would-be-summarised turn.
    let body =
        last_model_request_after_prompt(client, server, &session_id, "ACP_CANCEL_FOUR").await;
    assert!(
        body.contains("ACP_CANCEL_ONE"),
        "a cancelled /compact must not replace the in-memory transcript"
    );
}

fn assert_cancelled_compaction_persisted(transcript_path: &Path, source: &runtime::Session) {
    let restored = runtime::Session::load_from_path(transcript_path)
        .expect("cancelled compaction transcript should load");
    assert_eq!(restored.messages(), source.messages());
    assert_eq!(restored.compaction, source.compaction);
    assert_eq!(
        restored.compaction_achievement,
        source.compaction_achievement
    );
    let report = restored
        .last_compaction_report
        .as_ref()
        .expect("cancelled /compact should persist its final report");
    assert_eq!(report.outcome, runtime::CompactionOutcome::Cancelled);
    assert_eq!(
        report.before_history,
        runtime::estimate_session_tokens(source)
    );
    assert_eq!(report.after_history, report.before_history);
    assert!((1..=4).contains(&report.attempts), "{report:?}");
    assert_eq!(report.completed_responses, 0);
    let receipts: Vec<_> = restored
        .maintenance_usage
        .iter()
        .filter(|receipt| receipt.run_id == report.run_id)
        .collect();
    assert_eq!(receipts.len(), report.attempts);
    assert!(receipts.iter().all(|receipt| receipt.usage.is_none()));
    assert!(receipts
        .iter()
        .all(|receipt| (1..=4).contains(&receipt.attempt_id)));
    assert_eq!(
        restored.maintenance_usage.len(),
        source.maintenance_usage.len() + receipts.len()
    );
}

/// `/help` renders only the ACP table; an unknown command names the table.
async fn scenario_slash_help_and_unknown(client: &mut AcpTestClient, session_id: &str) {
    let (help, _resp) = run_slash_command(client, session_id, "/help").await;
    for expected in ["/compact", "/model <model-id>", "/status"] {
        assert!(
            help.contains(expected),
            "/help should list {expected}: {help}"
        );
    }
    for repl_only in ["/exit", "/resume", "/clear", "Ctrl-R"] {
        assert!(
            !help.contains(repl_only),
            "/help under ACP must not advertise REPL-only `{repl_only}`: {help}"
        );
    }

    let (unknown, _resp) = run_slash_command(client, session_id, "/nonexistent").await;
    assert!(
        unknown.contains("`/nonexistent` is not supported in ACP mode"),
        "got: {unknown}"
    );
    for expected in ["/compact", "/help"] {
        assert!(
            unknown.contains(expected),
            "unknown-command hint should list {expected}: {unknown}"
        );
    }
    assert!(
        !unknown.contains("/clear"),
        "unknown-command hint must not offer /clear: {unknown}"
    );

    // `/clear` itself is a REPL-only command under ACP.
    let (clear, _resp) = run_slash_command(client, session_id, "/clear").await;
    assert!(
        clear.contains("`/clear` is not supported in ACP mode"),
        "got: {clear}"
    );
}

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

async fn run_all_scenarios(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
) {
    scenario_initialize(client).await;
    let session_id = scenario_session_new(client, &workspace.root).await;
    scenario_session_prompt_streaming(client, &session_id).await;
    scenario_session_prompt_with_image_attachment(client, &session_id).await;
    scenario_session_list(client, &session_id).await;
    scenario_session_load_unknown_session_errors(client).await;
    scenario_unknown_method(client).await;
    scenario_slash_command_model(client, &session_id).await;
    scenario_slash_help_and_unknown(client, &session_id).await;
    scenario_slash_compact(client, server, workspace).await;
    scenario_slash_compact_cancel(client, server, workspace).await;
    // Run per-turn usage test last with a fresh session
    scenario_session_prompt_per_turn_usage(client, workspace).await;
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
    run_all_scenarios(&mut client, &server, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}

/// When the host closes the stdio connection (its stdin reaches EOF), the
/// agent must exit on its own instead of lingering as an orphaned process.
#[tokio::test]
async fn acp_stdio_exits_on_stdin_close() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-eof");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    // Drive a normal handshake so the server is fully up before we disconnect.
    scenario_initialize(&mut client).await;

    let (mut child, stdin) = match client.transport {
        Transport::Stdio { child, stdin, .. } => (child, stdin),
        Transport::WebSocket { .. } => panic!("expected stdio transport"),
    };

    // Closing stdin signals EOF to the agent, mirroring a host that
    // disconnected or was killed.
    drop(stdin);

    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("agent should exit promptly after stdin closes")
        .expect("waiting on child should succeed");
    assert!(
        status.success() || status.code().is_some(),
        "agent should terminate cleanly after stdin EOF, got {status:?}"
    );

    workspace.cleanup();
}

/// Resume across a process restart must restore prior conversation history.
///
/// Regression guard for the "amnesia on resume" bug. sudowork resumes a scode
/// session by id via the ACP-standard `session/load`; previously it sent a
/// generic `resumeSessionId` to `session/new`, which scode ignores, silently
/// minting a fresh EMPTY session and losing all history. This test creates a
/// session + one turn carrying a unique marker in process A, lets that process
/// exit, then in a FRESH process B loads the same session id and runs another
/// turn — asserting the upstream model request still carries process A's
/// message (proving history was restored, not started fresh).
/// `!<command>` over ACP: the prompt runs in the session workspace, the
/// output streams back as agent text, no model request is made, and the
/// exchange is persisted as `<bash-input>` / `<bash-stdout>` user messages
/// that the next real turn carries to the model.
#[tokio::test]
async fn acp_stdio_bang_prompt_runs_shell_without_model_turn() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-bang");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;

    let before = server.captured_requests().await.len();
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "! printf 'acp-bang-marker-%s' 7" }]
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "! prompt should end the turn normally: {resp}"
    );
    let text = notifs
        .iter()
        .filter(|m| {
            m["params"]["sessionId"].as_str() == Some(&session_id)
                && m["params"]["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk")
        })
        .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    assert!(
        text.contains("$ printf 'acp-bang-marker-%s' 7") && text.contains("acp-bang-marker-7"),
        "! output should stream back as agent text: {text:?}"
    );
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "a ! prompt must not call the model"
    );

    let transcript = read_session_transcript(&workspace.root, &session_id);
    assert!(
        transcript.contains("<bash-input>printf 'acp-bang-marker-%s' 7</bash-input>"),
        "transcript should record the bash input: {transcript}"
    );
    assert!(
        transcript.contains("<bash-stdout>acp-bang-marker-7</bash-stdout>"),
        "transcript should record the bash output: {transcript}"
    );

    // A host that prepends `<system-reminder>` notes to the user's text (apeiron
    // does, on every prompt) still gets bash mode: the reminder is skipped.
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "<system-reminder>\nworkspace note\n</system-reminder>\n\n! printf 'acp-bang-after-note'" }]
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "{resp}"
    );
    let text = notifs
        .iter()
        .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    assert!(
        text.contains("acp-bang-after-note"),
        "reminder-prefixed ! prompt should run the command: {text:?}"
    );
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "a reminder-prefixed ! prompt must not call the model"
    );

    // The next real turn carries the exchange to the model.
    run_marked_turn(&mut client, &session_id, "after-bang").await;
    let requests = server.captured_requests().await;
    let body = &requests
        .last()
        .expect("the marked turn should reach the mock")
        .raw_body;
    assert!(
        body.contains("<bash-input>") && body.contains("acp-bang-marker-7"),
        "the model request should carry the ! exchange: {body}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn acp_stdio_resume_restores_history_across_reconnect() {
    const HISTORY_MARKER: &str = "resume-marker-7f3a91c2";

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-resume");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    // --- Process A: create a session, run one turn carrying HISTORY_MARKER, then exit ---
    let session_id = {
        let mut client = spawn_stdio_client(&workspace);
        scenario_initialize(&mut client).await;
        let session_id = scenario_session_new(&mut client, &workspace.root).await;

        // The marker is a separate whitespace token, so detect_scenario still
        // resolves `streaming_text`; the marker rides along into the persisted
        // user message.
        let first_prompt = format!("{SCENARIO_PREFIX}streaming_text {HISTORY_MARKER}");
        let (_notifs, resp) = client
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": first_prompt }]
                }),
            )
            .await;
        assert!(
            resp["result"].get("stopReason").is_some(),
            "first turn should complete: {resp}"
        );
        client.shutdown().await;
        session_id
    };

    // --- Process B: a brand-new server process over the same workspace ---
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;

    // session/load of a session created by the previous process must SUCCEED.
    let (load_notifs, load_resp) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": workspace.root.to_string_lossy().to_string(),
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_notifs.is_empty(),
        "session/load should not produce notifications"
    );
    assert!(
        load_resp.get("error").is_none(),
        "session/load of a prior-process session should succeed, got: {load_resp}"
    );

    // A follow-up turn on the resumed session must carry process A's message
    // upstream — that only happens if history was restored from disk.
    let before = server.captured_requests().await.len();
    let (_n, follow_resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}streaming_text follow-up")
                }]
            }),
        )
        .await;
    assert!(
        follow_resp["result"].get("stopReason").is_some(),
        "resumed turn should complete: {follow_resp}"
    );

    let requests = server.captured_requests().await;
    assert!(
        requests.len() > before,
        "resumed prompt should reach the model"
    );
    let last = requests.last().expect("at least one captured request");
    assert!(
        last.raw_body.contains(HISTORY_MARKER),
        "resumed model request must include the prior turn's message ({HISTORY_MARKER}); \
         a fresh/empty session would omit it. body: {}",
        last.raw_body
    );
    // The full transcript is restored, not just the last user turn: the
    // assistant reply from process A must be in the request as well.
    assert!(
        last.raw_body.contains("\"role\":\"assistant\""),
        "resumed model request must carry process A's assistant turn; body: {}",
        last.raw_body
    );

    // The loaded session is a first-class live session again.
    scenario_session_list(&mut client, &session_id).await;
    scenario_session_load_rejects_other_cwd(&mut client, &workspace, &session_id).await;

    client.shutdown().await;
    workspace.cleanup();
}

/// Boundary of `session/load`: sessions are stored per workspace
/// (`<cwd>/.scode/sessions/<fingerprint(cwd)>/<id>.jsonl`) and a load
/// validates the persisted `workspace_root`, so loading the same id from a
/// *different* cwd is rejected rather than silently re-homed. Cross-cwd
/// continuation is a fork, not a load.
async fn scenario_session_load_rejects_other_cwd(
    client: &mut AcpTestClient,
    workspace: &TestWorkspace,
    session_id: &str,
) {
    let other_cwd = workspace.root.join("elsewhere");
    fs::create_dir_all(&other_cwd).expect("create other cwd");
    let (_, cross_resp) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": other_cwd.to_string_lossy().to_string(),
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        cross_resp.get("error").is_some(),
        "session/load from another cwd must be rejected, got: {cross_resp}"
    );
}

/// Regression: a long history must be compacted before the request is sent,
/// not rejected by the API client's context-window preflight.
///
/// Before the fix the ACP pre-send check only compacted above 85% of the
/// context window, while the preflight rejects at
/// `history + system + tools + max_tokens > window` — roughly 65% for a
/// 200K model with 64K `max_tokens`. A history in that gap failed on every
/// prompt with `context_window_blocked`, the failed prompt was persisted
/// (growing the history), and ACP has no `/compact`, so the session was
/// wedged for good. This seeds a transcript inside that gap and asserts the
/// next prompt is answered from a compacted request.
#[tokio::test]
async fn acp_stdio_long_history_is_compacted_instead_of_rejected() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-long-history");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    // --- Process A: create the session and run one turn so the transcript exists.
    let session_id = {
        let mut client = spawn_stdio_client(&workspace);
        scenario_initialize(&mut client).await;
        let session_id = scenario_session_new(&mut client, &workspace.root).await;
        let (_notifs, resp) = client
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }]
                }),
            )
            .await;
        assert!(
            resp["result"].get("stopReason").is_some(),
            "seed turn should complete: {resp}"
        );
        client.shutdown().await;
        session_id
    };

    // --- Rewrite the transcript with a history that fits the 85% rule but
    // not the preflight: ~150K estimated tokens (bytes/4) of plain text
    // against a 200K window with 64K max_tokens and ~7K of system+tools.
    let transcript_path = find_session_transcript(&workspace.root, &session_id);
    let mut session =
        runtime::Session::load_from_path(&transcript_path).expect("seed transcript should load");
    session = session.with_messages(Vec::new());
    let filler_line = "The conversation went on about the migration plan for a long while. ";
    let filler = filler_line.repeat(30_000 / filler_line.len()); // ~30 KB per message
    for turn in 0..10 {
        session
            .push_user_text(format!("user turn {turn}: {filler}"))
            .expect("seed user message");
        session
            .push_message(runtime::ConversationMessage::assistant(vec![
                runtime::ContentBlock::Text {
                    text: format!("assistant turn {turn}: {filler}"),
                },
            ]))
            .expect("seed assistant message");
    }
    session
        .save_to_path(&transcript_path)
        .expect("seeded transcript should persist");
    let seeded_bytes = fs::metadata(&transcript_path)
        .expect("seeded transcript metadata")
        .len();
    assert!(
        seeded_bytes > 550_000,
        "seed should be well past the preflight budget, got {seeded_bytes} bytes"
    );
    // Release the fixture writer before Process B resumes and compacts.
    drop(session);

    // --- Process B: load the session and prompt once.
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let (_load_notifs, load_resp) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": workspace.root.to_string_lossy().to_string(),
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_resp.get("error").is_none(),
        "session/load should succeed, got: {load_resp}"
    );

    let before = server.captured_requests().await.len();
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}streaming_text after long history")
                }]
            }),
        )
        .await;
    assert!(
        resp.get("error").is_none() && resp["result"].get("stopReason").is_some(),
        "prompt on a long history must be answered, not rejected: {resp}"
    );

    assert_compaction_lifecycle(&notifs, "completed");

    let requests = server.captured_requests().await;
    let new_requests = &requests[before..];
    assert!(
        new_requests
            .iter()
            .any(|request| request.scenario == "llm_compaction_roundtrip"),
        "history should have been compacted through the LLM path; scenarios seen: {:?}",
        new_requests
            .iter()
            .map(|request| request.scenario.as_str())
            .collect::<Vec<_>>()
    );
    let turn_request = new_requests
        .iter()
        .rev()
        .find(|request| request.stream)
        .expect("the answered turn should have reached the mock as a streaming request");
    assert!(
        turn_request
            .raw_body
            .contains("continued from a previous conversation"),
        "turn request should carry the compaction summary; body head: {}",
        &turn_request.raw_body[..turn_request.raw_body.len().min(400)]
    );
    assert!(
        turn_request.raw_body.len() < 250_000,
        "turn request should be far below the seeded history ({seeded_bytes} bytes), got {} bytes",
        turn_request.raw_body.len()
    );

    // The compacted transcript is what a later resume sees.
    let reloaded =
        runtime::Session::load_from_path(&transcript_path).expect("compacted transcript loads");
    assert!(
        reloaded.compaction.is_some() && reloaded.messages().len() < 20,
        "transcript on disk should be compacted: compaction={:?} messages={}",
        reloaded
            .compaction
            .as_ref()
            .map(|c| c.removed_message_count),
        reloaded.messages().len()
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Create a session, run one seed turn so its transcript exists, then
/// replace the transcript with `pairs` small user/assistant exchanges of
/// plain text (no parity markers, so a compaction request over them falls
/// through to the mock's `llm_compaction_roundtrip` detection). Small enough
/// that the ACP pre-send budget check never compacts on its own; what the
/// history provides is enough compactable messages (more than the four
/// preserved verbatim) for an in-turn compaction to have something to remove.
async fn seed_filler_history(
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
    pairs: usize,
) -> (String, PathBuf) {
    let session_id = {
        let mut client = spawn_stdio_client(workspace);
        scenario_initialize(&mut client).await;
        let session_id = scenario_session_new(&mut client, &workspace.root).await;
        let (_notifs, resp) = client
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }]
                }),
            )
            .await;
        assert!(
            resp["result"].get("stopReason").is_some(),
            "seed turn should complete: {resp}"
        );
        client.shutdown().await;
        session_id
    };
    // The seed turn's requests are not part of what the test inspects.
    let _ = server.captured_requests().await;

    let transcript_path = find_session_transcript(&workspace.root, &session_id);
    let mut session =
        runtime::Session::load_from_path(&transcript_path).expect("seed transcript should load");
    session = session.with_messages(Vec::new());
    for turn in 0..pairs {
        session
            .push_user_text(format!(
                "user turn {turn}: earlier discussion about the migration plan."
            ))
            .expect("seed user message");
        session
            .push_message(runtime::ConversationMessage::assistant(vec![
                runtime::ContentBlock::Text {
                    text: format!("assistant turn {turn}: acknowledged, continuing the plan."),
                },
            ]))
            .expect("seed assistant message");
    }
    session
        .save_to_path(&transcript_path)
        .expect("seeded transcript should persist");
    (session_id, transcript_path)
}

/// Regression: the overflow error a long Anthropic session actually gets is
/// `400 input length and max_tokens exceed context limit` — raised as soon
/// as `input + max_tokens` passes the window, long before the input alone
/// does ("prompt is too long"). That text matched none of the API client's
/// context-window markers — and even once it did, the API client threw the
/// classification away when it built the `RuntimeError`. Either gap alone
/// left the runtime unable to recognise the rejection: no compaction, no
/// retry, the rejected prompt still in the transcript, and ACP surfacing the
/// raw provider text. The mock rejects until the history carries a
/// compaction summary; the turn must be answered from a compacted request.
#[tokio::test]
async fn acp_stdio_context_limit_rejection_is_compacted_and_retried() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-context-limit");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    // Leave room for the mock's fixed complete checkpoint while keeping local
    // history far below preflight: the provider rejection must trigger compaction.
    let (session_id, transcript_path) = seed_filler_history(&server, &workspace, 64).await;

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let (_load_notifs, load_resp) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": workspace.root.to_string_lossy().to_string(),
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_resp.get("error").is_none(),
        "session/load should succeed, got: {load_resp}"
    );

    let before = server.captured_requests().await.len();
    let (text, _tools) = run_scenario_collect_text(
        &mut client,
        &session_id,
        "context_limit_then_text",
        Duration::from_secs(30),
        "prompt rejected with `exceed context limit` must be compacted and retried",
    )
    .await;
    assert!(
        text.contains("context limit recovery complete."),
        "turn should be answered after compaction, got: {text:?}"
    );

    let requests = server.captured_requests().await;
    let scenarios = requests[before..]
        .iter()
        .map(|request| request.scenario.as_str())
        .collect::<Vec<_>>();
    let rejected = requests[before..]
        .iter()
        .position(|request| {
            request.stream
                && request.scenario == "context_limit_then_text"
                && !request
                    .raw_body
                    .contains("continued from a previous conversation")
        })
        .unwrap_or_else(|| {
            panic!("the first attempt should have been rejected; saw {scenarios:?}")
        });
    let compacted = requests[before..]
        .iter()
        .position(|request| request.scenario == "llm_compaction_roundtrip")
        .unwrap_or_else(|| {
            panic!("the rejection should trigger LLM compaction; saw {scenarios:?}")
        });
    let answered = requests[before..]
        .iter()
        .rposition(|request| {
            request.stream
                && request.scenario == "context_limit_then_text"
                && request
                    .raw_body
                    .contains("continued from a previous conversation")
        })
        .unwrap_or_else(|| panic!("the retry should carry the summary; saw {scenarios:?}"));
    assert!(
        rejected < compacted && compacted < answered,
        "expected reject -> compact -> retry, saw {scenarios:?}"
    );

    let reloaded =
        runtime::Session::load_from_path(&transcript_path).expect("compacted transcript loads");
    assert!(
        reloaded.compaction.is_some(),
        "transcript on disk should record the compaction"
    );
    let report = reloaded.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::TargetMet);
    assert!(report.after_history <= report.before_history / 2);
    assert!(report.after_history <= report.safe_history_budget);
    assert!(
        reloaded
            .messages()
            .last()
            .is_some_and(|message| message.role == runtime::MessageRole::Assistant),
        "the answered turn should be persisted after the compacted history: {:?}",
        reloaded
            .messages()
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>()
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Regression: auto-compaction only ran at the end of a turn, so a tool loop
/// could grow the context from "under the budget" straight into a provider
/// rejection with no compaction in between.
///
/// This is also the case the in-turn guard's *local estimate* cannot see:
/// the tool result here is a one-line file, so the estimate stays tiny while
/// the provider reports 150K of context. Only the reported-context arm of
/// the budget notices, which is why the guard consults both. The runtime
/// must compact before building the next request of the same turn, and the
/// follow-up request must carry the summary alongside the tool result.
#[tokio::test]
async fn acp_stdio_tool_loop_compacts_before_next_request() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-tool-loop-growth");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    fs::write(
        workspace.root.join("fixture.txt"),
        "tool loop fixture line\n",
    )
    .expect("fixture should be written");
    // The fixed mock checkpoint must fit, but local history stays small so
    // provider-reported context growth triggers this in-turn compaction.
    let (session_id, transcript_path) = seed_filler_history(&server, &workspace, 64).await;

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let (_load_notifs, load_resp) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": workspace.root.to_string_lossy().to_string(),
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load_resp.get("error").is_none(),
        "session/load should succeed, got: {load_resp}"
    );

    let before = server.captured_requests().await.len();
    let (text, tools) = run_scenario_collect_text(
        &mut client,
        &session_id,
        "tool_loop_context_growth",
        Duration::from_secs(30),
        "tool loop whose context passes the threshold mid-turn",
    )
    .await;
    assert!(
        text.contains("tool loop compacted before the next request."),
        "the request after the tool result should carry a compaction summary; got: {text:?} \
         (tool outputs: {tools:?})"
    );

    let requests = server.captured_requests().await;
    let scenarios = requests[before..]
        .iter()
        .map(|request| request.scenario.as_str())
        .collect::<Vec<_>>();
    let tool_use = requests[before..]
        .iter()
        .position(|request| {
            request.stream
                && request.scenario == "tool_loop_context_growth"
                && !request.raw_body.contains("tool_result")
        })
        .unwrap_or_else(|| panic!("the first request should ask for the tool; saw {scenarios:?}"));
    let compacted = requests[before..]
        .iter()
        .position(|request| request.scenario == "llm_compaction_roundtrip")
        .unwrap_or_else(|| {
            panic!("the reported context should trigger LLM compaction; saw {scenarios:?}")
        });
    let follow_up = requests[before..]
        .iter()
        .rposition(|request| {
            request.stream
                && request.scenario == "tool_loop_context_growth"
                && request.raw_body.contains("tool_result")
        })
        .unwrap_or_else(|| panic!("the tool result should be sent back; saw {scenarios:?}"));
    assert!(
        tool_use < compacted && compacted < follow_up,
        "expected tool use -> compact -> follow-up, saw {scenarios:?}"
    );
    assert!(
        requests[before + follow_up]
            .raw_body
            .contains("continued from a previous conversation"),
        "follow-up request should carry the compaction summary; body head: {}",
        &requests[before + follow_up].raw_body
            [..requests[before + follow_up].raw_body.len().min(400)]
    );

    let reloaded = runtime::Session::load_from_path(&transcript_path).unwrap();
    let report = reloaded.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::TargetMet);
    assert!(report.after_history <= report.before_history / 2);
    assert!(report.after_history <= report.safe_history_budget);

    client.shutdown().await;
    workspace.cleanup();
}

/// Locate `<root>/.scode/sessions/<fingerprint>/<id>/transcript.jsonl`.
fn find_session_transcript(root: &std::path::Path, session_id: &str) -> PathBuf {
    let sessions = root.join(".scode").join("sessions");
    let entries = fs::read_dir(&sessions).expect("sessions dir should exist");
    for entry in entries {
        let candidate = entry
            .expect("sessions dir entry")
            .path()
            .join(session_id)
            .join("transcript.jsonl");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no transcript for {session_id} under {}",
        sessions.display()
    );
}

// ---------------------------------------------------------------------------
// Cross-session concurrency
// ---------------------------------------------------------------------------

/// True when `msg` is the JSON-RPC *response* to client request `id` (a
/// server-originated request such as `session/request_permission` also
/// carries an `id`, so the absence of `method` is what distinguishes them).
fn is_response_to(msg: &Value, id: u64) -> bool {
    msg.get("id").and_then(Value::as_u64) == Some(id) && msg.get("method").is_none()
}

fn is_server_request(msg: &Value, method: &str) -> bool {
    msg.get("method").and_then(Value::as_str) == Some(method) && msg.get("id").is_some()
}

/// Send a `session/prompt` on `session_id` that makes the mock model call
/// `bash`, and — because the session runs in `workspace-write` — parks the
/// turn on a `session/request_permission` round-trip that we deliberately do
/// NOT answer. Returns `(prompt request id, permission request id)`.
async fn park_session_on_permission_prompt(
    client: &mut AcpTestClient,
    session_id: &str,
) -> (u64, Value) {
    let (_, resp) = client
        .send_request(
            "session/setPermissionMode",
            json!({ "sessionId": session_id, "permissionMode": "workspace-write" }),
        )
        .await;
    assert!(
        resp.get("error").is_none(),
        "session/setPermissionMode should succeed: {resp}"
    );

    let prompt_id = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}bash_permission_prompt_approved")
                }]
            }),
        )
        .await;

    let (_, perm_req) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "session/request_permission")
        })
        .await
        .unwrap_or_else(|seen| {
            panic!("expected session/request_permission from the agent; saw: {seen:?}")
        });
    assert_eq!(
        perm_req["params"]["sessionId"].as_str(),
        Some(session_id),
        "permission request must be attributed to the parked session"
    );
    (prompt_id, perm_req["id"].clone())
}

/// Answer a pending `session/request_permission` with `allow_once`.
async fn allow_permission(client: &mut AcpTestClient, perm_req_id: &Value) {
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "id": perm_req_id,
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } }
        }))
        .await;
}

/// Regression guard for the multi-session P0: while session A is parked on
/// a permission prompt (waiting for the user), a `session/prompt` on an
/// unrelated session B must still be answered. Before the fix every prompt
/// took one process-wide delegate mutex for the whole turn, so A's pause
/// blocked B (and every other session) indefinitely.
///
/// `other_cwd` picks whether B lives in the same working directory as A or
/// in a sibling directory; both must work.
async fn scenario_paused_session_does_not_block_other_session(label: &str, other_cwd: bool) {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new(label);
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let cwd_b = if other_cwd {
        let dir = workspace.root.join("project-b");
        fs::create_dir_all(&dir).expect("create sibling cwd");
        dir
    } else {
        workspace.root.clone()
    };

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session_a = scenario_session_new(&mut client, &workspace.root).await;

    let (prompt_a, perm_req_id) = park_session_on_permission_prompt(&mut client, &session_a).await;

    // Mirrors the reported flow: the user opens a NEW conversation while the
    // first one is waiting on them. session/new itself must not block either.
    let new_b = client
        .send_request_no_wait(
            "session/new",
            json!({ "cwd": cwd_b.to_string_lossy().to_string(), "mcpServers": [] }),
        )
        .await;
    let (_, new_b_resp) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, new_b))
        .await
        .unwrap_or_else(|seen| {
            panic!(
                "session/new got no response within 20s while session A was parked on a \
                 permission prompt (cross-session blocking); messages seen: {seen:?}"
            )
        });
    let session_b = new_b_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new should succeed: {new_b_resp}"))
        .to_string();

    // Session B must make progress while A is parked.
    let prompt_b = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_b,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }]
            }),
        )
        .await;
    let (before_b, resp_b) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, prompt_b))
        .await
        .unwrap_or_else(|seen| {
            panic!(
                "session B's prompt got no response within 20s while session A was parked on a \
                 permission prompt (cross-session blocking); messages seen: {seen:?}"
            )
        });
    assert!(
        resp_b["result"].get("stopReason").is_some(),
        "session B's turn should complete normally: {resp_b}"
    );
    assert!(
        !before_b.iter().any(|m| is_response_to(m, prompt_a)),
        "session A must still be parked (its prompt must not have completed yet)"
    );
    // B's streamed updates must be attributed to B only.
    for m in &before_b {
        if m.get("method").and_then(Value::as_str) == Some("session/update") {
            assert_eq!(
                m["params"]["sessionId"].as_str(),
                Some(session_b.as_str()),
                "unexpected session/update for another session while B ran: {m}"
            );
        }
    }

    // Now release A: it must finish its turn normally (the parked session is
    // not lost or corrupted by B having run in the meantime).
    allow_permission(&mut client, &perm_req_id).await;
    let (notifs_a, resp_a) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, prompt_a))
        .await
        .unwrap_or_else(|seen| panic!("session A never completed after approval; saw: {seen:?}"));
    assert!(
        resp_a["result"].get("stopReason").is_some(),
        "session A's turn should complete after the permission is granted: {resp_a}"
    );
    let blob = serde_json::to_string(&notifs_a).unwrap_or_default();
    assert!(
        blob.contains("bash approved and executed"),
        "session A should have run the approved bash call and streamed the mock's final text; \
         got: {blob}"
    );

    // Both sessions must still be usable afterwards.
    scenario_session_prompt_streaming(&mut client, &session_a).await;
    scenario_session_prompt_streaming(&mut client, &session_b).await;

    client.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn acp_stdio_paused_session_does_not_block_other_session_same_cwd() {
    scenario_paused_session_does_not_block_other_session("stdio-concurrency-same-cwd", false).await;
}

#[tokio::test]
async fn acp_stdio_paused_session_does_not_block_other_session_other_cwd() {
    scenario_paused_session_does_not_block_other_session("stdio-concurrency-other-cwd", true).await;
}

/// A session running a LONG TOOL (not parked on the user) must not starve a
/// sibling session in a different cwd. This is the half of the P0 that the
/// cwd lease does not cover: the process cwd is a shared resource, so an
/// active turn in /a holds it for the whole turn — including a 30s bash —
/// and a turn in /b waits. Reported symptom: one conversation writes a long
/// report / installs deps, every other conversation of that user goes quiet.
#[tokio::test]
async fn acp_stdio_long_tool_does_not_block_other_cwd_session() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-long-tool-other-cwd");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let cwd_b = workspace.root.join("project-b");
    fs::create_dir_all(&cwd_b).expect("create sibling cwd");

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session_a = scenario_session_new(&mut client, &workspace.root).await;

    // `allow` so the bash tool executes instead of prompting — A must be BUSY,
    // not parked (a parked turn yields the cwd lease, which is the half that
    // is already fixed).
    let (_, mode_resp) = client
        .send_request(
            "session/setPermissionMode",
            json!({ "sessionId": session_a, "permissionMode": "allow" }),
        )
        .await;
    assert!(
        mode_resp.get("error").is_none(),
        "session/setPermissionMode allow should succeed: {mode_resp}"
    );

    // A: a turn whose tool call runs `sleep 30` — busy, never parked.
    let _prompt_a = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_a,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}bash_interrupt_long_running")
                }]
            }),
        )
        .await;

    // Give A time to reach the tool call and take the cwd lease.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // B, in a different cwd, must still be served while A is busy.
    let new_b = client
        .send_request_no_wait(
            "session/new",
            json!({ "cwd": cwd_b.to_string_lossy().to_string(), "mcpServers": [] }),
        )
        .await;
    let (_, new_b_resp) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, new_b))
        .await
        .unwrap_or_else(|seen| {
            panic!(
                "session/new got no response within 20s while session A was running a long \
                 tool in another cwd (cwd lease held for the whole turn); seen: {seen:?}"
            )
        });
    let session_b = new_b_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new should succeed: {new_b_resp}"))
        .to_string();

    let prompt_b = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_b,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}single_turn_text") }]
            }),
        )
        .await;
    client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, prompt_b))
        .await
        .unwrap_or_else(|seen| {
            panic!(
                "session B starved while session A ran a long tool in another cwd; seen: {seen:?}"
            )
        });

    client.shutdown().await;
    workspace.cleanup();
}

/// Set a session's permission mode, asserting success.
async fn set_permission_mode(client: &mut AcpTestClient, session_id: &str, mode: &str) {
    let (_, resp) = client
        .send_request(
            "session/setPermissionMode",
            json!({ "sessionId": session_id, "permissionMode": mode }),
        )
        .await;
    assert!(
        resp.get("error").is_none(),
        "session/setPermissionMode {mode} should succeed: {resp}"
    );
}

/// Run `scenario` on `session_id`, wait up to `limit` for the turn to
/// complete, and return the concatenated `agent_message_chunk` text the
/// agent streamed for it plus every `tool_call_update` raw output (for
/// diagnostics: the mock's final text does not distinguish a failed tool
/// call from a successful one).
async fn run_scenario_collect_text(
    client: &mut AcpTestClient,
    session_id: &str,
    scenario: &str,
    limit: Duration,
    what: &str,
) -> (String, Vec<Value>) {
    let prompt_id = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}{scenario}") }]
            }),
        )
        .await;
    let (notifs, resp) = client
        .recv_until(limit, |m| is_response_to(m, prompt_id))
        .await
        .unwrap_or_else(|seen| panic!("{what}: no response within {limit:?}; seen: {seen:?}"));
    assert!(
        resp["result"].get("stopReason").is_some(),
        "{what}: turn should complete normally: {resp}"
    );
    if matches!(
        scenario,
        "tool_loop_context_growth" | "context_limit_then_text"
    ) {
        assert_compaction_lifecycle(&notifs, "completed");
    }
    let text = notifs
        .iter()
        .filter(|m| {
            m["params"]["sessionId"].as_str() == Some(session_id)
                && m["params"]["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk")
        })
        .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    let tool_outputs = notifs
        .iter()
        .filter(|m| {
            m["params"]["sessionId"].as_str() == Some(session_id)
                && m["params"]["update"]["sessionUpdate"].as_str() == Some("tool_call_update")
        })
        .map(|m| m["params"]["update"]["rawOutput"].clone())
        .collect::<Vec<_>>();
    (text, tool_outputs)
}

/// The invariant that makes concurrent cross-cwd turns safe: once the process
/// cwd is no longer the single source of truth, a session's file operations
/// must resolve against *its own* directory — never against the directory
/// of whichever session happened to touch the process cwd last, and never
/// against a sibling that is mid-turn.
///
/// Layout: session A in `project-a`, sessions in `project-b`, plus a decoy
/// session created LAST in `decoy` (so the process cwd, which runtime
/// construction still sets, points at the decoy). Each directory carries a
/// differently worded `fixture.txt`; the scode process itself was started in
/// the workspace root, which carries yet another one. While A is busy in a
/// 30 s `bash`, sessions in `project-b` read and write relative paths and
/// must see only `project-b`; then A is cancelled and sessions in
/// `project-a` do the same there. (One session per file scenario: the mock
/// model answers from the latest tool result in the history, so a second
/// tool scenario on the same session would never issue its tool call.)
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn acp_stdio_concurrent_sessions_keep_file_ops_in_their_own_cwd() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-cwd-isolation");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let cwd_a = workspace.root.join("project-a");
    let cwd_b = workspace.root.join("project-b");
    let cwd_decoy = workspace.root.join("decoy");
    for (dir, marker) in [
        (&workspace.root, "root-fixture"),
        (&cwd_a, "alpha-fixture"),
        (&cwd_b, "bravo-fixture"),
        (&cwd_decoy, "decoy-fixture"),
    ] {
        fs::create_dir_all(dir).expect("create cwd");
        fs::write(dir.join("fixture.txt"), format!("{marker}\n")).expect("write fixture");
    }
    let generated = |dir: &std::path::Path| dir.join("generated").join("output.txt");

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session_a = scenario_session_new(&mut client, &cwd_a).await;
    let reader_b = scenario_session_new(&mut client, &cwd_b).await;
    let writer_b = scenario_session_new(&mut client, &cwd_b).await;
    // Created last: whatever still keys off the process cwd now sees `decoy`.
    let _session_decoy = scenario_session_new(&mut client, &cwd_decoy).await;
    for sid in [&session_a, &reader_b, &writer_b] {
        set_permission_mode(&mut client, sid, "allow").await;
    }

    // A: busy in `sleep 30` for the rest of the B phase.
    let prompt_a = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_a,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}bash_interrupt_long_running")
                }]
            }),
        )
        .await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // B reads a relative path while A is busy: must be B's own fixture.
    let (text_b, tools_b) = run_scenario_collect_text(
        &mut client,
        &reader_b,
        "read_file_roundtrip",
        Duration::from_secs(20),
        "project-b read_file while A busy in another cwd",
    )
    .await;
    assert!(
        text_b.contains("bravo-fixture"),
        "project-b session must read its own fixture.txt (bravo-fixture); got: {text_b:?} \
         (tool outputs: {tools_b:?})"
    );
    for leaked in ["alpha-fixture", "decoy-fixture", "root-fixture"] {
        assert!(
            !text_b.contains(leaked),
            "project-b session read another directory's fixture ({leaked}); got: {text_b:?}"
        );
    }

    // B writes a relative path while A is busy: must land in project-b only.
    let (text_b, tools_b) = run_scenario_collect_text(
        &mut client,
        &writer_b,
        "write_file_allowed",
        Duration::from_secs(20),
        "project-b write_file while A busy in another cwd",
    )
    .await;
    assert!(
        text_b.contains("write_file succeeded"),
        "project-b write should succeed: {text_b:?} (tool outputs: {tools_b:?})"
    );
    assert!(
        generated(&cwd_b).is_file(),
        "project-b relative write must land in project-b (tool outputs: {tools_b:?})"
    );
    for (name, dir) in [
        ("project-a", &cwd_a),
        ("decoy", &cwd_decoy),
        ("workspace root", &workspace.root),
    ] {
        assert!(
            !generated(dir).exists(),
            "project-b relative write leaked into {name}"
        );
    }
    // A must still be busy — the project-b turns ran concurrently, not after A.
    let a_done_early = client
        .recv_until(Duration::from_millis(200), |m| is_response_to(m, prompt_a))
        .await;
    assert!(
        a_done_early.is_err(),
        "session A should still be running its long tool while project-b did file ops"
    );

    // Cancel A: the turn must end promptly (its `sleep 30` is interrupted).
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_a }
        }))
        .await;
    let (_, resp_a) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, prompt_a))
        .await
        .unwrap_or_else(|seen| panic!("session A never finished after cancel; seen: {seen:?}"));
    assert!(
        resp_a.get("result").is_some() || resp_a.get("error").is_some(),
        "session A's cancelled prompt should get a response: {resp_a}"
    );

    // Now project-a: its sessions must see project-a, even though project-b
    // turns ran in between and the process cwd points at the decoy.
    let reader_a = scenario_session_new(&mut client, &cwd_a).await;
    let writer_a = scenario_session_new(&mut client, &cwd_a).await;
    for sid in [&reader_a, &writer_a] {
        set_permission_mode(&mut client, sid, "allow").await;
    }
    let (text_a, tools_a) = run_scenario_collect_text(
        &mut client,
        &reader_a,
        "read_file_roundtrip",
        Duration::from_secs(20),
        "project-a read_file",
    )
    .await;
    assert!(
        text_a.contains("alpha-fixture"),
        "project-a session must read its own fixture.txt (alpha-fixture); got: {text_a:?} \
         (tool outputs: {tools_a:?})"
    );
    let (text_a, tools_a) = run_scenario_collect_text(
        &mut client,
        &writer_a,
        "write_file_allowed",
        Duration::from_secs(20),
        "project-a write_file",
    )
    .await;
    assert!(
        text_a.contains("write_file succeeded"),
        "project-a write should succeed: {text_a:?} (tool outputs: {tools_a:?})"
    );
    assert!(
        generated(&cwd_a).is_file(),
        "project-a relative write must land in project-a (tool outputs: {tools_a:?})"
    );
    assert!(
        !generated(&cwd_decoy).exists() && !generated(&workspace.root).exists(),
        "project-a relative write leaked outside project-a"
    );

    // The cancelled session and a project-b session must still be usable.
    scenario_session_prompt_streaming(&mut client, &session_a).await;
    scenario_session_prompt_streaming(&mut client, &reader_b).await;

    client.shutdown().await;
    workspace.cleanup();
}

/// Same P0, other wait path: session A parked inside the `AskUserQuestion`
/// tool (`_scode/ask_user_question` extension request to the client, left
/// unanswered — "waiting for the user to answer a question" is exactly the
/// reported symptom), session B must still be served.
#[tokio::test]
async fn acp_stdio_session_parked_on_ask_user_question_does_not_block_other_session() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-concurrency-ask-user");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session_a = scenario_session_new(&mut client, &workspace.root).await;

    let prompt_a = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_a,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}ask_user_question_roundtrip")
                }]
            }),
        )
        .await;
    let (_, question_req) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "_scode/ask_user_question")
        })
        .await
        .unwrap_or_else(|seen| {
            panic!("expected _scode/ask_user_question from the agent; saw: {seen:?}")
        });
    assert_eq!(
        question_req["params"]["sessionId"].as_str(),
        Some(session_a.as_str())
    );

    // New conversation while A waits on the user: must be served.
    let new_b = client
        .send_request_no_wait(
            "session/new",
            json!({ "cwd": workspace.root.to_string_lossy().to_string(), "mcpServers": [] }),
        )
        .await;
    let (_, new_b_resp) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, new_b))
        .await
        .unwrap_or_else(|seen| {
            panic!("session/new blocked behind an unanswered AskUserQuestion; saw: {seen:?}")
        });
    let session_b = new_b_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new should succeed: {new_b_resp}"))
        .to_string();
    let prompt_b = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_b,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }]
            }),
        )
        .await;
    let (_, resp_b) = client
        .recv_until(Duration::from_secs(20), |m| is_response_to(m, prompt_b))
        .await
        .unwrap_or_else(|seen| {
            panic!("session B blocked behind an unanswered AskUserQuestion; saw: {seen:?}")
        });
    assert!(resp_b["result"].get("stopReason").is_some(), "{resp_b}");

    // Answer A's question; A must complete with the answer visible to the model.
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "id": question_req["id"],
            "result": { "answers": [{ "id": "q1", "value": "blue", "label": "blue" }] }
        }))
        .await;
    let (notifs_a, resp_a) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, prompt_a))
        .await
        .unwrap_or_else(|seen| panic!("session A never completed after the answer; saw: {seen:?}"));
    assert!(resp_a["result"].get("stopReason").is_some(), "{resp_a}");
    let blob = serde_json::to_string(&notifs_a).unwrap_or_default();
    assert!(
        blob.contains("ask_user_question answered") && blob.contains("blue"),
        "session A should have resumed with the user's answer; got: {blob}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// The flip side of cross-session concurrency: prompts on the SAME session
/// must stay strictly serial. A second `session/prompt` sent while the first
/// is parked on a permission prompt must not reach the model or complete
/// until the first turn has finished. Tool progress from the parked turn
/// may still arrive after its permission request.
#[tokio::test]
async fn acp_stdio_same_session_prompts_stay_serial() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-same-session-serial");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session_a = scenario_session_new(&mut client, &workspace.root).await;

    let (prompt_1, perm_req_id) = park_session_on_permission_prompt(&mut client, &session_a).await;
    let requests_before = server.captured_requests().await.len();

    let prompt_2 = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_a,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }]
            }),
        )
        .await;

    // A queued tool_call notification belongs to turn 1; it does not prove
    // that turn 2 started. Observe both prompt responses and actual upstream
    // requests instead of rejecting any session/update on the shared stream.
    let leaked = client
        .recv_until(Duration::from_secs(3), |m| {
            is_response_to(m, prompt_1) || is_response_to(m, prompt_2)
        })
        .await;
    assert!(
        leaked.is_err(),
        "second prompt on the same session must not run while the first is parked; got: {:?}",
        leaked.ok().map(|(_, m)| m)
    );
    assert_eq!(
        server.captured_requests().await.len(),
        requests_before,
        "a second prompt must not reach the model while turn 1 awaits permission"
    );

    allow_permission(&mut client, &perm_req_id).await;
    let (_, resp_1) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, prompt_1))
        .await
        .unwrap_or_else(|seen| panic!("first turn never completed; saw: {seen:?}"));
    assert!(resp_1["result"].get("stopReason").is_some(), "{resp_1}");
    let (_, resp_2) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, prompt_2))
        .await
        .unwrap_or_else(|seen| panic!("second turn never completed; saw: {seen:?}"));
    assert!(resp_2["result"].get("stopReason").is_some(), "{resp_2}");
    assert_eq!(
        server
            .captured_requests()
            .await
            .last()
            .map(|r| r.scenario.as_str()),
        Some("streaming_text"),
        "the queued second prompt must run after the first turn completes"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// End-to-end regression guard for the wrong-model VLM route. Verifies the
/// entire VLM call chain including the sudorouter round-trip:
///
///  1. Text-only fixture model is active.
///  2. Sudorouter creds in sudocode.json point at OpenAiCompatMock instead
///     of the real hk.sudorouter.ai.
///  3. push_images sees vision_capable=false → VLM branch → HTTP POST to
///     the mock's `/chat/completions` with the image bytes inline.
///  4. Mock returns a canned description ("MOCK_VLM_DESCRIPTION").
///  5. push_images splices `[Image #1: MOCK_VLM_DESCRIPTION]` into the
///     prompt as ContentBlock::Text and pushes to the session.
///
/// Assertions:
///  - mock.captured_requests() has ≥1 entry with method=POST, path containing
///    `chat/completions`, body containing an `image_url` content part + the
///    DEFAULT_VISION_MODEL (gemini-2.5-flash) + Bearer auth header.
///  - session/prompt returns a stopReason (didn't hang) — proves both the
///    VLM leg and the subsequent MockAnthropicService leg completed.
///  - stderr shows `[push_images] VLM-route start` + `VLM done` eprintlns.
///
/// This catches ALL three regression classes in one test:
///  a) endpoint catalog not loaded/scoped in ACP (vision defaults to true,
///     push_images takes the native branch, and the VLM mock gets no request).
///  b) VLM-route wire-format regressions (wrong endpoint path, wrong content
///     shape, missing Authorization header, wrong model name).
///  c) block_in_place / runtime nesting regressions (would hang the call
///     past the RECV_TIMEOUT and fail with a clear panic).
///
/// **Choice of mock**: `OpenAiCompatMock` stands in for sudorouter's
/// `/v1/chat/completions`, `MockAnthropicService` stands in for the LLM
/// provider that scode's own turn will call. Both are localhost so the
/// timing is fast (no network hops); a real-network variant was tried
/// (pointing at hk.sudorouter.ai) but hung 90+ s on CI's isolated network.
#[tokio::test]
async fn acp_wrong_model_vlm_full_roundtrip() {
    // `sonnet` = CLI alias other tests use (safe pass-through to mock).
    // `claude-sonnet-4-6` = the WIRE model name scode resolves the alias
    // to, and what push_images actually calls vision_capable() with — the
    // cache seed MUST use the wire name.
    const TEST_MODEL: &str = "sonnet";
    const WIRE_MODEL: &str = "claude-sonnet-4-6";
    const MOCK_DESCRIPTION: &str = "MOCK_VLM_DESCRIPTION_a1b2c3";

    let anthropic_mock = MockAnthropicService::spawn()
        .await
        .expect("anthropic mock should start");
    let sudorouter_mock = OpenAiCompatMock::spawn(MOCK_DESCRIPTION)
        .await
        .expect("sudorouter mock should start");
    let workspace = TestWorkspace::new("vlm-full-roundtrip");
    workspace.create();
    workspace.write_sudocode_json_with_sudorouter(
        &anthropic_mock.base_url(),
        sudorouter_mock.base_url(),
    );
    anthropic_mock
        .set_model_catalog(json!({
            "data": [{
                "id": WIRE_MODEL,
                "context_window": 131072,
                "max_output_tokens": 64000,
                "vision_supported": false,
            }],
            "has_more": false,
        }))
        .await;
    workspace
        .seed_model_catalog(&anthropic_mock.base_url())
        .await;

    let mut cmd = isolated_command(&workspace, Path::new(env!("CARGO_BIN_EXE_scode")));
    cmd.args([
        "--auth",
        "api-key",
        "--model",
        TEST_MODEL,
        "--permission-mode",
        "read-only",
        "acp",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn scode acp");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    let stderr_captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let stderr_captured_bg = std::sync::Arc::clone(&stderr_captured);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    stderr_captured_bg
                        .lock()
                        .await
                        .push(line.trim_end().to_string());
                }
            }
        }
    });

    let mut client = AcpTestClient {
        transport: Transport::Stdio {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        },
        next_id: 1,
        last_available_commands: None,
        raw_lines: Vec::new(),
    };

    // 1×1 transparent PNG.
    const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=";

    let (_notifs, init_resp) = client
        .send_request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    assert!(init_resp["result"].get("protocolVersion").is_some());

    let (_notifs, new_resp) = client
        .send_request(
            "session/new",
            json!({ "cwd": workspace.root.to_string_lossy(), "mcpServers": [] }),
        )
        .await;
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new failed or returned no sessionId: {new_resp}"))
        .to_string();

    // Send prompt with inline image; text-only fixture model → VLM route
    // fires → hits sudorouter_mock → response splice → passes to anthropic mock.
    let (_notifs, prompt_resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    { "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" },
                    { "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }
                ]
            }),
        )
        .await;

    let result = &prompt_resp["result"];
    assert!(
        result.get("stopReason").is_some(),
        "session/prompt should complete with a stopReason — full VLM roundtrip + LLM leg both had to succeed. Got: {prompt_resp}"
    );

    // Assert the VLM mock actually got hit.
    let vlm_requests = sudorouter_mock.captured_requests().await;
    // On failure, dump captured stderr so CI logs show what push_images
    // actually did (native branch / VLM branch / crash). Silent panics
    // without this context cost 40+ min of blind debugging on 2026-07-01.
    let captured_now = stderr_captured.lock().await.clone();
    assert!(
        !vlm_requests.is_empty(),
        "OpenAiCompatMock (standing in for sudorouter) must have received at least one request from push_images's VLM route. \
         If empty, either vision_capable(sonnet) returned true (SSOT cache didn't populate — regression of the load() bug), \
         or push_images silently skipped the VLM branch (regression of the branch logic in main.rs). \
         scode stderr snapshot: {captured_now:#?}"
    );

    let vlm_req = &vlm_requests[0];
    assert_eq!(
        vlm_req.method, "POST",
        "VLM request must be POST /chat/completions"
    );
    assert!(
        vlm_req.path.contains("chat/completions"),
        "VLM request must target /chat/completions endpoint, got: {}",
        vlm_req.path
    );
    assert!(
        vlm_req
            .authorization
            .as_deref()
            .unwrap_or("")
            .starts_with("Bearer "),
        "VLM request must carry Bearer auth. Got: {:?}",
        vlm_req.authorization
    );
    assert!(
        vlm_req.raw_body.contains("image_url"),
        "VLM request body must contain image_url content part (OpenAI-compat shape). Got body head: {}",
        &vlm_req.raw_body[..vlm_req.raw_body.len().min(500)]
    );
    assert!(
        vlm_req.raw_body.contains("gemini-2.5-flash"),
        "VLM request must use DEFAULT_VISION_MODEL gemini-2.5-flash. Got: {}",
        &vlm_req.raw_body[..vlm_req.raw_body.len().min(500)]
    );

    // Optional sanity: stderr should have logged both entries.
    let final_lines = stderr_captured.lock().await.clone();
    let saw_vlm_start = final_lines.iter().any(|l| l.contains("VLM-route start"));
    let saw_vlm_done = final_lines.iter().any(|l| l.contains("VLM done"));
    assert!(
        saw_vlm_start && saw_vlm_done,
        "expected [push_images] VLM-route start + VLM done lines in stderr, got: {final_lines:#?}"
    );

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
    run_all_scenarios(&mut client, &server, &workspace).await;
    client.shutdown().await;
    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// session/new mcp_servers injection (per-session stdio MCP)
// ---------------------------------------------------------------------------

/// Minimal NDJSON MCP server (python3). Writes a proof file on startup
/// (proving spawn + env passthrough), and exposes an `echo` tool whose
/// `tools/call` result shape is what `mcp_echo_verdict` extracts. scode's
/// MCP client performs initialize → tools/list → tools/call and sends no
/// notifications, so those three methods are all this server handles.
const MCP_DUMMY_SCRIPT: &str = r#"import json, os, sys

proof = os.environ.get("DUMMY_PROOF")
if proof:
    with open(proof, "w") as f:
        f.write(os.environ.get("DUMMY_TOKEN", ""))

def read_msg():
    line = sys.stdin.buffer.readline()
    return None if not line else json.loads(line.decode())

def send_msg(m):
    sys.stdout.buffer.write(json.dumps(m).encode() + b"\n")
    sys.stdout.buffer.flush()

while True:
    req = read_msg()
    if req is None:
        break
    method = req.get("method")
    if method == "initialize":
        send_msg({"jsonrpc": "2.0", "id": req["id"], "result": {
            "protocolVersion": req["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "parity-mcp", "version": "0.1.0"}}})
    elif method == "tools/list":
        send_msg({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{
            "name": "echo",
            "inputSchema": {"type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]}}]}})
    elif method == "tools/call":
        args = req["params"].get("arguments") or {}
        text = args.get("text", "")
        send_msg({"jsonrpc": "2.0", "id": req["id"], "result": {
            "content": [{"type": "text", "text": f"echo:{text}"}],
            "isError": False}})
    elif "id" in req:
        send_msg({"jsonrpc": "2.0", "id": req["id"],
            "error": {"code": -32601, "message": f"unknown method: {method}"}})
"#;

/// Like `spawn_stdio_client` but with `--permission-mode danger-full-access`
/// so MCP tool calls are not gated behind an interactive permission prompt.
fn spawn_stdio_client_danger(workspace: &TestWorkspace) -> AcpTestClient {
    spawn_stdio_client_danger_binary(workspace, Path::new(env!("CARGO_BIN_EXE_scode")))
}

fn spawn_stdio_client_danger_binary(workspace: &TestWorkspace, binary: &Path) -> AcpTestClient {
    let mut cmd = isolated_command(workspace, binary);
    cmd.args([
        "--auth",
        "api-key",
        "--model",
        "sonnet",
        "--permission-mode",
        "danger-full-access",
        "acp",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn scode acp stdio (danger)");
    let stdin = child.stdin.take().expect("stdin should be piped");
    let stdout = child.stdout.take().expect("stdout should be piped");

    AcpTestClient {
        transport: Transport::Stdio {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        },
        next_id: 1,
        last_available_commands: None,
        raw_lines: Vec::new(),
    }
}

/// Like [`spawn_stdio_client_danger`] but also enables a global
/// `--allowedTools` allow-list, to verify per-session injected MCP tools stay
/// available under an allow-list (they are added to it at runtime build time).
fn spawn_stdio_client_danger_with_allowed(
    workspace: &TestWorkspace,
    allowed: &str,
) -> AcpTestClient {
    let mut cmd = isolated_command(workspace, Path::new(env!("CARGO_BIN_EXE_scode")));
    cmd.args([
        "--auth",
        "api-key",
        "--model",
        "sonnet",
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        allowed,
        "acp",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn scode acp stdio (danger+allowed)");
    let stdin = child.stdin.take().expect("stdin should be piped");
    let stdout = child.stdout.take().expect("stdout should be piped");

    AcpTestClient {
        transport: Transport::Stdio {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        },
        next_id: 1,
        last_available_commands: None,
        raw_lines: Vec::new(),
    }
}

/// A model switch rebuilds the session runtime. Cancellation must keep using
/// the same abort signal registered by `session/new`, otherwise the rebuilt
/// runtime waits on a fresh signal and the prompt runs to its natural end.
#[tokio::test]
async fn acp_session_cancel_survives_model_switch() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("cancel-after-model-switch");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;

    // The process starts on sonnet. A different model is required to enter
    // handle_acp_model_switch's runtime-rebuild branch.
    let (_, set_resp) = client
        .send_request(
            "session/set_model",
            json!({ "sessionId": session_id, "modelId": "haiku" }),
        )
        .await;
    assert!(
        set_resp.get("error").is_none(),
        "session/setModel should succeed: {set_resp}"
    );

    let prompt_id = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}bash_interrupt_long_running")
                }]
            }),
        )
        .await;

    // Wait until the long-running tool has been announced so cancellation is
    // exercised mid-turn rather than before prompt dispatch.
    client
        .recv_until(Duration::from_secs(20), |message| {
            message["method"].as_str() == Some("session/update")
                && message["params"]["sessionId"].as_str() == Some(session_id.as_str())
                && message["params"]["update"]["sessionUpdate"].as_str() == Some("tool_call")
                && message["params"]["update"]["toolCallId"].as_str()
                    == Some("toolu_bash_interrupt")
        })
        .await
        .unwrap_or_else(|seen| panic!("long-running tool never started; saw: {seen:?}"));

    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id }
        }))
        .await;

    // The point is that the cancel lands at all, not that it lands inside two
    // seconds: under a parallel `cargo test --workspace` this box is running a
    // crowd of other `scode` children, and the turn needs room to unwind.
    const CANCEL_WINDOW: Duration = Duration::from_secs(20);
    let (_, prompt_resp) = client
        .recv_until(CANCEL_WINDOW, |message| is_response_to(message, prompt_id))
        .await
        .unwrap_or_else(|seen| {
            panic!(
                "prompt did not stop within {CANCEL_WINDOW:?} after cancel following model switch; saw: {seen:?}"
            )
        });
    assert!(
        prompt_resp.get("result").is_some() || prompt_resp.get("error").is_some(),
        "cancelled prompt should receive a response: {prompt_resp}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Deterministic subagent-over-ACP coverage. This is the mock-backed
/// counterpart to the live `live_subagent_smoke_stdio` test: it pins the
/// exact ACP wire contract for a parent that delegates to sub-agents without
/// depending on a real model choosing to delegate.
///
/// The parent turn (`SubagentDelegationParent`) emits three synchronous
/// `Agent` `tool_use` blocks whose prompts each carry a
/// `PARITY_SCENARIO:subagent_calc_child` marker. Each spawned child inherits
/// the parent's `base_url` (so its `/v1/messages` call reaches the same mock)
/// and its first user message is exactly that prompt, so the mock routes the
/// child call to `SubagentCalcChild`, which answers with the addition sum.
/// Once all three `tool_result`s are back the parent emits the aggregated JSON.
#[tokio::test]
async fn acp_subagent_delegation_deterministic() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("subagent-delegation");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    // Agent tool requires DangerFullAccess to spawn without a permission
    // prompt — matches the live test's `danger-full-access` client.
    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;

    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}subagent_delegation_parent")
                }]
            }),
        )
        .await;

    assert!(
        !notifs.is_empty(),
        "subagent delegation prompt should produce notifications"
    );

    // Exactly three Agent tool_call starts — the wire contract the live test
    // asserts, now deterministic.
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

    // Each subagent's result surfaces via a completed tool_call_update's
    // rawOutput.result — collect them and confirm the three sums.
    let mut agent_results: Vec<String> = notifs
        .iter()
        .filter(|n| {
            let update = &n["params"]["update"];
            update["sessionUpdate"] == "tool_call_update" && update["status"] == "completed"
        })
        .filter_map(|n| {
            n["params"]["update"]["rawOutput"]["result"]
                .as_str()
                .map(String::from)
        })
        .collect();
    agent_results.sort();
    assert!(
        agent_results.iter().any(|r| r.contains("203"))
            && agent_results.iter().any(|r| r.contains("403"))
            && agent_results.iter().any(|r| r.contains("603")),
        "expected subagent results to contain 203, 403, 603 but got: {agent_results:?}"
    );

    let result = &resp["result"];
    assert_eq!(
        result["stopReason"], "end_turn",
        "subagent delegation stopReason should be end_turn"
    );
    assert!(
        result.get("usage").is_some(),
        "subagent delegation response should include usage"
    );

    // This client never opted into sub-agent events: nothing is forwarded
    // and no update carries sub-agent `_meta`.
    for n in &notifs {
        assert!(
            n["params"]["update"].get("_meta").is_none(),
            "no update may carry _meta without the subagentEvents opt-in: {n}"
        );
    }

    client.shutdown().await;
    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// Sub-agent events (`clientCapabilities._meta.sudocode.subagentEvents`).
// The contract is written down in docs/acp.md § Sub-agent events.
// ---------------------------------------------------------------------------

/// How long a test waits for background sub-agents to report their end.
const SUBAGENT_EVENTS_WAIT: Duration = Duration::from_secs(60);

/// A workspace for the sub-agent scenarios: the file the mock child reads and
/// the custom `nester` agent type, which may spawn agents itself.
fn subagent_events_workspace(label: &str, base_url: &str) -> TestWorkspace {
    let workspace = TestWorkspace::new(label);
    workspace.create();
    workspace.write_sudocode_json(base_url);
    fs::write(
        workspace.root.join(SUBAGENT_CHILD_READ_PATH),
        "notes for the sub-agent\n",
    )
    .expect("seed the file the mock child reads");
    let agents = workspace.root.join(".sudocode").join("agents");
    fs::create_dir_all(&agents).expect("custom agents dir");
    fs::write(
        agents.join("nester.md"),
        "---\nname: nester\ndescription: Delegates once more.\ntools: [agent_spawn, read_file]\n---\nYou delegate.\n",
    )
    .expect("write nester agent");
    workspace
}

/// `initialize` with the sub-agent events opt-in; checks scode echoes it.
async fn initialize_with_subagent_events(client: &mut AcpTestClient) {
    let (_, resp) = client
        .send_request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "_meta": { "sudocode": { "subagentEvents": { "version": 1 } } }
                }
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["_meta"]["sudocode"]["subagentEvents"],
        json!({ "version": 1, "cancel": true }),
        "scode must echo the subagentEvents opt-in: {resp}"
    );
}

async fn prompt_scenario(
    client: &mut AcpTestClient,
    session_id: &str,
    scenario: &str,
) -> (Vec<Value>, Value) {
    let (notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}{scenario}") }]
            }),
        )
        .await;
    assert_eq!(
        resp["result"]["stopReason"], "end_turn",
        "{scenario} should end its turn: {resp}"
    );
    (notifs, resp)
}

fn sudocode_meta(n: &Value) -> &Value {
    &n["params"]["update"]["_meta"]["sudocode"]
}

/// `(agentId, phase)` of a lifecycle update, if `n` is one.
fn lifecycle_of(n: &Value) -> Option<(String, String)> {
    let spawn = &sudocode_meta(n)["agentSpawn"];
    let phase = spawn["lifecycle"]["phase"].as_str()?;
    Some((spawn["agentId"].as_str()?.to_string(), phase.to_string()))
}

fn finished_count(notifs: &[Value]) -> usize {
    notifs
        .iter()
        .filter(|n| lifecycle_of(n).is_some_and(|(_, phase)| phase == "finished"))
        .count()
}

/// Keep reading until `want` agents have finished (counting `seen`), and
/// return everything read.
async fn collect_until_finished(
    client: &mut AcpTestClient,
    seen: &[Value],
    want: usize,
) -> Vec<Value> {
    collect_until_subagent_condition(
        client,
        seen,
        |updates| finished_count(updates) >= want,
        &format!("{want} finished sub-agents"),
    )
    .await
}

async fn collect_until_subagent_condition(
    client: &mut AcpTestClient,
    seen: &[Value],
    ready: impl Fn(&[Value]) -> bool,
    goal: &str,
) -> Vec<Value> {
    let mut updates = seen.to_vec();
    let deadline = tokio::time::Instant::now() + SUBAGENT_EVENTS_WAIT;
    while !ready(&updates) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(msg) = timeout(remaining, client.recv_inner()).await else {
            panic!("did not observe {goal} within {SUBAGENT_EVENTS_WAIT:?}: {updates:#?}");
        };
        updates.push(msg);
    }
    updates.split_off(seen.len())
}

/// What one spawned agent looked like on the wire.
#[derive(Debug, Default)]
struct AgentTrace {
    /// Client-visible id of the call that spawned it.
    spawn_call: String,
    /// Its `finished` lifecycle update.
    finished: Value,
    /// Its own stream (updates whose `subagent.agentId` is this agent).
    stream: Vec<Value>,
}

/// The invariants of docs/acp.md § Sub-agent events that hold for any
/// opted-in session, checked over everything it received (in arrival order).
/// Returns each agent's trace.
fn assert_subagent_invariants(notifs: &[Value]) -> BTreeMap<String, AgentTrace> {
    let mut traces: BTreeMap<String, AgentTrace> = BTreeMap::new();
    // (agentId) -> seqs in arrival order, stream and lifecycle together.
    let mut seqs: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    let mut call_ids = std::collections::HashSet::new();
    for n in notifs {
        if n["method"] != "session/update" {
            continue;
        }
        let update = &n["params"]["update"];
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        let meta = sudocode_meta(n);
        if kind == "tool_call" {
            let id = update["toolCallId"].as_str().expect("toolCallId");
            assert!(call_ids.insert(id.to_string()), "duplicate toolCallId {id}");
        }
        if let Some(sub) = meta.get("subagent") {
            let agent = sub["agentId"]
                .as_str()
                .expect("subagent.agentId")
                .to_string();
            let seq = sub["seq"].as_u64().expect("subagent.seq");
            let parent = sub["parentToolCallId"].as_str().expect("parentToolCallId");
            let trace = traces.entry(agent.clone()).or_default();
            if trace.spawn_call.is_empty() {
                trace.spawn_call = parent.to_string();
            }
            assert_eq!(
                trace.spawn_call, parent,
                "one parentToolCallId per agent: {n}"
            );
            trace.stream.push(n.clone());
            seqs.entry(agent.clone())
                .or_default()
                .push((seq, kind.into()));
            if kind.starts_with("tool_call") && lifecycle_of(n).is_none() {
                let id = update["toolCallId"].as_str().expect("toolCallId");
                assert!(
                    id.starts_with(&format!("{agent}:")),
                    "a sub-agent's call id is namespaced with its agentId: {n}"
                );
            }
        } else if kind.starts_with("tool_call") {
            let id = update["toolCallId"].as_str().expect("toolCallId");
            assert!(
                !id.contains(':'),
                "a parent-session call must not look namespaced: {n}"
            );
        }
        if let Some((agent, phase)) = lifecycle_of(n) {
            assert!(
                update.get("status").is_none(),
                "lifecycle updates carry no status: {n}"
            );
            let seq = meta["agentSpawn"]["lifecycle"]["seq"]
                .as_u64()
                .expect("lifecycle.seq");
            let trace = traces.entry(agent.clone()).or_default();
            let spawn_call = update["toolCallId"]
                .as_str()
                .expect("toolCallId")
                .to_string();
            if phase == "started" {
                assert!(
                    trace.spawn_call.is_empty() || trace.spawn_call == spawn_call,
                    "started names the spawning call: {n}"
                );
                trace.spawn_call = spawn_call;
            } else {
                assert_eq!(
                    trace.spawn_call, spawn_call,
                    "finished names the spawning call"
                );
                trace.finished = n.clone();
            }
            seqs.entry(agent).or_default().push((seq, phase));
        }
    }
    for (agent, seen) in &seqs {
        let order: Vec<u64> = seen.iter().map(|(seq, _)| *seq).collect();
        let expected: Vec<u64> = (0..order.len() as u64).collect();
        assert_eq!(
            order, expected,
            "agent {agent}: seq must run 0.. without gaps in arrival order ({seen:?})"
        );
        assert_eq!(
            seen.first().map(|(_, k)| k.as_str()),
            Some("started"),
            "{agent}"
        );
        assert_eq!(
            seen.last().map(|(_, k)| k.as_str()),
            Some("finished"),
            "{agent}"
        );
    }
    traces
}

/// The first `tool_call` with `toolCallId == id`.
fn tool_call_named<'a>(notifs: &'a [Value], id: &str) -> &'a Value {
    notifs
        .iter()
        .find(|n| {
            n["params"]["update"]["sessionUpdate"] == "tool_call"
                && n["params"]["update"]["toolCallId"] == id
        })
        .unwrap_or_else(|| panic!("no tool_call {id}"))
}

/// The status-bearing `tool_call_update` for `id` (not a lifecycle one).
fn tool_result_named<'a>(notifs: &'a [Value], id: &str) -> &'a Value {
    notifs
        .iter()
        .find(|n| {
            let update = &n["params"]["update"];
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == id
                && update.get("status").is_some()
        })
        .unwrap_or_else(|| panic!("no tool_call_update with status for {id}"))
}

/// A child's stream, in order: its read, the read's result, its answer.
fn assert_child_stream(trace: &AgentTrace, agent: &str) {
    let kinds: Vec<&str> = trace
        .stream
        .iter()
        .filter(|n| lifecycle_of(n).is_none())
        .map(|n| {
            n["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(
        kinds,
        ["tool_call", "tool_call_update", "agent_message_chunk"],
        "agent {agent} stream"
    );
    let call = &trace.stream[0]["params"]["update"];
    assert_eq!(
        call["toolCallId"],
        format!("{agent}:{SUBAGENT_CHILD_TOOL_ID}")
    );
    assert_eq!(call["title"], "read_file");
    assert_eq!(trace.stream[1]["params"]["update"]["status"], "completed");
    assert_eq!(
        trace.stream[2]["params"]["update"]["content"]["text"],
        SUBAGENT_CHILD_ANSWER
    );
}

fn assert_finished(trace: &AgentTrace, status: &str, result: Option<&str>) {
    let update = &trace.finished["params"]["update"];
    let lifecycle = &update["_meta"]["sudocode"]["agentSpawn"]["lifecycle"];
    assert_eq!(lifecycle["status"], status, "finished status: {update}");
    assert!(lifecycle["startedAt"].is_string() && lifecycle["completedAt"].is_string());
    if let Some(result) = result {
        assert_eq!(
            update["rawOutput"]["result"], result,
            "final result: {update}"
        );
    }
}

/// Print a scenario's opted-in wire when `SUDOCODE_DUMP_SUBAGENT_EVENTS` is
/// set — how the samples in docs/acp.md were captured.
fn dump_subagent_events(label: &str, notifs: &[Value]) {
    if std::env::var_os("SUDOCODE_DUMP_SUBAGENT_EVENTS").is_some() {
        for n in notifs {
            eprintln!("[{label}] {n}");
        }
    }
}

/// Without the opt-in, the wire is what it was before sub-agent events
/// existed. The fixtures were captured from the pre-contract binary
/// (`origin/main` e3000e02) over these same scenarios and normalized the same
/// way: workspace path, version, the returned session ID, generated ids and
/// timestamps (runs of ten or more digits), the palette colour hashed from an
/// agent id, and the size
/// estimate that depends on path lengths. Background children are given time
/// to finish so a late update would be caught too.
/// Independent terminal tool updates may arrive in completion order; only
/// contiguous runs of those updates are reordered for the comparison.
///
/// Regenerate (only when an off-path change is intended) with
/// `SUDOCODE_UPDATE_ACP_FIXTURES=1`.
///
/// Unix-only: the fixtures hold Unix paths, and on Windows the workspace
/// path appears JSON-escaped, so the byte comparison cannot line up there.
/// The Windows run still checks the off path through
/// `acp_subagent_delegation_deterministic` (no `_meta` without the opt-in)
/// and `scenario_initialize` (no `subagentEvents` advertised).
#[cfg(unix)]
#[tokio::test]
async fn acp_subagent_events_off_matches_pre_contract_output() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let fixtures =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp_subagent_events_off");
    for scenario in [
        "subagent_delegation_parent",
        "subagent_events_sync",
        "subagent_events_background",
        "subagent_events_nested",
    ] {
        let workspace = subagent_events_workspace("subagent-events-off", &server.base_url());
        let mut client = spawn_stdio_client_danger(&workspace);
        scenario_initialize(&mut client).await;
        let initialize = client.raw_lines.last().cloned().expect("initialize line");
        let session_id = scenario_session_new(&mut client, &workspace.root).await;
        client.raw_lines.clear();
        prompt_scenario(&mut client, &session_id, scenario).await;
        // Background children finish after the turn: wait them out.
        let _ = client.recv_until(Duration::from_secs(6), |_| false).await;

        let canonical = fs::canonicalize(&workspace.root).expect("canonical root");
        let normalize = |line: &str| -> String {
            let mut line = line
                // Session IDs are opaque. Keep the original fixture marker
                // while checking every other field in the pre-contract wire.
                .replace(&session_id, "session-<N>-0")
                .replace(canonical.to_string_lossy().as_ref(), "<WS>")
                .replace(workspace.root.to_string_lossy().as_ref(), "<WS>")
                .replace(
                    &format!(
                        r#""name":"scode","version":"{}""#,
                        env!("CARGO_PKG_VERSION")
                    ),
                    r#""name":"scode","version":"<VERSION>""#,
                );
            for color in runtime::agent_color::AGENT_COLOR_PALETTE {
                line = line
                    .replace(&format!(r#""color":"{color}""#), r#""color":"<COLOR>""#)
                    .replace(
                        &format!(r#"\"color\": \"{color}\""#),
                        r#"\"color\": \"<COLOR>\""#,
                    );
            }
            normalize_digit_runs(&normalize_number_after(
                &line,
                r#""estimatedSessionTokens":"#,
            ))
        };
        let actual: String = std::iter::once(initialize)
            .chain(client.raw_lines.iter().cloned())
            .map(|line| normalize(&line) + "\n")
            .collect();
        let fixture = fixtures.join(format!("{scenario}.jsonl"));
        if std::env::var_os("SUDOCODE_UPDATE_ACP_FIXTURES").is_some() {
            fs::write(&fixture, &actual).expect("write fixture");
        } else {
            let expected = fs::read_to_string(&fixture).expect("fixture");
            assert!(
                canonicalize_terminal_tool_updates(&actual)
                    == canonicalize_terminal_tool_updates(&expected),
                "{scenario}: output without the opt-in changed.\n--- expected\n{expected}\n--- actual\n{actual}"
            );
        }
        client.shutdown().await;
        workspace.cleanup();
    }
}

/// Preserve the wire payload and ordering except for adjacent tool completions.
#[cfg(unix)]
fn canonicalize_terminal_tool_updates(transcript: &str) -> String {
    let mut lines: Vec<_> = transcript
        .lines()
        .map(|line| {
            // Golden normalization uses a bare <N> for numeric fields. Parse
            // a disposable copy only to identify terminal updates and ids.
            let message: Value =
                serde_json::from_str(&line.replace("<N>", "0")).expect("normalized ACP message");
            let update = &message["params"]["update"];
            let id = (update["sessionUpdate"] == "tool_call_update"
                && matches!(update["status"].as_str(), Some("completed" | "failed")))
            .then(|| {
                update["toolCallId"]
                    .as_str()
                    .expect("tool call id")
                    .to_owned()
            });
            (id, line)
        })
        .collect();
    let mut start = 0;
    while start < lines.len() {
        if lines[start].0.is_none() {
            start += 1;
            continue;
        }
        let end = start
            + lines[start..]
                .iter()
                .take_while(|(id, _)| id.is_some())
                .count();
        lines[start..end].sort_by(|left, right| left.0.cmp(&right.0));
        start = end;
    }
    lines
        .into_iter()
        .map(|(_, line)| format!("{line}\n"))
        .collect()
}

/// Replace the number following `key` with `<N>`.
#[cfg(unix)]
fn normalize_number_after(line: &str, key: &str) -> String {
    let Some(at) = line.find(key) else {
        return line.to_string();
    };
    let start = at + key.len();
    let end = start
        + line[start..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(line.len() - start);
    format!("{}<N>{}", &line[..start], &line[end..])
}

/// Replace every run of ten or more ASCII digits with `<N>`.
#[cfg(unix)]
fn normalize_digit_runs(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() >= 10 {
            out.push_str("<N>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in line.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Synchronous: the whole child stream lands inside the parent's turn,
/// between `started` and the parent's own result.
#[tokio::test]
async fn acp_subagent_events_sync() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = subagent_events_workspace("subagent-events-sync", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_subagent_events(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;
    let (notifs, _) = prompt_scenario(&mut client, &session_id, "subagent_events_sync").await;
    dump_subagent_events("sync", &notifs);

    let traces = assert_subagent_invariants(&notifs);
    assert_eq!(traces.len(), 1, "one agent: {traces:#?}");
    let (agent, trace) = traces.iter().next().expect("one agent");
    assert_eq!(trace.spawn_call, "toolu_events_sync");
    assert_child_stream(trace, agent);
    assert_finished(trace, "completed", Some(SUBAGENT_CHILD_ANSWER));

    // The spawn call itself: identity from the model's arguments first (no
    // agentId yet), then the parent's result — after `finished` — names it.
    let call = tool_call_named(&notifs, "toolu_events_sync");
    let spawn = &sudocode_meta(call)["agentSpawn"];
    assert!(spawn.get("agentId").is_none(), "{call}");
    assert_eq!(spawn["background"], false);
    assert_eq!(spawn["subagentType"], "general-purpose");
    assert!(sudocode_meta(call).get("subagent").is_none());
    let result = tool_result_named(&notifs, "toolu_events_sync");
    assert_eq!(result["params"]["update"]["status"], "completed");
    assert_eq!(
        sudocode_meta(result)["agentSpawn"]["agentId"],
        agent.as_str()
    );
    let position = |needle: &Value| notifs.iter().position(|n| n == needle).expect("present");
    assert!(
        position(&trace.finished) < position(result),
        "finished precedes the parent's result"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Background, two at once: the parent's calls complete at once with
/// `status: running`; the children then run in parallel after the turn
/// ended, and each one's end — with its result — still reaches the client.
#[tokio::test]
async fn acp_subagent_events_background_parallel() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = subagent_events_workspace("subagent-events-bg", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_subagent_events(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;
    let release = workspace.root.join("release-background-children");
    let scenario = format!(
        "subagent_events_background SUBAGENT_RELEASE:{}",
        json!(release.to_str().expect("fixture release path"))
    );
    let (during, _) = prompt_scenario(&mut client, &session_id, &scenario).await;
    assert_eq!(
        finished_count(&during),
        0,
        "unreleased background children cannot have finished inside the turn"
    );
    for id in ["toolu_events_bg_1", "toolu_events_bg_2"] {
        let result = tool_result_named(&during, id);
        assert_eq!(result["params"]["update"]["rawOutput"]["status"], "running");
        assert_eq!(
            sudocode_meta(tool_call_named(&during, id))["agentSpawn"]["background"],
            true
        );
    }
    // Observe real child streams before either can finish. Fixed response
    // delays only made overlap probable and flaked under workspace build load.
    // A serial executor cannot satisfy this barrier and still fails the test.
    let before_release = collect_until_subagent_condition(
        &mut client,
        &during,
        |updates| {
            assert_eq!(finished_count(updates), 0, "child finished before release");
            let active: std::collections::BTreeSet<_> = updates
                .iter()
                .filter_map(|n| sudocode_meta(n)["subagent"]["agentId"].as_str())
                .collect();
            active.len() == 2
        },
        "two active child streams before release",
    )
    .await;
    let seen: Vec<Value> = during.iter().chain(&before_release).cloned().collect();
    fs::write(release, "both child streams observed").expect("release real children");
    let after = collect_until_finished(&mut client, &seen, 2).await;
    for n in &after {
        assert_eq!(
            n["method"], "session/update",
            "only updates after the turn: {n}"
        );
        assert_eq!(n["params"]["sessionId"], session_id.as_str());
    }
    let all: Vec<Value> = seen.iter().chain(&after).cloned().collect();
    dump_subagent_events("background", &all);

    let traces = assert_subagent_invariants(&all);
    assert_eq!(traces.len(), 2, "two agents: {traces:#?}");
    let mut spawn_calls: Vec<&str> = traces.values().map(|t| t.spawn_call.as_str()).collect();
    spawn_calls.sort_unstable();
    assert_eq!(spawn_calls, ["toolu_events_bg_1", "toolu_events_bg_2"]);
    for (agent, trace) in &traces {
        assert_child_stream(trace, agent);
        assert_finished(trace, "completed", Some(SUBAGENT_CHILD_ANSWER));
    }
    // Parallel: each agent's stream began before the other one finished.
    let position = |needle: &Value| all.iter().position(|n| n == needle).expect("present");
    let traces: Vec<&AgentTrace> = traces.values().collect();
    for (a, b) in [(0, 1), (1, 0)] {
        assert!(
            position(&traces[a].stream[0]) < position(&traces[b].finished),
            "the two children overlap"
        );
    }

    client.shutdown().await;
    workspace.cleanup();
}

/// One level of nesting: the child's own `agent_spawn` is part of the
/// child's stream and spawns a grandchild whose stream points back at it.
#[tokio::test]
async fn acp_subagent_events_nested() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = subagent_events_workspace("subagent-events-nested", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_subagent_events(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;
    let (notifs, _) = prompt_scenario(&mut client, &session_id, "subagent_events_nested").await;
    dump_subagent_events("nested", &notifs);

    let requests = server.captured_requests().await;
    let parent = requests
        .iter()
        .find(|request| request.scenario == "subagent_events_nested")
        .expect("the parent calls the provider");
    let parent_body: Value = serde_json::from_str(&parent.raw_body).unwrap();
    let routing_key = parent_body["metadata"]["user_id"]
        .as_str()
        .filter(|key| !key.is_empty())
        .expect("the parent carries a routing identity");
    for scenario in [
        "subagent_events_nested",
        "subagent_nest_child",
        "subagent_tool_child",
    ] {
        let turns: Vec<_> = requests
            .iter()
            .filter(|request| request.scenario == scenario)
            .collect();
        assert!(!turns.is_empty(), "{scenario} must call the provider");
        for turn in turns {
            let body: Value = serde_json::from_str(&turn.raw_body).unwrap();
            assert_eq!(
                body["metadata"]["user_id"].as_str(),
                Some(routing_key),
                "{scenario} must retain the parent's routing identity",
            );
        }
    }

    let traces = assert_subagent_invariants(&notifs);
    assert_eq!(traces.len(), 2, "child + grandchild: {traces:#?}");
    let (child, child_trace) = traces
        .iter()
        .find(|(_, t)| t.spawn_call == "toolu_events_nest")
        .expect("the child");
    let nested_call = format!("{child}:toolu_nest_spawn");
    let (grandchild, grand_trace) = traces
        .iter()
        .find(|(_, t)| t.spawn_call == nested_call)
        .expect("the grandchild hangs off the child's agent_spawn");
    assert_child_stream(grand_trace, grandchild);
    assert_finished(grand_trace, "completed", Some(SUBAGENT_CHILD_ANSWER));
    assert_finished(child_trace, "completed", Some("subagent_nest_child done"));

    // The child's agent_spawn call: in the child's stream AND a spawn.
    let call = tool_call_named(&notifs, &nested_call);
    assert_eq!(sudocode_meta(call)["subagent"]["agentId"], child.as_str());
    assert_eq!(sudocode_meta(call)["agentSpawn"]["background"], false);
    // The grandchild's lifecycle belongs to the child's stream too.
    assert!(
        !grand_trace.stream.iter().any(|n| lifecycle_of(n).is_some()),
        "a grandchild's lifecycle is not part of its own stream"
    );
    let grand_lifecycle: Vec<&Value> = child_trace
        .stream
        .iter()
        .filter(|n| lifecycle_of(n).is_some_and(|(agent, _)| &agent == grandchild))
        .collect();
    assert_eq!(
        grand_lifecycle.len(),
        2,
        "started + finished in the child's stream"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// `_sudocode/agent/cancel` stops one running background agent, whose end
/// then reports `cancelled`; it refuses agents the session never spawned.
#[tokio::test]
async fn acp_subagent_events_cancel() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = subagent_events_workspace("subagent-events-cancel", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_subagent_events(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;
    let (during, _) = prompt_scenario(&mut client, &session_id, "subagent_events_cancel").await;
    let (agent, _) = during
        .iter()
        .find_map(lifecycle_of)
        .expect("the background agent started inside the turn");

    let (_, refused) = client
        .send_request(
            "_sudocode/agent/cancel",
            json!({ "sessionId": session_id, "agentId": "agent-0" }),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "unknown agent: {refused}");

    let (mut early, resp) = client
        .send_request(
            "_sudocode/agent/cancel",
            json!({ "sessionId": session_id, "agentId": agent }),
        )
        .await;
    assert_eq!(resp["result"], json!({ "cancelled": true }), "{resp}");
    let seen: Vec<Value> = during.iter().chain(&early).cloned().collect();
    let after = collect_until_finished(&mut client, &seen, 1).await;
    early.extend(after);
    let all: Vec<Value> = during.iter().chain(&early).cloned().collect();
    dump_subagent_events("cancel", &all);
    let traces = assert_subagent_invariants(&all);
    assert_finished(&traces[&agent], "cancelled", None);

    client.shutdown().await;
    workspace.cleanup();
}

/// Cancelling an agent interrupts the tool it is running. The child here is
/// inside a 30s `bash` call when the cancel lands; the agent must end with
/// `cancelled` long before that command would have finished on its own.
///
/// Unix-only: the child's tool is a `sh -c` subprocess (`sleep 30`).
#[cfg(unix)]
#[tokio::test]
async fn acp_subagent_cancel_interrupts_the_running_tool() {
    const PROMPT_END: Duration = Duration::from_secs(15);
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = subagent_events_workspace("subagent-cancel-tool", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_subagent_events(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;
    let (during, _) =
        prompt_scenario(&mut client, &session_id, "subagent_events_cancel_tool").await;
    let (agent, _) = during
        .iter()
        .find_map(lifecycle_of)
        .expect("the background agent started inside the turn");

    // Wait until the child's bash call is under way (it may already have
    // been reported inside the turn), then cancel.
    let sleep_call = format!("{agent}:toolu_child_sleep");
    let is_sleep_call = |n: &Value| {
        n["params"]["update"]["sessionUpdate"] == "tool_call"
            && n["params"]["update"]["toolCallId"] == sleep_call.as_str()
    };
    let mut seen = Vec::new();
    let call = if let Some(call) = during.iter().find(|n| is_sleep_call(n)) {
        call.clone()
    } else {
        let (before, call) = client
            .recv_until(Duration::from_secs(30), is_sleep_call)
            .await
            .unwrap_or_else(|seen| panic!("the child never started its bash call: {seen:#?}"));
        seen.extend(before);
        seen.push(call.clone());
        call
    };
    assert_eq!(
        call["params"]["update"]["rawInput"]["command"],
        mock_anthropic_service::SUBAGENT_SLEEP_COMMAND
    );
    let cancelled_at = tokio::time::Instant::now();
    let (early, resp) = client
        .send_request(
            "_sudocode/agent/cancel",
            json!({ "sessionId": session_id, "agentId": agent }),
        )
        .await;
    assert_eq!(resp["result"], json!({ "cancelled": true }), "{resp}");
    seen.extend(early);

    let (after_cancel, finished) = client
        .recv_until(PROMPT_END, |n| {
            lifecycle_of(n).is_some_and(|(id, phase)| id == agent && phase == "finished")
        })
        .await
        .unwrap_or_else(|seen| {
            panic!("the agent did not end within {PROMPT_END:?} of the cancel — the running tool was not interrupted: {seen:#?}")
        });
    assert!(cancelled_at.elapsed() < PROMPT_END);
    seen.extend(after_cancel);
    seen.push(finished);
    let all: Vec<Value> = during.iter().chain(&seen).cloned().collect();
    let traces = assert_subagent_invariants(&all);
    assert_finished(&traces[&agent], "cancelled", None);

    client.shutdown().await;
    workspace.cleanup();
}

/// Without the opt-in there is no cancel surface for a session.
#[tokio::test]
async fn acp_subagent_cancel_needs_the_opt_in() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = subagent_events_workspace("subagent-cancel-off", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    let session_id = scenario_session_new(&mut client, &workspace.root).await;
    let (updates, _) = prompt_scenario(&mut client, &session_id, "subagent_events_cancel").await;
    let spawned =
        &tool_result_named(&updates, "toolu_events_cancel")["params"]["update"]["rawOutput"];
    let agent_id = spawned["agentId"].as_str().expect("background agent id");
    assert!(
        updates
            .iter()
            .all(|update| update["params"]["update"].get("_meta").is_none()),
        "events still require opt-in"
    );
    let (_, resp) = client
        .send_request(
            "_sudocode/agent/cancel",
            json!({ "sessionId": session_id, "agentId": agent_id }),
        )
        .await;
    assert_eq!(resp["error"]["code"], -32602, "{resp}");

    // Closing owns the child even though no rendering events were subscribed.
    let (_, closed) = client
        .send_request("session/close", json!({"sessionId":session_id}))
        .await;
    assert!(closed.get("error").is_none(), "{closed}");
    // Close signals every owned task immediately, but its drain is bounded.
    // A child still starting under load can persist cancellation after the
    // close response. Require that terminal fact within the cancellation budget.
    let manifest_path = workspace
        .root
        .join(".sudocode-agents")
        .join(format!("{agent_id}.json"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let manifest: Value = loop {
        let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        if !matches!(
            manifest["status"].as_str(),
            Some("running" | "backgrounded")
        ) {
            break manifest;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "closing an unsubscribed session did not stop its child: {manifest}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        manifest["status"], "cancelled",
        "closing an unsubscribed session must stop its child: {manifest}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// `session/new.mcp_servers` is injected: the stdio dummy is spawned during
/// runtime build (proof written) and its `echo` tool round-trips through the
/// model (mcp_echo_verdict yields `echo:hello from mcp parity`, not MISSING).
#[tokio::test]
async fn acp_session_new_injects_stdio_mcp() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("mcp-inject");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let dummy = workspace.root.join("dummy-mcp.py");
    fs::write(&dummy, MCP_DUMMY_SCRIPT).expect("write dummy script");
    let proof = workspace.root.join("dummy-proof.txt");
    let token = "token-7f3a91c2";

    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;

    let (_, new_resp) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root.to_string_lossy(),
                "mcpServers": [{
                    "name": "parity",
                    "command": python::resolve_python(),
                    "args": [dummy.to_string_lossy()],
                    "env": [
                        {"name": "DUMMY_PROOF", "value": proof.to_string_lossy()},
                        {"name": "DUMMY_TOKEN", "value": token},
                    ],
                }],
            }),
        )
        .await;
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId should be present")
        .to_string();

    // spawn + env passthrough: proof written with the injected DUMMY_TOKEN.
    let proof_content = fs::read_to_string(&proof).expect("proof should exist after session/new");
    assert_eq!(proof_content, token);

    // tool round-trip: mock calls mcp__parity__echo, dummy returns echo:...,
    // mcp_echo_verdict surfaces `echo:hello from mcp parity` (not MISSING).
    let (notifs, _) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")
                }]
            }),
        )
        .await;
    let blob = serde_json::to_string(&notifs).unwrap_or_default();
    assert!(
        blob.contains("echo:hello from mcp parity"),
        "dummy echo should round-trip; got: {blob}"
    );
    assert!(
        !blob.contains("echo MISSING"),
        "dummy not invoked or bad result shape; got: {blob}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// The actual HTTP MCP authorization and tool are retained when ACP rebuilds
/// its runtime for a model switch. The server receipt proves a new invocation.
#[tokio::test]
async fn acp_http_mcp_auth_and_tool_survive_model_switch() {
    let model = MockAnthropicService::spawn()
        .await
        .expect("start model fixture");
    let workspace = TestWorkspace::new("http-mcp-model-switch");
    workspace.create();
    workspace.write_sudocode_json(&model.base_url());
    let proof = workspace.root.join("http-mcp-receipts.jsonl");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http_mcp.py");
    let mut remote = Command::new(python::resolve_python())
        .arg("-u")
        .arg(script)
        .arg(&proof)
        .arg("fixture-remote-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start real HTTP MCP server");
    let mut ready = BufReader::new(remote.stdout.take().expect("MCP readiness pipe"));
    let mut url = String::new();
    tokio::time::timeout(Duration::from_secs(30), ready.read_line(&mut url))
        .await
        .expect("HTTP MCP readiness deadline")
        .expect("HTTP MCP readiness");
    assert!(
        url.starts_with("http://127.0.0.1:"),
        "unexpected readiness: {url}"
    );
    let mut client = spawn_stdio_client_danger(&workspace);
    let (_, initialized) = client
        .send_request("initialize", json!({"protocolVersion": 1}))
        .await;
    assert_eq!(
        initialized["result"]["agentCapabilities"]["mcpCapabilities"]["http"],
        true
    );
    let (_, created) = client.send_request("session/new", json!({
        "cwd": workspace.root.to_string_lossy(),
        "mcpServers": [{"name": "parity", "type": "http", "url": url.trim(),
            "headers": [{"name": "Authorization", "value": "Bearer fixture-remote-token"}]}],
    })).await;
    let session = created["result"]["sessionId"]
        .as_str()
        .expect("HTTP MCP session");
    {
        let (_, switched) = client
            .send_request(
                "session/set_model",
                json!({"sessionId": session, "modelId": "haiku"}),
            )
            .await;
        assert!(
            switched.get("error").is_none(),
            "model switch failed: {switched}"
        );
        let (updates, response) = client.send_request("session/prompt", json!({
            "sessionId": session,
            "prompt": [{"type": "text", "text": format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")}],
        })).await;
        assert!(
            response.get("error").is_none(),
            "remote prompt failed: {response}"
        );
        let text = serde_json::to_string(&updates).expect("ACP updates");
        assert!(
            text.contains("echo:hello from mcp parity"),
            "HTTP echo missing: {text}"
        );
    }
    let calls: Vec<Value> = fs::read_to_string(proof)
        .expect("real MCP receipt")
        .lines()
        .map(|line| serde_json::from_str(line).expect("MCP receipt JSON"))
        .collect();
    let invoked: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "tools/call")
        .collect();
    assert_eq!(
        invoked.len(),
        1,
        "the prompt after rebuilding must call the authenticated remote tool once"
    );
    assert!(
        calls
            .iter()
            .filter(|call| call["method"] == "initialize")
            .count()
            >= 2,
        "both the initial and rebuilt runtime must authenticate to the remote server"
    );
    assert!(invoked
        .iter()
        .all(|call| call["params"]["arguments"]["text"] == "hello from mcp parity"));
    client.shutdown().await;
    remote.kill().await.expect("stop owned HTTP MCP server");
    workspace.cleanup();
}

/// Injected stdio MCP survives a model switch by respawning with its config.
#[tokio::test]
async fn acp_session_new_mcp_survives_model_switch() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("mcp-modelswitch");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let dummy = workspace.root.join("dummy-mcp.py");
    fs::write(&dummy, MCP_DUMMY_SCRIPT).expect("write dummy script");
    let proof = workspace.root.join("dummy-proof.txt");
    let token = "token-mswitch-44";

    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;

    let mcp_servers = json!([{
        "name": "parity",
        "command": python::resolve_python(),
        "args": [dummy.to_string_lossy()],
        "env": [
            {"name": "DUMMY_PROOF", "value": proof.to_string_lossy()},
            {"name": "DUMMY_TOKEN", "value": token},
        ],
    }]);

    let (_, new_resp) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root.to_string_lossy(),
                "mcpServers": mcp_servers,
            }),
        )
        .await;
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId should be present")
        .to_string();

    // Delete the proof so a rewritten file proves a re-spawn after setModel.
    let _ = fs::remove_file(&proof);

    // Switch to a model != the startup `sonnet`; main.rs:2009 short-circuits
    // when resolved == self.model, so a different model is required to trigger
    // the runtime rebuild in handle_acp_model_switch.
    let (_, set_resp) = client
        .send_request(
            "session/set_model",
            json!({"sessionId": session_id, "modelId": "haiku"}),
        )
        .await;
    assert!(
        set_resp.get("error").is_none(),
        "session/setModel should succeed; got: {set_resp}"
    );

    // Re-spawn: proof rewritten with the same token.
    let proof_content =
        fs::read_to_string(&proof).expect("proof should be rewritten after model switch");
    assert_eq!(proof_content, token);

    // Tool still available after the rebuild.
    let (notifs, _) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")
                }]
            }),
        )
        .await;
    let blob = serde_json::to_string(&notifs).unwrap_or_default();
    assert!(
        blob.contains("echo:hello from mcp parity"),
        "mcp should remain available after model switch; got: {blob}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Run one `streaming_text` turn on `session_id` and return the raw body of
/// the last `/v1/messages` request the mock received for it.
async fn last_model_request_after_prompt(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    session_id: &str,
    text: &str,
) -> String {
    let before = server.captured_requests().await.len();
    let (_notifs, resp) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text {text}") }]
            }),
        )
        .await;
    assert!(
        resp["result"].get("stopReason").is_some(),
        "turn should complete: {resp}"
    );
    let requests = server.captured_requests().await;
    assert!(requests.len() > before, "prompt should reach the model");
    requests
        .iter()
        .rev()
        .find(|r| !r.path.contains("count_tokens"))
        .expect("a /v1/messages request")
        .raw_body
        .clone()
}

/// `session/new` carrying the given `_meta.sudocode` object; returns the raw response.
async fn session_new_with_meta(
    client: &mut AcpTestClient,
    cwd: &std::path::Path,
    sudocode_meta: Value,
) -> Value {
    let (_, resp) = client
        .send_request(
            "session/new",
            json!({
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
                "_meta": { "sudocode": sudocode_meta },
            }),
        )
        .await;
    resp
}

fn session_id_of(resp: &Value) -> String {
    resp["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("sessionId should be present; got: {resp}"))
        .to_string()
}

/// Empty / non-string `_meta.sudocode.systemPrompt` / `appendSystemPrompt`
/// values must be rejected with `invalid_params`, never silently ignored.
async fn assert_bad_prompt_meta_rejected(
    client: &mut AcpTestClient,
    root: &std::path::Path,
    valid_override: &str,
) {
    for bad_meta in [
        json!({"systemPrompt": ""}),
        json!({"systemPrompt": 42}),
        json!({"appendSystemPrompt": "   "}),
        json!({"appendSystemPrompt": ["x"]}),
        json!({"systemPrompt": valid_override, "appendSystemPrompt": 7}),
    ] {
        let resp = session_new_with_meta(client, root, bad_meta.clone()).await;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32602),
            "bad _meta.sudocode {bad_meta} must be rejected with invalid_params; got: {resp}"
        );
    }
}

/// A session `systemPrompt` must replace the built-in blocks *without*
/// discarding what the process-wide `--append-system-prompt` added.
///
/// Regression: the append used to be a dynamic section, so a session override —
/// which only replaces static ones — could not touch it. Moving the append into
/// the static block to get it into the cacheable prefix made the two collide,
/// and `docs/acp.md` promises they layer.
#[tokio::test]
async fn acp_session_override_keeps_process_level_append() {
    const PROCESS_APPEND: &str = "Process-wide rule append-9d1c04.";
    const SESSION_OVERRIDE: &str = "You are session-persona-5e8b72.";

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("sysprompt-layering");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client =
        spawn_stdio_client_with_args(&workspace, &["--append-system-prompt", PROCESS_APPEND]);
    scenario_initialize(&mut client).await;
    let root = workspace.root.clone();

    // Without a session override the process-level append is simply present.
    let plain = scenario_session_new(&mut client, &root).await;
    let body = last_model_request_after_prompt(&mut client, &server, &plain, "l1").await;
    assert!(
        body.contains(PROCESS_APPEND),
        "process-level append must reach the model; body: {body}"
    );

    // With one, it must survive alongside the replacement.
    let resp = session_new_with_meta(
        &mut client,
        &root,
        json!({"systemPrompt": SESSION_OVERRIDE}),
    )
    .await;
    let overridden = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &overridden, "l2").await;
    assert!(
        body.contains(SESSION_OVERRIDE),
        "session override must reach the model; body: {body}"
    );
    assert!(
        body.contains(PROCESS_APPEND),
        "session override must not discard the process-level append; body: {body}"
    );
    assert!(
        !body.contains(DEFAULT_IDENTITY),
        "session override must still replace the built-in identity; body: {body}"
    );
    assert!(
        body.find(SESSION_OVERRIDE) < body.find(PROCESS_APPEND),
        "the replacement comes first, the append stays after it; body: {body}"
    );
}

/// Built-in identity block: present on the default prompt, gone under override.
const DEFAULT_IDENTITY: &str = "You are Sudo Code";
/// The first dynamic block every session carries. The append is a *static*
/// section, so it must land before this — that ordering is what proves it
/// sits in the cacheable prefix rather than the per-turn block.
const MEMORY_HEADING: &str = "# auto memory";

/// `_meta.sudocode.systemPrompt` (replace the built-in static blocks) and
/// `_meta.sudocode.appendSystemPrompt` (append a trailing static block) on
/// `session/new`, checked on the wire body the model receives:
///  - neither → the default prompt (regression guard),
///  - append only → appended at the end of the static block (before the
///    dynamic auto-memory block), identity kept,
///  - override only → identity replaced, nothing appended,
///  - both → both take effect (the two are orthogonal),
///  - other sessions in the process are unaffected,
///  - both survive `session/setModel`, and `session/load` honours both,
///  - empty / non-string values → `invalid_params` for either key.
#[tokio::test]
async fn acp_session_new_system_prompt_override_and_append_reach_model() {
    const OVERRIDE: &str = "You are override-persona-3b9e1d. Reply tersely.";
    const APPEND: &str = "Tenant rule append-4f7a20: always sign off with AHOY.";
    const LOAD_OVERRIDE: &str = "You are load-persona-7c2f40. Reply in haiku.";

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("sysprompt-override");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let root = workspace.root.clone();

    // neither: default prompt, nothing injected.
    let plain = scenario_session_new(&mut client, &root).await;
    let body = last_model_request_after_prompt(&mut client, &server, &plain, "p1").await;
    assert!(
        body.contains(DEFAULT_IDENTITY) && !body.contains(OVERRIDE) && !body.contains(APPEND),
        "no _meta → default prompt; body: {body}"
    );

    // append only.
    let resp =
        session_new_with_meta(&mut client, &root, json!({"appendSystemPrompt": APPEND})).await;
    let append_only = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &append_only, "a1").await;
    assert!(
        body.contains(DEFAULT_IDENTITY) && body.contains(APPEND),
        "append must reach the model with the identity block intact; body: {body}"
    );
    let (identity_at, memory_at, append_at) = (
        body.find(DEFAULT_IDENTITY),
        body.find(MEMORY_HEADING),
        body.find(APPEND),
    );
    assert!(
        memory_at.is_some() && append_at.is_some() && append_at < memory_at,
        "append is a static section, so it must precede the dynamic auto-memory \
         block: append@{append_at:?} memory@{memory_at:?}"
    );
    assert!(
        append_at > identity_at,
        "append must come after the built-in identity block, i.e. last within \
         the static block: identity@{identity_at:?} append@{append_at:?}"
    );

    // override only.
    let resp = session_new_with_meta(&mut client, &root, json!({"systemPrompt": OVERRIDE})).await;
    let override_only = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &override_only, "o1").await;
    assert!(
        body.contains(OVERRIDE) && !body.contains(DEFAULT_IDENTITY) && !body.contains(APPEND),
        "override must replace the identity block; body: {body}"
    );

    // both.
    let resp = session_new_with_meta(
        &mut client,
        &root,
        json!({"systemPrompt": OVERRIDE, "appendSystemPrompt": APPEND}),
    )
    .await;
    let both = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &both, "b1").await;
    assert!(
        body.contains(OVERRIDE) && body.contains(APPEND) && !body.contains(DEFAULT_IDENTITY),
        "override and append must compose; body: {body}"
    );

    // The default session in the same process is still untouched.
    let body = last_model_request_after_prompt(&mut client, &server, &plain, "p2").await;
    assert!(
        body.contains(DEFAULT_IDENTITY) && !body.contains(OVERRIDE) && !body.contains(APPEND),
        "other sessions must not inherit another session's _meta; body: {body}"
    );

    // setModel rebuilds the runtime; both adjustments must be re-applied.
    let (_, set_resp) = client
        .send_request(
            "session/set_model",
            json!({"sessionId": both, "modelId": "haiku"}),
        )
        .await;
    assert!(
        set_resp.get("error").is_none(),
        "session/setModel should succeed; got: {set_resp}"
    );
    let body = last_model_request_after_prompt(&mut client, &server, &both, "b2").await;
    assert!(
        body.contains(OVERRIDE) && body.contains(APPEND) && !body.contains(DEFAULT_IDENTITY),
        "override + append must survive a model switch; body: {body}"
    );

    // session/load accepts the same fields.
    let (_, load_resp) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": plain,
                "cwd": root.to_string_lossy(),
                "mcpServers": [],
                "_meta": { "sudocode": { "systemPrompt": LOAD_OVERRIDE, "appendSystemPrompt": APPEND } },
            }),
        )
        .await;
    assert!(
        load_resp.get("error").is_none(),
        "session/load with _meta should succeed; got: {load_resp}"
    );
    let body = last_model_request_after_prompt(&mut client, &server, &plain, "p3").await;
    assert!(
        body.contains(LOAD_OVERRIDE) && body.contains(APPEND) && !body.contains(DEFAULT_IDENTITY),
        "session/load override + append must reach the model; body: {body}"
    );

    // A failed rebuild retains the live writer and its previous prompt options.
    let config_path = workspace.config_home.join("sudocode.json");
    let valid_config = fs::read(&config_path).unwrap();
    let before_failed_reload = server.captured_requests().await.len();
    fs::write(&config_path, "{invalid reload configuration").unwrap();
    let (_, failed_reload) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": plain, "cwd": root.to_string_lossy(), "mcpServers": [],
                "_meta": {"sudocode": {"systemPrompt": "FAILED_RELOAD_MUST_STAY_PRIVATE"}},
            }),
        )
        .await;
    fs::write(&config_path, valid_config).unwrap();
    assert!(failed_reload.get("error").is_some(), "{failed_reload}");
    assert_eq!(server.captured_requests().await.len(), before_failed_reload);
    let body = last_model_request_after_prompt(&mut client, &server, &plain, "p4").await;
    assert!(
        body.contains(LOAD_OVERRIDE)
            && body.contains(APPEND)
            && !body.contains("FAILED_RELOAD_MUST_STAY_PRIVATE"),
        "failed reload changed the live context: {body}"
    );

    // Validation: empty / non-string values are invalid_params for either key.
    assert_bad_prompt_meta_rejected(&mut client, &root, OVERRIDE).await;

    client.shutdown().await;
    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// `_meta.sudocode.memory` — the per-session memory switch
// ---------------------------------------------------------------------------

/// A distinctive line inside the seeded memory entry. It can only reach the
/// model through the auto-memory block, so finding it in a request body is
/// proof the session read memory.
const MEMORY_ENTRY_MARKER: &str = "deploys run at marker-7ac3f1";

/// Seed `dir` with one valid memory entry plus its index and return the
/// directory's contents as a sorted `(file name, contents)` snapshot.
fn seed_memory_dir(dir: &std::path::Path) -> Vec<(String, String)> {
    fs::create_dir_all(dir).expect("memory dir");
    fs::write(
        dir.join("release-window.md"),
        format!(
            "---\nname: release-window\ndescription: when releases go out\n\
             metadata:\n  type: project\n---\n\n{MEMORY_ENTRY_MARKER}\n"
        ),
    )
    .expect("write memory entry");
    fs::write(
        dir.join("MEMORY.md"),
        "- [Release window](release-window.md) — when releases go out\n",
    )
    .expect("write memory index");
    memory_dir_snapshot(dir)
}

/// Every file directly under `dir`, as sorted `(name, contents)` pairs — the
/// shape the "a disabled session adds nothing and changes nothing" assertion
/// compares.
fn memory_dir_snapshot(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = fs::read_dir(dir)
        .expect("read memory dir")
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| {
            (
                e.file_name().to_string_lossy().to_string(),
                fs::read_to_string(e.path()).expect("read memory file"),
            )
        })
        .collect();
    files.sort();
    files
}

/// `_meta.sudocode.memory` on `session/new`, checked on the wire body the
/// model receives:
///  - absent → memory as today (regression guard: the default is unchanged),
///  - `"disabled"` → no auto-memory block, no entry text,
///  - `"enabled"` → same as absent,
///  - the modes are per-session: one process, one memory directory, sessions
///    on both sides of the switch at the same time,
///  - a disabled session survives `session/setModel` (the rebuild re-applies
///    the mode) and adds/changes nothing under the memory directory,
///  - a later session that omits the key still reads the seeded entry, so
///    disabling only stood memory down — it deleted nothing,
///  - unknown / non-string values → `invalid_params`.
#[tokio::test]
async fn acp_session_memory_toggle_is_per_session() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("memory-toggle");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let memory_dir = workspace.root.join("memory");
    let seeded = seed_memory_dir(&memory_dir);

    let mut client = spawn_stdio_client_with_args_and_env(
        &workspace,
        &[],
        &[("SUDOCODE_MEMORY_DIR", &memory_dir.to_string_lossy())],
    );
    scenario_initialize(&mut client).await;
    let root = workspace.root.clone();

    // No key: memory works exactly as it does today.
    let default_session = scenario_session_new(&mut client, &root).await;
    let body =
        last_model_request_after_prompt(&mut client, &server, &default_session, "mem-default")
            .await;
    assert!(
        body.contains(MEMORY_HEADING) && body.contains(MEMORY_ENTRY_MARKER),
        "no _meta.sudocode.memory → memory reaches the model as before; body: {body}"
    );

    // "disabled": neither the auto-memory instructions nor the entry.
    let resp = session_new_with_meta(&mut client, &root, json!({"memory": "disabled"})).await;
    let disabled = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &disabled, "mem-off").await;
    assert!(
        !body.contains(MEMORY_HEADING) && !body.contains(MEMORY_ENTRY_MARKER),
        "a disabled session must carry no memory block; body: {body}"
    );

    // "enabled": the explicit spelling of the default.
    let resp = session_new_with_meta(&mut client, &root, json!({"memory": "enabled"})).await;
    let enabled = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &enabled, "mem-on").await;
    assert!(
        body.contains(MEMORY_HEADING) && body.contains(MEMORY_ENTRY_MARKER),
        "an explicitly enabled session reads memory; body: {body}"
    );

    // Per-session, not per-process: the sessions opened before the disabled
    // one still read memory after it ran a turn.
    let body =
        last_model_request_after_prompt(&mut client, &server, &default_session, "mem-default-2")
            .await;
    assert!(
        body.contains(MEMORY_ENTRY_MARKER),
        "one session's disabled memory must not reach another session; body: {body}"
    );

    // A runtime rebuild must re-apply the mode, not fall back to the default.
    let (_, set_resp) = client
        .send_request(
            "session/set_model",
            json!({"sessionId": disabled, "modelId": "haiku"}),
        )
        .await;
    assert!(
        set_resp.get("error").is_none(),
        "session/setModel should succeed; got: {set_resp}"
    );
    let body = last_model_request_after_prompt(&mut client, &server, &disabled, "mem-off-2").await;
    assert!(
        !body.contains(MEMORY_HEADING) && !body.contains(MEMORY_ENTRY_MARKER),
        "disabled memory must survive a model switch; body: {body}"
    );

    // Disabling only stands memory down: nothing was added, changed or removed.
    assert_eq!(
        memory_dir_snapshot(&memory_dir),
        seeded,
        "a disabled session must leave the memory directory byte-identical"
    );

    // And a fresh session that omits the key finds the same entry — proof the
    // data was never touched.
    let after = scenario_session_new(&mut client, &root).await;
    let body = last_model_request_after_prompt(&mut client, &server, &after, "mem-after").await;
    assert!(
        body.contains(MEMORY_ENTRY_MARKER),
        "memory must still be readable after a disabled session ran; body: {body}"
    );

    // Validation: an unknown or mistyped value is rejected, never treated as
    // "on" — a client that means to disable memory must not be told nothing.
    for bad in [
        json!({"memory": "off"}),
        json!({"memory": "Disabled"}),
        json!({"memory": ""}),
        json!({"memory": false}),
        json!({"memory": ["disabled"]}),
    ] {
        let resp = session_new_with_meta(&mut client, &root, bad.clone()).await;
        assert_eq!(
            resp["error"]["code"].as_i64(),
            Some(-32602),
            "bad _meta.sudocode {bad} must be rejected with invalid_params; got: {resp}"
        );
    }

    client.shutdown().await;
    workspace.cleanup();
}

/// A disabled session does not create the memory directory either. The
/// enabled path calls `ensure_memory_dir_exists` on every prompt build, so
/// this is the check that memory-off touches the filesystem not at all — it
/// needs a process where no *other* session could have created the directory
/// first, hence its own test.
#[tokio::test]
async fn acp_session_memory_disabled_never_creates_the_memory_directory() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("memory-off-untouched");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let memory_dir = workspace.root.join("never-created");
    let mut client = spawn_stdio_client_with_args_and_env(
        &workspace,
        &[],
        &[("SUDOCODE_MEMORY_DIR", &memory_dir.to_string_lossy())],
    );
    scenario_initialize(&mut client).await;

    let resp =
        session_new_with_meta(&mut client, &workspace.root, json!({"memory": "disabled"})).await;
    let disabled = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &disabled, "mem-fs").await;
    assert!(
        !body.contains(MEMORY_HEADING),
        "a disabled session must carry no memory block; body: {body}"
    );
    assert!(
        !memory_dir.exists(),
        "a disabled session must not create {}",
        memory_dir.display()
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Per-session isolation: session A injects `parity`, session B does not.
/// A sees the tool; B does not (mcp__parity__echo missing → echo MISSING).
#[tokio::test]
async fn acp_session_new_mcp_isolated_per_session() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("mcp-isolation");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let dummy = workspace.root.join("dummy-mcp.py");
    fs::write(&dummy, MCP_DUMMY_SCRIPT).expect("write dummy script");
    let proof_a = workspace.root.join("dummy-proof-a.txt");
    let token_a = "token-a";

    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;

    // Session A: inject parity.
    let (_, new_a) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root.to_string_lossy(),
                "mcpServers": [{
                    "name": "parity",
                    "command": python::resolve_python(),
                    "args": [dummy.to_string_lossy()],
                    "env": [
                        {"name": "DUMMY_PROOF", "value": proof_a.to_string_lossy()},
                        {"name": "DUMMY_TOKEN", "value": token_a},
                    ],
                }],
            }),
        )
        .await;
    let sid_a = new_a["result"]["sessionId"]
        .as_str()
        .expect("sessionId A")
        .to_string();

    // Session B: no injection.
    let (_, new_b) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root.to_string_lossy(),
                "mcpServers": [],
            }),
        )
        .await;
    let sid_b = new_b["result"]["sessionId"]
        .as_str()
        .expect("sessionId B")
        .to_string();

    // A can see parity.
    let (notifs_a, _) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": sid_a,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")
                }]
            }),
        )
        .await;
    let blob_a = serde_json::to_string(&notifs_a).unwrap_or_default();
    assert!(
        blob_a.contains("echo:hello from mcp parity"),
        "session A should see its injected parity; got: {blob_a}"
    );

    // B cannot: the tool is absent on B's runtime → echo MISSING.
    let (notifs_b, _) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": sid_b,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")
                }]
            }),
        )
        .await;
    let blob_b = serde_json::to_string(&notifs_b).unwrap_or_default();
    assert!(
        blob_b.contains("echo MISSING"),
        "session B should NOT see A's parity (per-session isolation); got: {blob_b}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Per-session MCP tools remain available under a global `--allowedTools`
/// allow-list: `build_runtime_with_plugin_state` adds the injected tools'
/// qualified names (`mcp__<server>__<tool>`) to the allow-list, so they are
/// neither hidden from the model nor rejected at execution. Regression guard
/// for the allowed-tools × session-mcp incompatibility.
#[tokio::test]
async fn acp_session_new_mcp_available_under_allowed_tools() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("mcp-allowed");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let dummy = workspace.root.join("dummy-mcp.py");
    fs::write(&dummy, MCP_DUMMY_SCRIPT).expect("write dummy script");
    let proof = workspace.root.join("dummy-proof.txt");
    let token = "token-allowed-9c2f";

    // Active allow-list naming only `Read`; the injected parity tool is not
    // listed, so without the fix it would be filtered out and rejected.
    let mut client = spawn_stdio_client_danger_with_allowed(&workspace, "Read");
    scenario_initialize(&mut client).await;

    let (_, new_resp) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root.to_string_lossy(),
                "mcpServers": [{
                    "name": "parity",
                    "command": python::resolve_python(),
                    "args": [dummy.to_string_lossy()],
                    "env": [
                        {"name": "DUMMY_PROOF", "value": proof.to_string_lossy()},
                        {"name": "DUMMY_TOKEN", "value": token},
                    ],
                }],
            }),
        )
        .await;
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("sessionId should be present")
        .to_string();

    let proof_content = fs::read_to_string(&proof).expect("proof should exist");
    assert_eq!(proof_content, token);

    // With the fix, mcp__parity__echo is auto-added to the allow-list, so the
    // mock's tool_use round-trips. Without the fix it would be filtered out
    // (echo MISSING / tool rejected).
    let (notifs, _) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")
                }]
            }),
        )
        .await;
    let blob = serde_json::to_string(&notifs).unwrap_or_default();
    assert!(
        blob.contains("echo:hello from mcp parity"),
        "session mcp tool must remain available under --allowedTools; got: {blob}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// ACP spec compliance: `tool_call_update` must carry `content` — the field
/// clients render — and not `rawOutput` alone, which the spec marks as an
/// optional machine-readable payload. From v0.1.21 through the shipped
/// v0.1.25 `on_tool_result` filled `rawOutput` only, so every standard ACP
/// client (Zed, and the downstream apeiron layer, which reads `content` per
/// spec) saw tool calls complete with no visible output at all.
///
/// Asserts both fields ride the same notification and that the content text
/// is the *verbatim* tool-result string — i.e. re-deriving `rawOutput` from
/// the content text with the same parse-or-wrap rule the server uses
/// reproduces `rawOutput` exactly.
#[tokio::test]
async fn acp_stdio_tool_call_update_carries_content_and_raw_output() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-tool-call-content");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session = scenario_session_new(&mut client, &workspace.root).await;
    // `allow` so bash runs instead of parking the turn on a permission prompt.
    set_permission_mode(&mut client, &session, "allow").await;

    let prompt_id = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{
                    "type": "text",
                    "text": format!("{SCENARIO_PREFIX}bash_stdout_roundtrip")
                }]
            }),
        )
        .await;
    let (notifs, resp) = client
        .recv_until(Duration::from_secs(60), |m| is_response_to(m, prompt_id))
        .await
        .unwrap_or_else(|seen| panic!("bash_stdout_roundtrip did not finish; seen: {seen:?}"));
    assert!(
        resp["result"].get("stopReason").is_some(),
        "turn should complete normally: {resp}"
    );

    let updates = notifs
        .iter()
        .filter(|m| {
            m["params"]["sessionId"].as_str() == Some(session.as_str())
                && m["params"]["update"]["sessionUpdate"].as_str() == Some("tool_call_update")
        })
        .map(|m| m["params"]["update"].clone())
        .collect::<Vec<_>>();
    assert!(
        !updates.is_empty(),
        "scenario should emit at least one tool_call_update; notifs: {notifs:?}"
    );

    let completed = updates
        .iter()
        .find(|u| u["status"].as_str() == Some("completed"))
        .unwrap_or_else(|| panic!("no completed tool_call_update; updates: {updates:?}"));

    let raw_output = &completed["rawOutput"];
    assert!(
        !raw_output.is_null(),
        "rawOutput must stay populated for backward compatibility: {completed}"
    );

    let content = completed["content"]
        .as_array()
        .unwrap_or_else(|| panic!("tool_call_update must carry ACP `content`: {completed}"));
    assert_eq!(
        content.len(),
        1,
        "one text block per tool result: {completed}"
    );
    assert_eq!(
        content[0]["type"].as_str(),
        Some("content"),
        "content block must use the ACP `content` variant: {completed}"
    );
    assert_eq!(
        content[0]["content"]["type"].as_str(),
        Some("text"),
        "tool output is emitted as a text content block: {completed}"
    );
    let text = content[0]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("text content block must carry `text`: {completed}"));
    assert!(
        text.contains("alpha from bash"),
        "content text should be the bash stdout the model saw, got: {text}"
    );
    // Same parse-or-wrap rule the server applies to build rawOutput; equality
    // proves `text` is the original tool-result string, unreformatted.
    let rederived: Value =
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
    assert_eq!(
        &rederived, raw_output,
        "content text must be the verbatim tool output that rawOutput was built from"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Collect `(index, tool_use ids)` for every message in an Anthropic request
/// body that carries `tool_use` blocks.
fn tool_use_ids_by_message(messages: &[Value]) -> Vec<(usize, Vec<String>)> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let ids = message["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"].as_str() == Some("tool_use"))
                        .filter_map(|block| block["id"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (!ids.is_empty()).then_some((index, ids))
        })
        .collect()
}

/// Regression guard for LLM compaction of a conversation that contains a
/// parallel tool call.
///
/// Anthropic requires every `tool_use` in an assistant message to be
/// answered by a `tool_result` in the SAME next user message. The runtime
/// stores each tool result as its own `Tool`-role message and relies on
/// `convert_messages` to merge consecutive `Tool` messages into one user
/// message; `build_compaction_messages` used to remap `Tool` to `User`
/// before that merge, so the compaction request split the results across
/// consecutive user messages and the API rejected it with
/// `messages.N: tool_use ids were found without tool_result blocks
/// immediately after`. The runtime then silently fell back to the lossy
/// local summary. The mock does not validate pairing, so the assertion on
/// the captured request body has to be explicit.
#[tokio::test]
async fn acp_stdio_llm_compaction_keeps_parallel_tool_results_paired() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-compaction-pairing");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    fs::write(workspace.root.join("fixture.txt"), "alpha parity line\n")
        .expect("fixture.txt should be written");

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session = scenario_session_new(&mut client, &workspace.root).await;

    // Keep enough substantive history that the checkpoint shrinks it and the
    // parallel exchange falls outside the token-budgeted recent suffix.
    for scenario in [
        "multi_tool_turn_roundtrip",
        "streaming_text",
        "streaming_text",
        "streaming_text",
        "streaming_text",
        "streaming_text",
    ] {
        let (_notifs, resp) = client
            .send_request(
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{
                        "type": "text",
                        "text": format!("{SCENARIO_PREFIX}{scenario} {}", "migration design context. ".repeat(100))
                    }]
                }),
            )
            .await;
        assert!(
            resp["result"].get("stopReason").is_some(),
            "{scenario} turn should complete normally: {resp}"
        );
    }

    run_slash_command(&mut client, &session, "/compact").await;

    // Compaction requests are identified by their system prompt rather than
    // the mock's scenario tag: the transcript being summarised still carries
    // the PARITY_SCENARIO marker of earlier task requests.
    let compaction_bodies = server
        .captured_requests()
        .await
        .iter()
        .filter(|request| !request.path.contains("count_tokens"))
        .filter_map(|request| serde_json::from_str::<Value>(&request.raw_body).ok())
        .filter(|body| {
            body["system"]
                .as_str()
                .is_some_and(|system| system.contains("summarizing conversations"))
        })
        .collect::<Vec<_>>();
    assert!(
        !compaction_bodies.is_empty(),
        "auto-compaction should have sent at least one LLM compaction request"
    );

    let mut saw_parallel_tool_use = false;
    for body in &compaction_bodies {
        let messages = body["messages"]
            .as_array()
            .unwrap_or_else(|| panic!("compaction request should carry messages: {body}"));
        for (index, tool_use_ids) in tool_use_ids_by_message(messages) {
            if tool_use_ids.len() > 1 {
                saw_parallel_tool_use = true;
            }
            let next = messages.get(index + 1).unwrap_or_else(|| {
                panic!("compaction request messages.{index} has tool_use blocks but no following message: {body}")
            });
            assert_eq!(
                next["role"].as_str(),
                Some("user"),
                "compaction request messages.{} must be the user message carrying the tool_results: {body}",
                index + 1
            );
            let result_ids = next["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"].as_str() == Some("tool_result"))
                        .filter_map(|block| block["tool_use_id"].as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for id in &tool_use_ids {
                assert!(
                    result_ids.contains(id),
                    "compaction request messages.{index}: tool_use {id} has no tool_result in the \
                     immediately following user message (messages.{} carries {result_ids:?}); \
                     Anthropic rejects this shape with 400 `tool_use ids were found without \
                     tool_result blocks immediately after`. Request: {body}",
                    index + 1
                );
            }
        }
    }
    assert!(
        saw_parallel_tool_use,
        "at least one compaction request should include the parallel tool_use pair; bodies: {compaction_bodies:?}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// `session/new` + `_meta.sudocode.forkFrom`: fork a conversation into a new cwd
// ---------------------------------------------------------------------------

/// `session/new` carrying `forkFrom` for the given source, into `cwd`.
async fn fork_session(
    client: &mut AcpTestClient,
    cwd: &std::path::Path,
    fork_from: Value,
) -> Value {
    fs::create_dir_all(cwd).expect("fork cwd should exist");
    session_new_with_meta(client, cwd, json!({ "forkFrom": fork_from })).await
}

fn assert_invalid_params(resp: &Value, what: &str) {
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "{what} must be rejected with invalid_params, got: {resp}"
    );
}

/// The apeiron shape: the parent conversation lives in one directory, a
/// "branch from here" creates a NEW session in a NEW directory. Before
/// `forkFrom` the only option was a plain `session/new`, which starts empty
/// (`session/load` refuses another cwd) — the model on the branch had no
/// memory of the conversation. With `forkFrom {sessionId}` on an open
/// session the branch's first model request carries the parent's history,
/// the transcript persisted under the new cwd records the lineage, and the
/// parent goes on unaffected.
#[tokio::test]
async fn acp_stdio_fork_copies_open_session_history_into_new_cwd() {
    const MARKER: &str = "fork-marker-live-5b1e9c7d";
    const BRANCH_TURN: &str = "branch-turn-2e0f4a";
    const PARENT_TURN: &str = "parent-turn-9c3d1b";

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-fork-live");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client(&workspace);

    let (_, init) = client
        .send_request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    assert_eq!(
        init["result"]["_meta"]["sudocode"]["sessionFork"].as_bool(),
        Some(true),
        "initialize must advertise _meta.sudocode.sessionFork: {init}"
    );

    let parent = scenario_session_new(&mut client, &workspace.root).await;
    run_marked_turn(&mut client, &parent, MARKER).await;

    let fork_cwd = workspace.root.join("branch");
    let resp = fork_session(&mut client, &fork_cwd, json!({ "sessionId": parent })).await;
    assert!(
        resp.get("error").is_none(),
        "forkFrom an open session should succeed: {resp}"
    );
    let forked = session_id_of(&resp);
    assert_ne!(forked, parent, "a fork is a new session");
    assert_available_commands_update(
        client
            .last_available_commands
            .as_ref()
            .expect("a forked session/new is followed by available_commands_update"),
        &forked,
    );

    // The branch's first model request carries the parent's turn — user
    // message and assistant reply — not just the new prompt.
    let body = last_model_request_after_prompt(&mut client, &server, &forked, BRANCH_TURN).await;
    assert!(
        body.contains(MARKER),
        "forked session's model request must include the parent's message ({MARKER}); \
         a plain session/new would omit it. body: {body}"
    );
    assert!(
        body.contains("\"role\":\"assistant\""),
        "forked session's model request must carry the parent's assistant turn; body: {body}"
    );

    // Persisted under the NEW cwd's store, with the lineage recorded.
    let transcript = read_session_transcript(&fork_cwd, &forked);
    assert!(
        transcript.contains(&parent),
        "forked transcript must record parent_session_id={parent}; got: {transcript}"
    );
    assert!(
        transcript.contains(MARKER),
        "forked transcript must contain the copied history; got: {transcript}"
    );

    // The parent is untouched: its next turn does not see the branch's turn.
    let parent_body =
        last_model_request_after_prompt(&mut client, &server, &parent, PARENT_TURN).await;
    assert!(
        !parent_body.contains(BRANCH_TURN),
        "parent must not see the branch's turn; body: {parent_body}"
    );
    let parent_transcript = read_session_transcript(&workspace.root, &parent);
    assert!(
        !parent_transcript.contains(BRANCH_TURN),
        "parent transcript must not contain the branch's turn"
    );

    // The fork is a first-class session of this process — and of the new
    // cwd: `session/load {cwd: branch}` accepts it after a restart.
    client.shutdown().await;
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let (_, load) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": forked,
                "cwd": fork_cwd.to_string_lossy(),
                "mcpServers": []
            }),
        )
        .await;
    assert!(
        load.get("error").is_none(),
        "the fork must be loadable from its own cwd: {load}"
    );
    let body = last_model_request_after_prompt(&mut client, &server, &forked, "after-reload").await;
    assert!(
        body.contains(MARKER) && body.contains(BRANCH_TURN),
        "reloaded fork must carry both the parent history and its own turn; body: {body}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// `forkFrom` against a session that is NOT open in this process (the
/// parent was created by an earlier process — apeiron after a restart):
/// `sessionId` alone cannot be resolved and is rejected, `cwd` points the
/// lookup at that directory's session store (validated like `session/load`),
/// and `cwd` alone forks the most recent session persisted there.
#[tokio::test]
async fn acp_stdio_fork_from_persisted_session_across_processes() {
    const MARKER: &str = "fork-marker-persisted-a71c04e2";

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-fork-persisted");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());

    // --- Process A: the parent conversation, then exit ---
    let parent = {
        let mut client = spawn_stdio_client(&workspace);
        scenario_initialize(&mut client).await;
        let parent = scenario_session_new(&mut client, &workspace.root).await;
        run_marked_turn(&mut client, &parent, MARKER).await;
        client.shutdown().await;
        parent
    };

    // --- Process B: fork it ---
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let branch_a = workspace.root.join("branch-a");
    let branch_b = workspace.root.join("branch-b");
    let elsewhere = workspace.root.join("elsewhere");

    // Malformed `forkFrom` is rejected, never silently ignored.
    for (label, meta) in [
        ("non-object", json!("nope")),
        ("empty object", json!({})),
        ("empty sessionId", json!({ "sessionId": "  " })),
        ("non-string cwd", json!({ "cwd": 7 })),
    ] {
        let resp = fork_session(&mut client, &branch_a, meta).await;
        assert_invalid_params(&resp, &format!("forkFrom {label}"));
    }

    // Not open here and no cwd to look it up under.
    let resp = fork_session(&mut client, &branch_a, json!({ "sessionId": parent })).await;
    assert_invalid_params(&resp, "forkFrom.sessionId of a session that is not open");

    // Wrong directory: the persisted workspace root does not match.
    fs::create_dir_all(&elsewhere).expect("create elsewhere");
    let resp = fork_session(
        &mut client,
        &branch_a,
        json!({ "sessionId": parent, "cwd": elsewhere.to_string_lossy() }),
    )
    .await;
    assert_invalid_params(&resp, "forkFrom with a cwd the session was not created in");

    // sessionId + cwd: the persisted transcript is the source.
    let resp = fork_session(
        &mut client,
        &branch_a,
        json!({ "sessionId": parent, "cwd": workspace.root.to_string_lossy() }),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "forkFrom persisted session: {resp}"
    );
    let forked_a = session_id_of(&resp);
    let body = last_model_request_after_prompt(&mut client, &server, &forked_a, "branch-a").await;
    assert!(
        body.contains(MARKER),
        "fork of a persisted session must carry its history; body: {body}"
    );

    // cwd only: the latest session persisted under that directory (the
    // parent — the fork above lives under branch-a, not here).
    let resp = fork_session(
        &mut client,
        &branch_b,
        json!({ "cwd": workspace.root.to_string_lossy() }),
    )
    .await;
    assert!(resp.get("error").is_none(), "forkFrom.cwd only: {resp}");
    let forked_b = session_id_of(&resp);
    assert_ne!(forked_b, forked_a);
    let body = last_model_request_after_prompt(&mut client, &server, &forked_b, "branch-b").await;
    assert!(
        body.contains(MARKER) && !body.contains("branch-a"),
        "cwd-only fork must start from the parent, not from the sibling branch; body: {body}"
    );
    assert!(
        read_session_transcript(&branch_b, &forked_b).contains(&parent),
        "cwd-only fork must record the parent as its lineage"
    );

    // A directory with no sessions at all cannot be forked from.
    let empty = workspace.root.join("empty");
    fs::create_dir_all(&empty).expect("create empty");
    let resp = fork_session(
        &mut client,
        &branch_a,
        json!({ "cwd": empty.to_string_lossy() }),
    )
    .await;
    assert_invalid_params(&resp, "forkFrom.cwd with no persisted session");

    client.shutdown().await;
    workspace.cleanup();
}

/// A provider failure during manual maintenance is a failed prompt, with
/// ordered lifecycle notifications and unchanged durable message history.
#[tokio::test]
async fn acp_compaction_failure_is_terminal_and_preserves_history() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("compaction-terminal-failure");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let (session_id, path) = seed_filler_history(&server, &workspace, 32).await;
    let before = runtime::Session::load_from_path(&path).unwrap();
    // A deliberately incompatible response exercises a bounded summary
    // failure while its real HTTP attempts still produce usage receipts.
    let invalid_provider = OpenAiCompatMock::spawn("").await.unwrap();
    workspace.write_sudocode_json(invalid_provider.base_url());
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({
                "sessionId": session_id, "cwd": workspace.root, "mcpServers": []
            }),
        )
        .await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let (notifs, response) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id, "prompt": [{ "type": "text", "text": "/compact" }]
            }),
        )
        .await;
    assert_compaction_lifecycle(&notifs, "failed");
    let text = notifs
        .iter()
        .filter_map(|n| n["params"]["update"]["content"]["text"].as_str())
        .collect::<String>();
    assert!(response.get("error").is_some(), "{response}");
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("上下文压缩失败，本次对话已停止"));
    assert!(text.contains("上下文压缩失败，本次对话已停止"), "{text}");
    let restored = runtime::Session::load_from_path(&path).unwrap();
    assert_eq!(restored.messages(), before.messages());
    assert_eq!(restored.compaction, before.compaction);
    assert_eq!(
        restored.compaction_achievement,
        before.compaction_achievement
    );
    let report = restored.last_compaction_report.as_ref().unwrap();
    assert_eq!(report.outcome, runtime::CompactionOutcome::Failed);
    assert_eq!(
        report.before_history,
        runtime::estimate_session_tokens(&before)
    );
    assert_eq!(report.after_history, report.before_history);
    assert!((1..=4).contains(&report.attempts), "{report:?}");
    assert!(report.completed_responses <= 2, "{report:?}");
    let receipts: Vec<_> = restored
        .maintenance_usage
        .iter()
        .filter(|receipt| receipt.run_id == report.run_id)
        .collect();
    assert_eq!(receipts.len(), report.attempts);
    assert!(receipts
        .iter()
        .all(|receipt| (1..=4).contains(&receipt.attempt_id)));
    client.shutdown().await;
    workspace.cleanup();
}

/// An unanswered question must not stop tool notifications in the same turn.
#[tokio::test]
async fn acp_question_wait_keeps_same_turn_tool_results_streaming() {
    let server = MockAnthropicService::spawn().await.expect("mock service");
    let workspace = TestWorkspace::new("stdio-parallel-question");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    std::fs::write(
        workspace.root.join("parallel-read.txt"),
        "ACP_PARALLEL_READ_RESULT",
    )
    .unwrap();
    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    let session = scenario_session_new(&mut client, &workspace.root).await;
    let calls = json!([
        {"id":"question", "name":"AskUserQuestion", "input":{"question":"ACP_PARALLEL_QUESTION"}},
        {"id":"read", "name":"Read", "input":{"path":"parallel-read.txt"}}
    ]);
    let prompt = client.send_request_no_wait("session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]})).await;
    let (seen, question) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "_scode/ask_user_question")
        })
        .await
        .unwrap_or_else(|seen| panic!("missing question: {seen:?}"));
    if !serde_json::to_string(&seen)
        .unwrap()
        .contains("ACP_PARALLEL_READ_RESULT")
    {
        client
            .recv_until(Duration::from_secs(15), |m| {
                m["method"] == "session/update"
                    && m.to_string().contains("ACP_PARALLEL_READ_RESULT")
            })
            .await
            .unwrap_or_else(|seen| {
                panic!("sibling output blocked by unanswered question: {seen:?}")
            });
    }
    client.send_raw(&json!({"jsonrpc":"2.0","id":question["id"],"result":{"answers":[{"id":"q1","value":"ack","label":"ack"}]}})).await;
    let (_, response) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, prompt))
        .await
        .unwrap_or_else(|seen| panic!("turn did not finish: {seen:?}"));
    assert!(response["result"].get("stopReason").is_some(), "{response}");
    client.shutdown().await;
    workspace.cleanup();
}

/// Cancelling a prompt closes its input lease. A late wire reply must not
/// answer a different question in the next turn of the same engine session.
#[tokio::test]
async fn acp_cancelled_question_does_not_answer_the_next_turn() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("stdio-question-cancel-resume");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let calls = json!([{ "id":"question", "name":"AskUserQuestion", "input":{"question":"Choose the next action"} }]);
    let prompt = json!({"sessionId":sid,"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]});
    let first = client
        .send_request_no_wait("session/prompt", prompt.clone())
        .await;
    let (_, old_question) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "_scode/ask_user_question")
        })
        .await
        .unwrap();
    let other_sid = scenario_session_new(&mut client, &workspace.root).await;
    let other_prompt = json!({"sessionId":other_sid,"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]});
    let other = client
        .send_request_no_wait("session/prompt", other_prompt)
        .await;
    let (_, other_question) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "_scode/ask_user_question")
        })
        .await
        .unwrap();
    assert_ne!(
        old_question["params"]["toolCallId"], other_question["params"]["toolCallId"],
        "wire question ids must be unique across sessions on the same connection"
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":other_question["id"],"result":{"answers":[{"id":"q1","value":"other","label":"other"}]}})).await;
    let (_, other_completed) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, other))
        .await
        .unwrap();
    assert_eq!(other_completed["result"]["stopReason"], "end_turn");
    client
        .send_raw(&json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}}))
        .await;
    let (_, cancelled) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, first))
        .await
        .unwrap();
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );
    let mut next_calls = calls.clone();
    next_calls[0]["id"] = json!("question-next");
    let next_prompt = json!({"sessionId":sid,"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{next_calls}")}]});
    let second = client
        .send_request_no_wait("session/prompt", next_prompt)
        .await;
    let (_, new_question) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "_scode/ask_user_question")
        })
        .await
        .unwrap();
    assert_ne!(
        old_question["params"]["toolCallId"],
        new_question["params"]["toolCallId"]
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":old_question["id"],"result":{"answers":[{"id":"q1","value":"old","label":"old"}]}})).await;
    // A round trip lets any stale response run through the server before we
    // answer the live question; the second prompt must still be waiting.
    let (during, _) = client.send_request("session/list", json!({})).await;
    assert!(
        during.iter().all(|m| !is_response_to(m, second)),
        "stale answer completed the next turn: {during:?}"
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":new_question["id"],"result":{"answers":[{"id":"q1","value":"new","label":"new"}]}})).await;
    let (_, completed) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, second))
        .await
        .unwrap();
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    let requests = server.captured_requests().await;
    let body: Value = serde_json::from_str(
        &requests
            .iter()
            .rev()
            .find(|r| !r.path.contains("count_tokens"))
            .unwrap()
            .raw_body,
    )
    .unwrap();
    assert!(
        body["messages"].to_string().contains("new"),
        "live answer must reach the model"
    );
    client.shutdown().await;
    workspace.cleanup();
}

/// Plan approval must work without a TTY and continue inside the same engine
/// turn. The client supplies only its answer, never a replacement prompt.
#[tokio::test]
async fn acp_plan_approval_carries_markdown_and_executes_without_a_new_prompt() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("stdio-plan-approval");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let session = scenario_session_new(&mut client, &workspace.root).await;
    set_permission_mode(&mut client, &session, "allow").await;
    let prompt = client.send_request_no_wait("session/prompt", json!({
        "sessionId": session,
        "prompt": [{"type":"text","text":format!("{SCENARIO_PREFIX}plan_execution_roundtrip")}]
    })).await;
    let (_, question) = client
        .recv_until(Duration::from_secs(30), |message| {
            is_server_request(message, "_scode/ask_user_question")
        })
        .await
        .unwrap_or_else(|seen| panic!("missing plan review: {seen:?}"));
    assert_eq!(question["params"]["descriptionFormat"], "markdown");
    let document = question["params"]["description"].as_str().unwrap();
    assert!(document.contains(mock_anthropic_service::PLAN_REVIEW_MARKDOWN));
    for section in ["## Context", "## Constraints", "## Acceptance Criteria"] {
        assert!(document.contains(section));
    }
    assert!(!workspace.root.join("plan-executed.txt").exists());
    client
        .send_raw(&json!({"jsonrpc":"2.0","id":question["id"],"result":{
            "answers":[{"id":"action","value":"1","label":"Clear context & execute plan"}]
        }}))
        .await;
    let (_, response) = client
        .recv_until(Duration::from_secs(30), |message| {
            is_response_to(message, prompt)
        })
        .await
        .unwrap_or_else(|seen| {
            panic!("approved plan did not finish its original prompt: {seen:?}")
        });
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        fs::read_to_string(workspace.root.join("plan-executed.txt")).unwrap(),
        "completed from approved plan\n"
    );
    client.shutdown().await;
    workspace.cleanup();
}

/// Background terminals belong to the session, rather than its turn observer.
#[cfg(unix)]
#[tokio::test]
async fn acp_background_terminal_is_queryable_across_prompts() {
    let server = MockAnthropicService::spawn().await.expect("mock service");
    let workspace = TestWorkspace::new("background-terminal-turns");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    let session = scenario_session_new(&mut client, &workspace.root).await;

    async fn call(client: &mut AcpTestClient, session: &str, name: &str, input: Value) -> Value {
        let call_id = format!("background-operation-{}", client.next_id);
        let calls = json!([{"id":call_id, "name":name, "input":input}]);
        let (updates, response) = client.send_request("session/prompt", json!({
            "sessionId":session,
            "prompt":[{"type":"text", "text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]
        })).await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        updates
            .into_iter()
            .find_map(|update| {
                let update = &update["params"]["update"];
                (update["toolCallId"].as_str() == Some(call_id.as_str())
                    && update["status"] == "completed"
                    && update.get("rawOutput").is_some())
                .then(|| update["rawOutput"].clone())
            })
            .expect("completed tool output")
    }

    let launched = call(&mut client, &session, "Bash", json!({
        "command":"echo ACP_BACKGROUND_STARTED; while [ ! -f release-background ]; do sleep 0.1; done; echo ACP_BACKGROUND_FINISHED; exit 7",
        "run_in_background":true,
        "description":"Cross-prompt background terminal"
    })).await;
    let id = launched["backgroundTaskId"]
        .as_str()
        .expect("background task id");
    let running = call(
        &mut client,
        &session,
        "pid_output",
        json!({"pid":id,"block":true,"timeout_ms":100}),
    )
    .await;
    assert_eq!(running["status"], "running", "{running}");
    assert_eq!(running["retrieval_status"], "timeout", "{running}");
    fs::write(workspace.root.join("release-background"), "released").unwrap();
    let finished = call(
        &mut client,
        &session,
        "pid_output",
        json!({"pid":id,"block":true,"timeout_ms":5000}),
    )
    .await;
    assert_eq!(finished["status"], "failed", "{finished}");
    assert_eq!(finished["exit_code"], 7, "{finished}");
    assert_eq!(finished["retrieval_status"], "success", "{finished}");
    assert!(finished["stdout"]
        .as_str()
        .unwrap()
        .contains("ACP_BACKGROUND_FINISHED"));
    assert_eq!(
        fs::read_to_string(finished["rawOutputPath"].as_str().unwrap()).unwrap(),
        "ACP_BACKGROUND_STARTED\nACP_BACKGROUND_FINISHED\n"
    );
    client.shutdown().await;
    workspace.cleanup();
}

struct CompletionFixtureRelease(PathBuf);
impl Drop for CompletionFixtureRelease {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
}

/// Locate the sole context in fixtures that open one completion writer.
fn completion_context_path(workspace: &TestWorkspace, sid: &str) -> PathBuf {
    let transcript = find_session_transcript(&workspace.root, sid);
    let binding = transcript.with_extension("completions.json");
    let saved: Value = serde_json::from_slice(&fs::read(&binding).unwrap()).unwrap();
    if saved["sources"].is_object() {
        return binding;
    }
    let contexts: Vec<_> = fs::read_dir(transcript.with_extension("completions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    assert_eq!(
        contexts.len(),
        1,
        "expected one completion context: {contexts:?}"
    );
    contexts.into_iter().next().unwrap()
}

/// Bind the v2 journal capture to a real ACP context, including its prompt snapshot.
fn seed_legacy_completion(workspace: &TestWorkspace, sid: &str, context: &Value) -> Value {
    use sha2::{Digest, Sha256};

    let cwd = fs::canonicalize(&workspace.root).unwrap();
    let path = find_session_transcript(&workspace.root, sid);
    let session = runtime::Session::load_from_path(&path).unwrap();
    let mut meta: Value = serde_json::from_str(&session.to_json().unwrap().render()).unwrap();
    let messages = meta.as_object_mut().unwrap().remove("messages").unwrap();
    meta["type"] = json!("session_meta");
    let mut records = vec![meta.to_string()];
    for mut message in messages.as_array().unwrap().iter().cloned() {
        message.as_object_mut().unwrap().remove("history");
        records.push(json!({"type":"message","message":message}).to_string());
    }
    let transcript = records.join("\n") + "\n";
    let mut journal: Value =
        serde_json::from_str(include_str!("fixtures/completion_v2/journal.json")).unwrap();
    let binding: Value =
        serde_json::from_slice(&fs::read(path.with_extension("completions.json")).unwrap())
            .unwrap();
    journal["session_id"] = json!(sid);
    journal["cwd"] = json!(cwd);
    journal["storage_ref"] = binding["storage_ref"].clone();
    journal["context"] = context.clone();
    journal["context"]["digest"] = json!(format!("{:x}", Sha256::digest(transcript.as_bytes())));
    let directory = path.with_extension("completions");
    assert!(fs::canonicalize(&directory).unwrap().starts_with(&cwd));
    fs::remove_dir_all(directory).unwrap();
    fs::write(&path, transcript).unwrap();
    fs::write(path.with_extension("completions.json"), journal.to_string()).unwrap();
    journal["context"].clone()
}

#[tokio::test]
async fn acp_legacy_completion_upgrade_preserves_context_and_model_history() {
    exercise_legacy_completion_upgrade(false).await;
}

#[tokio::test]
async fn acp_legacy_completion_upgrade_into_record_history_preserves_context_and_model_history() {
    exercise_legacy_completion_upgrade(true).await;
}

async fn exercise_legacy_completion_upgrade(is_record_history: bool) {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("legacy-completion-upgrade");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let spawn = || {
        spawn_stdio_client_with_args_and_env(
            &workspace,
            &["--permission-mode", "danger-full-access"],
            &[(
                "SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY",
                if is_record_history { "1" } else { "0" },
            )],
        )
    };
    let mut producer = spawn();
    initialize_task_completions(&mut producer).await;
    let sid = scenario_session_new(&mut producer, &workspace.root).await;
    let (_, previous) = producer
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,"_meta":{"traceId":"legacy-prefix"},
                "prompt":[{"type":"text","text":format!(
                    "LEGACY_COMPLETION_PREFIX {SCENARIO_PREFIX}streaming_text")}]}),
        )
        .await;
    assert_eq!(previous["result"]["stopReason"], "end_turn", "{previous}");
    let context = previous["result"]["_meta"]["sudocode"]["contextRef"].clone();
    producer.shutdown().await;
    let previous_context = seed_legacy_completion(&workspace, &sid, &context);
    let binding: Value = serde_json::from_slice(
        &fs::read(
            find_session_transcript(&workspace.root, &sid).with_extension("completions.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let before_load = server.captured_requests().await.len();
    let mut resumed = spawn();
    initialize_task_completions(&mut resumed).await;
    let (_, loaded) = resumed
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    let metadata = &loaded["result"]["_meta"]["sudocode"];
    assert_eq!(metadata["storageRef"], binding["storage_ref"]);
    assert_eq!(
        metadata["contextRef"]["revision"],
        previous_context["revision"]
    );
    assert_eq!(
        metadata["contextRef"]["lastTraceId"],
        previous_context["lastTraceId"]
    );
    assert_eq!(server.captured_requests().await.len(), before_load);
    let (_, continued) = resumed
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,"_meta":{"traceId":"after-upgrade"},
                "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}streaming_text")}]}),
        )
        .await;
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    assert!(server
        .captured_requests()
        .await
        .last()
        .unwrap()
        .raw_body
        .contains("LEGACY_COMPLETION_PREFIX"));
    resumed.shutdown().await;
    workspace.cleanup();
}

async fn frozen_completion_sources(workspace: &TestWorkspace, sid: &str, count: usize) -> Value {
    let path = completion_context_path(workspace, sid);
    timeout(Duration::from_secs(15), async {
        loop {
            let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let sources = saved["sources"].as_object().unwrap();
            if sources.len() == count
                && sources.values().all(|source| source["payload"].is_object())
            {
                break saved;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all expected completion payloads must be frozen before process exit")
}

async fn initialize_with_completions(client: &mut AcpTestClient) {
    let (_, response) = client
        .send_request(
            "initialize",
            json!({
                "protocolVersion": 1, "clientCapabilities": {"_meta":{"sudocode":{
                    "subagentEvents":{"version":1}, "subagentCompletions":{"version":1}
                }}}
            }),
        )
        .await;
    assert_eq!(
        response["result"]["_meta"]["sudocode"]["subagentCompletions"],
        json!({"version":1})
    );
}

#[tokio::test]
async fn acp_completions_retry_ack_and_exact_context() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = subagent_events_workspace("completions", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_completions(&mut client).await;
    let (_, created) = client
        .send_request("session/new", json!({"cwd":workspace.root,"mcpServers":[]}))
        .await;
    let sid = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let metadata = &created["result"]["_meta"]["sudocode"];
    assert!(
        metadata["workspace"]["fileRoot"].as_str().is_some(),
        "{created}"
    );
    assert!(
        metadata["workspace"]["shellRoot"].as_str().is_some(),
        "{created}"
    );
    let (_, replay) = client
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid, "cwd":workspace.root}),
        )
        .await;
    assert_eq!(replay["result"]["unresolved"], 0);
    let (during, response) = client.send_request("session/prompt", json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}subagent_events_background")}],
        "_meta":{"traceId":"run-1:1"}
    })).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let context = response["result"]["_meta"]["sudocode"]["contextRef"].clone();
    assert_eq!(context["lastTraceId"], "run-1:1");
    assert_eq!(context["sessionId"], sid);
    assert!(during
        .iter()
        .all(|v| v["method"] != "_sudocode/subagent/completed"));
    let (_, first) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap();
    assert_eq!(first["params"]["originTraceId"], "run-1:1");
    assert_eq!(first["params"]["sessionId"], sid);
    assert!(first["params"]["message"]
        .as_str()
        .unwrap()
        .contains(SUBAGENT_CHILD_ANSWER));
    // Failure to durably receive must cause the exact payload to be retried.
    client.send_raw(&json!({"jsonrpc":"2.0","id":first["id"],"error":{"code":-32603,"message":"database unavailable"}})).await;
    let (_, mut retry) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap();
    assert_eq!(retry["params"], first["params"]);
    frozen_completion_sources(&workspace, &sid, 2).await;
    // Repeated cold bindings must retain the same unacknowledged payload,
    // including when an old idle event reaches the transport after binding.
    for restart in 1..=3 {
        client.shutdown().await;
        client = spawn_stdio_client_danger(&workspace);
        initialize_with_completions(&mut client).await;
        let (during, inventory) = client
            .send_request(
                "_sudocode/subagent/replay",
                json!({"sessionId":sid,"cwd":workspace.root}),
            )
            .await;
        assert!(inventory.get("result").is_some(), "{inventory}");
        retry = if let Some(request) = during
            .into_iter()
            .find(|v| v["method"] == "_sudocode/subagent/completed")
        {
            request
        } else {
            client
                .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
                    v["method"] == "_sudocode/subagent/completed"
                })
                .await
                .unwrap_or_else(|seen| {
                    panic!("cold replay {restart} lost delivery: inventory={inventory}, messages={seen:?}")
                })
                .1
        };
        assert_eq!(
            retry["params"], first["params"],
            "restart {restart} must preserve the frozen delivery"
        );
    }

    client.send_raw(&json!({"jsonrpc":"2.0","id":retry["id"],"result":{"completionId":retry["params"]["completionId"],"disposition":"stored"}})).await;
    let (_, second) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap();
    assert_ne!(
        second["params"]["completionId"],
        first["params"]["completionId"]
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":second["id"],"result":{"completionId":second["params"]["completionId"],"disposition":"stored"}})).await;
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert_eq!(
        loaded["result"]["_meta"]["sudocode"]["contextRef"], context,
        "{loaded}"
    );
    let metadata = &loaded["result"]["_meta"]["sudocode"];
    assert!(metadata["storageRef"].as_str().is_some(), "{loaded}");
    assert!(
        metadata["workspace"]["fileRoot"].as_str().is_some(),
        "{loaded}"
    );
    assert!(
        metadata["workspace"]["shellRoot"].as_str().is_some(),
        "{loaded}"
    );
    let transcript = find_session_transcript(&workspace.root, &sid);
    let saved = fs::read(&transcript).unwrap();
    let saved_text = String::from_utf8(saved.clone()).unwrap();
    let records: Vec<Value> = saved_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let text = records
        .iter()
        .filter_map(|record| record["message"]["blocks"].as_array())
        .flatten()
        .find(|block| block["type"] == "text")
        .expect("saved history contains text");
    let changed = saved_text
        .replace(
            &text["text"].to_string(),
            &json!(format!(
                "{} externally edited",
                text["text"].as_str().unwrap()
            ))
            .to_string(),
        )
        .into_bytes();
    fs::write(&transcript, &changed).unwrap();
    let (_, unconfirmed) = client
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert!(
        unconfirmed["error"]
            .to_string()
            .contains("source_context_unconfirmed"),
        "load must validate the durable context: {unconfirmed}"
    );
    assert_eq!(
        fs::read(&transcript).unwrap(),
        changed,
        "rejected load must leave the transcript intact"
    );
    fs::write(&transcript, saved).unwrap();
    let before_rejection = fs::read(&transcript).unwrap();
    let requests_before_rejection = server.captured_requests().await.len();
    let mut wrong = context.clone();
    wrong["digest"] = json!("wrong");
    let (updates, rejected) = client
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
                "prompt":[{"type":"text","text":"must not reach the model"},{"type":"image","data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=","mimeType":"image/png"}],
                "_meta":{"traceId":"run-2:1","sudocode":{"expectedContext":wrong}}
            }),
        )
        .await;
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert!(updates.is_empty());
    assert_eq!(
        server.captured_requests().await.len(),
        requests_before_rejection,
        "rejected context must not invoke a model"
    );
    assert_eq!(
        fs::read(&transcript).unwrap(),
        before_rejection,
        "rejected image input must not mutate the transcript"
    );

    client.shutdown().await;
    // Cold replay delivers metadata without loading even one historical engine.
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_completions(&mut client).await;
    let (_, replay) = client
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert_eq!(replay["result"]["unresolved"], 0, "{replay}");
    assert_eq!(replay["result"]["contextRef"], context);
    let (_, listed) = client.send_request("session/list", json!({})).await;
    assert_eq!(
        listed["result"]["sessions"],
        json!([]),
        "replay must not construct engines: {listed}"
    );
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert_eq!(
        loaded["result"]["_meta"]["sudocode"]["contextRef"], context,
        "{loaded}"
    );
    let (_, continued) = client
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
                "prompt":[{"type":"text","text":format!("Summarize the completed workers.\n{}\n{}", first["params"]["message"].as_str().unwrap(), second["params"]["message"].as_str().unwrap())}],
                "_meta":{"traceId":"run-2:1","sudocode":{"expectedContext":context}}
            }),
        )
        .await;
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    assert_eq!(
        continued["result"]["_meta"]["sudocode"]["contextRef"]["revision"],
        2
    );
    let requests = server.captured_requests().await;
    let parent = requests
        .iter()
        .rev()
        .find(|request| !request.path.contains("count_tokens"))
        .expect("continued parent request");
    let body: Value = serde_json::from_str(&parent.raw_body).unwrap();
    let messages = serde_json::to_string(&body["messages"]).unwrap();
    for completion in [&first, &second] {
        let message = completion["params"]["message"].as_str().unwrap();
        let escaped = serde_json::to_string(message).unwrap();
        assert_eq!(
            messages.matches(&escaped[1..escaped.len() - 1]).count(),
            1,
            "each acknowledged result must reach the parent model exactly once"
        );
    }
    client.shutdown().await;
    workspace.cleanup();
}

/// Connection replacement and ACK validation belong to the shared engine driver.
#[tokio::test]
async fn acp_completion_rebind_ignores_late_receipts_and_quarantines_bad_ack() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = subagent_events_workspace("completion-rebind", &server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_with_completions(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let (_, parent) = client
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}subagent_events_background")}],
        "_meta":{"traceId":"rebind:1"}}),
        )
        .await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    let (_, original) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            is_server_request(v, "_sudocode/subagent/completed")
        })
        .await
        .unwrap();
    // Replay rebinds the transport while its old request is awaiting a receipt.
    let (during, replay) = client
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert!(replay.get("result").is_some(), "{replay}");
    let rebound = match during
        .into_iter()
        .find(|v| is_server_request(v, "_sudocode/subagent/completed"))
    {
        Some(request) => request,
        None => {
            client
                .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
                    is_server_request(v, "_sudocode/subagent/completed")
                })
                .await
                .unwrap()
                .1
        }
    };
    assert_eq!(rebound["params"], original["params"]);
    assert_ne!(rebound["id"], original["id"]);
    client.send_raw(&json!({"jsonrpc":"2.0","id":original["id"],"result":{"completionId":"late-wrong-identity","disposition":"stored"}})).await;
    client.send_raw(&json!({"jsonrpc":"2.0","id":rebound["id"],"result":{"completionId":rebound["params"]["completionId"],"disposition":"stored"}})).await;
    let (_, second) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            is_server_request(v, "_sudocode/subagent/completed")
        })
        .await
        .unwrap();
    assert_ne!(
        second["params"]["completionId"],
        rebound["params"]["completionId"]
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":second["id"],"result":{"completionId":"wrong-current-identity","disposition":"stored"}})).await;
    // Wait for the durable receipt boundary before replay rebinds the sender.
    let journal = completion_context_path(&workspace, &sid);
    timeout(Duration::from_secs(10), async {
        loop {
            let state: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
            if state["sources"]
                .as_object()
                .unwrap()
                .values()
                .any(|source| source["blocked"] == "ack_identity_mismatch")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("invalid current ACK must be durably quarantined");
    let (_, inventory) = client
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert_eq!(inventory["result"]["pending"], 0, "{inventory}");
    assert_eq!(
        inventory["result"]["blocked"],
        json!(["ack_identity_mismatch"]),
        "{inventory}"
    );
    assert_eq!(
        inventory["result"]["settled"], 1,
        "late reply must not quarantine the acknowledged result: {inventory}"
    );
    let (_, continued) = client.send_request("session/prompt", json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}streaming_text")}],
        "_meta":{"traceId":"rebind:2","sudocode":{"expectedContext":parent["result"]["_meta"]["sudocode"]["contextRef"]}}})).await;
    assert_eq!(
        continued["result"]["stopReason"], "end_turn",
        "late reply must not close the connection: {continued}"
    );
    client.shutdown().await;
    workspace.cleanup();
}

/// Management uses the same optimistic admission and durable commit as model work.
#[cfg(unix)]
#[tokio::test]
async fn acp_management_and_model_share_completion_admission() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("management-completion-context");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let (_, first) = client
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}streaming_text")}],
        "_meta":{"traceId":"model:1"}}),
        )
        .await;
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let context = first["result"]["_meta"]["sudocode"]["contextRef"].clone();
    let transcript = find_session_transcript(&workspace.root, &sid);
    let journal = transcript.with_extension("completions.json");
    let before_transcript = fs::read(&transcript).unwrap();
    let before_journal = fs::read(&journal).unwrap();
    let model_calls = server.captured_requests().await.len();
    let mut wrong = context.clone();
    wrong["digest"] = json!("stale-context");
    let command = "!printf MANAGEMENT_COMMITTED > managed-result; cat managed-result";
    for text in [command, "/compact"] {
        let (updates, rejected) = client
            .send_request(
                "session/prompt",
                json!({"sessionId":sid,
            "prompt":[{"type":"text","text":text}],
            "_meta":{"traceId":"must-not-start","sudocode":{"expectedContext":wrong}}}),
            )
            .await;
        assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
        assert!(
            updates.is_empty(),
            "rejected management emitted progress: {updates:?}"
        );
        assert_eq!(fs::read(&transcript).unwrap(), before_transcript);
        assert_eq!(fs::read(&journal).unwrap(), before_journal);
        assert!(!workspace.root.join("managed-result").exists());
        assert_eq!(server.captured_requests().await.len(), model_calls);
    }
    let (updates, managed) = client
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
        "prompt":[{"type":"text","text":command}],
        "_meta":{"traceId":"management:1","sudocode":{"expectedContext":context}}}),
        )
        .await;
    assert_eq!(managed["result"]["stopReason"], "end_turn", "{managed}");
    let next = managed["result"]["_meta"]["sudocode"]["contextRef"].clone();
    assert_eq!(next["revision"], context["revision"].as_u64().unwrap() + 1);
    assert_eq!(next["lastTraceId"], "management:1");
    assert_eq!(
        fs::read_to_string(workspace.root.join("managed-result")).unwrap(),
        "MANAGEMENT_COMMITTED"
    );
    assert_eq!(server.captured_requests().await.len(), model_calls);
    let chunks: Vec<_> = updates
        .iter()
        .filter_map(|event| event["params"]["update"]["content"]["text"].as_str())
        .collect();
    assert!(
        chunks
            .iter()
            .any(|text| text.contains("MANAGEMENT_COMMITTED")),
        "{updates:?}"
    );
    let state: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    assert!(state["active_trace"].is_null());
    let (_, continued) = client
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}streaming_text")}],
        "_meta":{"traceId":"model:2","sudocode":{"expectedContext":next}}}),
        )
        .await;
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    assert_eq!(
        continued["result"]["_meta"]["sudocode"]["contextRef"]["revision"],
        3
    );
    let requests = server.captured_requests().await;
    let body: Value = serde_json::from_str(
        &requests
            .iter()
            .rev()
            .find(|request| !request.path.contains("count_tokens"))
            .unwrap()
            .raw_body,
    )
    .unwrap();
    assert!(serde_json::to_string(&body["messages"])
        .unwrap()
        .contains("MANAGEMENT_COMMITTED"));
    client.shutdown().await;
    workspace.cleanup();
}

/// Closing a session must reap its shared driver and stop the wire retry task,
/// releasing the journal writer even while a completion ACK is outstanding.
#[tokio::test]
async fn acp_close_releases_completion_writer_without_killing_the_server() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = subagent_events_workspace("completion-close", &server.base_url());
    let mut owner = spawn_stdio_client_danger(&workspace);
    initialize_with_completions(&mut owner).await;
    let sid = scenario_session_new(&mut owner, &workspace.root).await;
    let (_, response) = owner.send_request("session/prompt", json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}subagent_events_background")}],
        "_meta":{"traceId":"close-with-pending-ack"}
    })).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let (_, pending) = owner
        .recv_until(SUBAGENT_EVENTS_WAIT, |m| {
            m["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap();
    let mut successor = spawn_stdio_client_danger(&workspace);
    initialize_with_completions(&mut successor).await;
    let (_, closed) = owner
        .send_request("session/close", json!({"sessionId":sid}))
        .await;
    assert!(closed.get("result").is_some(), "{closed}");
    let (_, replay) = successor
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert!(
        replay.get("result").is_some(),
        "close must release the writer before responding: {replay}"
    );
    assert!(replay["result"]["pending"].as_u64().unwrap() >= 1);
    owner.send_raw(&json!({"jsonrpc":"2.0","id":pending["id"],"result":{"completionId":pending["params"]["completionId"],"disposition":"stored"}})).await;
    let (_, listed) = owner.send_request("session/list", json!({})).await;
    assert_eq!(
        listed["result"]["sessions"],
        json!([]),
        "closing one session must leave its ACP server usable"
    );
    owner.shutdown().await;
    successor.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn acp_completions_sync_and_nested_do_not_auto_publish() {
    let server = MockAnthropicService::spawn().await.unwrap();
    for scenario in ["subagent_events_sync", "subagent_events_nested"] {
        let workspace = subagent_events_workspace("completions-sync", &server.base_url());
        let mut client = spawn_stdio_client_danger(&workspace);
        initialize_with_completions(&mut client).await;
        let sid = scenario_session_new(&mut client, &workspace.root).await;
        client
            .send_request(
                "_sudocode/subagent/replay",
                json!({"sessionId":sid,"cwd":workspace.root}),
            )
            .await;
        let (updates, response) = client
            .send_request(
                "session/prompt",
                json!({"sessionId":sid,
                    "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}{scenario}")}],
                    "_meta":{"traceId":"run-1:1"}
                }),
            )
            .await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert!(updates
            .iter()
            .all(|v| v["method"] != "_sudocode/subagent/completed"));
        let (_, replay) = client
            .send_request(
                "_sudocode/subagent/replay",
                json!({"sessionId":sid,"cwd":workspace.root}),
            )
            .await;
        assert_eq!(replay["result"]["pending"], 0, "{replay}");
        assert_eq!(
            replay["result"]["settled"], 1,
            "only the root's direct spawn is registered: {replay}"
        );
        client.shutdown().await;
        workspace.cleanup();
    }
}

// ---------------------------------------------------------------------------
// Zone-contract subprocess scope (review acceptance: "real subprocess
// scope-denial coverage")
// ---------------------------------------------------------------------------

/// Like [`spawn_stdio_client_danger`] but injects the subprocess runner env
/// (`NEXUS_ZONE_ID` + `NEXUS_RESOURCE_SCOPE`) after `isolate_env`'s
/// `env_clear`, standing in for the zone metadata a moss runner manifest
/// would plant. `danger-full-access` keeps the tool layer out of the way so
/// a denial can only come from the zone contract.
fn spawn_stdio_client_danger_zone_env(
    workspace: &TestWorkspace,
    zone_id: &str,
    resource_scope: &str,
) -> AcpTestClient {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.current_dir(&workspace.root);
    isolate_env(&mut cmd, workspace);
    cmd.args([
        "--auth",
        "api-key",
        "--model",
        "sonnet",
        "--permission-mode",
        "danger-full-access",
        "acp",
    ])
    .env("NEXUS_ZONE_ID", zone_id)
    .env("NEXUS_RESOURCE_SCOPE", resource_scope)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn scode acp stdio (danger+zone)");
    let stdin = child.stdin.take().expect("stdin should be piped");
    let stdout = child.stdout.take().expect("stdout should be piped");

    AcpTestClient {
        transport: Transport::Stdio {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        },
        next_id: 1,
        last_available_commands: None,
        raw_lines: Vec::new(),
    }
}

/// The host-injected runner env must shape the CLI's file authorization
/// (review finding 1): a real subprocess with a scoped zone writes inside
/// its scope and is refused outside it, while the same operation in a
/// genuinely env-less subprocess succeeds — the standalone-local exception
/// is limited to that absence, and the refusal is attributable to the zone
/// layer (the `ZoneAuthError::OutOfScope` message), not the tool layer.
///
/// The scope prefix is computed over the CANONICALIZED workspace root: the
/// child canonicalizes the session cwd (`canonical_session_cwd`) before
/// booting its kernel, and `path_is_within` is a plain string prefix match —
/// an un-canonicalized `/tmp`-style path would diverge on macOS
/// (`/private/tmp`) and 8.3-short-name Windows temp dirs.
///
/// Mock scenarios are fixed to write `generated/output.txt` /
/// `generated/denied.txt`, so in-scope vs out-of-scope is chosen by the
/// prefix the scope grants: `…/generated` includes the writes; `…/allowed`
/// excludes them.
#[tokio::test]
async fn zone_scope_env_shapes_cli_subprocess_file_authorization() {
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");

    let scope_json_for = |granted_prefix: &str| {
        serde_json::json!({
            "schema_version": 1,
            "zone_id": "zone-scope-e2e",
            "rules": [
                {"capability": "zone.data.read", "resource_prefixes": [granted_prefix]},
                {"capability": "zone.data.write", "resource_prefixes": [granted_prefix]}
            ]
        })
        .to_string()
    };

    // Group 1 — in scope: the write under the granted prefix succeeds.
    {
        let workspace = TestWorkspace::new("zone-in-scope");
        workspace.create();
        workspace.write_sudocode_json(&server.base_url());
        let canonical = fs::canonicalize(&workspace.root).expect("canonicalize root");
        let ws_vfs = runtime::vfs_path_for_host_path(&canonical).expect("workspace VFS prefix");
        let scope = scope_json_for(&format!("{ws_vfs}/generated"));

        let mut client = spawn_stdio_client_danger_zone_env(&workspace, "zone-scope-e2e", &scope);
        scenario_initialize(&mut client).await;
        let session = scenario_session_new(&mut client, &workspace.root).await;
        let (text, tools) = run_scenario_collect_text(
            &mut client,
            &session,
            "write_file_allowed",
            Duration::from_secs(30),
            "zone in-scope write",
        )
        .await;
        assert!(
            text.contains("write_file succeeded"),
            "in-scope write must succeed: text {text:?} tools {tools:?}"
        );
        assert!(
            workspace.root.join("generated/output.txt").is_file(),
            "in-scope write must land on disk"
        );
        client.shutdown().await;
        workspace.cleanup();
    }

    // Group 2 — out of scope: the identical write shape is refused, nothing
    // lands, and the refusal reason is the zone layer's OutOfScope.
    {
        let workspace = TestWorkspace::new("zone-out-of-scope");
        workspace.create();
        workspace.write_sudocode_json(&server.base_url());
        let canonical = fs::canonicalize(&workspace.root).expect("canonicalize root");
        let ws_vfs = runtime::vfs_path_for_host_path(&canonical).expect("workspace VFS prefix");
        let scope = scope_json_for(&format!("{ws_vfs}/allowed"));

        let mut client = spawn_stdio_client_danger_zone_env(&workspace, "zone-scope-e2e", &scope);
        scenario_initialize(&mut client).await;
        let session = scenario_session_new(&mut client, &workspace.root).await;
        let (text, tools) = run_scenario_collect_text(
            &mut client,
            &session,
            "write_file_denied",
            Duration::from_secs(30),
            "zone out-of-scope write",
        )
        .await;
        assert!(
            text.contains("write_file denied as expected"),
            "out-of-scope write must be denied: text {text:?} tools {tools:?}"
        );
        assert!(
            !workspace.root.join("generated/denied.txt").exists(),
            "out-of-scope write must not land on disk"
        );
        let tool_text = tools
            .iter()
            .map(|tool| tool.to_string())
            .collect::<String>();
        assert!(
            format!("{text}{tool_text}").contains("is outside runtime scope"),
            "the refusal must be the zone layer's OutOfScope, not the tool \
             layer's: text {text:?} tools {tool_text:?}"
        );
        client.shutdown().await;
        workspace.cleanup();
    }

    // Group 3 — control, genuinely env-less: the same write the zone refused
    // succeeds, proving the refusal came from the env-shaped authorization
    // and not the tool layer or the mock. The mock's closing text is fixed
    // ("denied as expected") regardless of the real outcome, so only the
    // file on disk is the evidence here.
    {
        let workspace = TestWorkspace::new("zone-env-absent");
        workspace.create();
        workspace.write_sudocode_json(&server.base_url());

        let mut client = spawn_stdio_client_danger(&workspace);
        scenario_initialize(&mut client).await;
        let session = scenario_session_new(&mut client, &workspace.root).await;
        let _ = run_scenario_collect_text(
            &mut client,
            &session,
            "write_file_denied",
            Duration::from_secs(30),
            "env-less control write",
        )
        .await;
        assert!(
            workspace.root.join("generated/denied.txt").is_file(),
            "env-less control write must land on disk"
        );
        client.shutdown().await;
        workspace.cleanup();
    }
}

async fn initialize_task_completions(client: &mut AcpTestClient) {
    let (_, response) = client.send_request("initialize", json!({"protocolVersion":1,"clientCapabilities":{"_meta":{"sudocode":{"taskCompletions":{"version":1},"subagentCompletions":{"version":1}}}}})).await;
    assert_eq!(
        response["result"]["_meta"]["sudocode"]["taskCompletions"],
        json!({"version":1}),
        "{response}"
    );
    assert!(response["result"]["_meta"]["sudocode"]
        .get("subagentCompletions")
        .is_none());
}

/// Independent ACP processes retain completion ownership while they append to
/// one SID, including a provider call ID reused within the first process.
#[tokio::test]
async fn acp_shared_sid_scopes_background_results_and_protects_live_contexts() {
    let server = MockAnthropicService::spawn().await.unwrap();
    // Resolve before isolate_env replaces Unix PATH with /usr/bin:/bin.
    let node = std::process::Command::new("node")
        .args(["-p", "process.execPath"])
        .output()
        .expect("Node.js is required for the terminal completion fixture");
    assert!(node.status.success(), "{node:?}");
    let node = String::from_utf8(node.stdout).expect("Node executable path is UTF-8");
    let node = node.trim();
    assert!(Path::new(node).is_absolute(), "{node}");
    let node = if cfg!(windows) {
        format!("& '{}'", node.replace('\'', "''"))
    } else {
        format!("'{}'", node.replace('\'', "'\"'\"'"))
    };
    let workspace = TestWorkspace::new("shared-sid-completion-owners");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    fs::write(
        workspace.root.join("completion-gate.cjs"),
        r#"
const fs = require('node:fs');
const label = process.argv[2];
fs.writeFileSync(label + '.ready', label);
const timer = setInterval(() => {
  if (!fs.existsSync(label + '.release')) return;
  clearInterval(timer);
  fs.writeFileSync(label + '.effect', label + ' durable effect');
  process.stdout.write(label + ' completed');
}, 10);
"#,
    )
    .unwrap();
    let spawn = || {
        spawn_stdio_client_with_args_and_env(
            &workspace,
            &["--permission-mode", "danger-full-access"],
            &[("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1")],
        )
    };
    let mut producer = spawn();
    initialize_task_completions(&mut producer).await;
    let sid = scenario_session_new(&mut producer, &workspace.root).await;
    let (_, previous) = producer
        .send_request(
            "session/prompt",
            json!({
                "sessionId":sid,"_meta":{"traceId":"legacy-prefix"},"prompt":[{"type":"text",
                "text":format!("LEGACY_SHARED_PREFIX {SCENARIO_PREFIX}streaming_text")}]
            }),
        )
        .await;
    assert_eq!(previous["result"]["stopReason"], "end_turn", "{previous}");
    let context = previous["result"]["_meta"]["sudocode"]["contextRef"].clone();
    producer.shutdown().await;
    let previous_context = context;
    let mut first = spawn();
    initialize_task_completions(&mut first).await;
    let (_, migrated) = first
        .send_request(
            "session/load",
            json!({
                "sessionId":sid,"cwd":workspace.root,"mcpServers":[]
            }),
        )
        .await;
    assert!(migrated.get("error").is_none(), "{migrated}");
    let migrated_context = &migrated["result"]["_meta"]["sudocode"]["contextRef"];
    assert_eq!(
        migrated_context["revision"], previous_context["revision"],
        "{migrated}"
    );
    assert_eq!(
        migrated_context["lastTraceId"], previous_context["lastTraceId"],
        "{migrated}"
    );
    let mut second = spawn();
    initialize_task_completions(&mut second).await;
    let (_, loaded) = second
        .send_request(
            "session/load",
            json!({
                "sessionId":sid,"cwd":workspace.root,"mcpServers":[]
            }),
        )
        .await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let storage = loaded["result"]["_meta"]["sudocode"]["storageRef"].clone();
    assert!(storage.is_string(), "{loaded}");
    for label in ["A1", "B1", "A2"] {
        let client = if label == "B1" {
            &mut second
        } else {
            &mut first
        };
        let shell = if cfg!(windows) { "PowerShell" } else { "Bash" };
        let calls = json!([{"id":"call_1","name":shell,"input":{
            "command":format!("{node} completion-gate.cjs {label}"),"run_in_background":true
        }}]);
        let (events, response) = client
            .send_request(
                "session/prompt",
                json!({
                    "sessionId":sid,"_meta":{"traceId":label},"prompt":[{"type":"text",
                    "text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]
                }),
            )
            .await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
        assert!(
            events
                .iter()
                .all(|event| event["method"] != "_sudocode/task/completed"),
            "{label} completed before its release gate: {events:?}"
        );
        timeout(Duration::from_secs(10), async {
            while !workspace.root.join(format!("{label}.ready")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background process reached its gate");
    }
    let (_, rejected) = first
        .send_request(
            "_sudocode/task/purge",
            json!({
                "sessionId":sid,"cwd":workspace.root,"storageRef":storage,"cleanupId":"live-peer"
            }),
        )
        .await;
    assert!(
        rejected["error"]
            .to_string()
            .contains("session_writer_busy"),
        "{rejected}"
    );
    for label in ["B1", "A2", "A1"] {
        let client = if label == "B1" {
            &mut second
        } else {
            &mut first
        };
        fs::write(workspace.root.join(format!("{label}.release")), "release").unwrap();
        let (_, event) = client
            .recv_until(SUBAGENT_EVENTS_WAIT, |event| {
                event["method"] == "_sudocode/task/completed"
            })
            .await
            .expect("durable terminal notification");
        assert_eq!(event["params"]["originTraceId"], label, "{event}");
        assert_eq!(event["params"]["parentToolCallId"], "call_1", "{event}");
        assert!(
            event["params"]["message"].as_str().unwrap().contains(label),
            "{event}"
        );
        assert_eq!(
            fs::read_to_string(workspace.root.join(format!("{label}.effect"))).unwrap(),
            format!("{label} durable effect")
        );
        client
            .send_raw(&json!({"jsonrpc":"2.0","id":event["id"],"result":{
                "completionId":event["params"]["completionId"],"disposition":"stored"
            }}))
            .await;
    }
    for (client, expected) in [(&mut first, 2), (&mut second, 1)] {
        let (_, status) = client
            .send_request(
                "_sudocode/task/replay",
                json!({
                    "sessionId":sid,"cwd":workspace.root,"storageRef":storage
                }),
            )
            .await;
        assert_eq!(status["result"]["settled"], expected, "{status}");
        assert_eq!(status["result"]["pending"], 0, "{status}");
    }
    first.shutdown().await;
    second.shutdown().await;
    let mut restored = spawn();
    initialize_task_completions(&mut restored).await;
    let (_, loaded) = restored
        .send_request(
            "session/load",
            json!({
                "sessionId":sid,"cwd":workspace.root,"mcpServers":[]
            }),
        )
        .await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let (_, status) = restored
        .send_request(
            "_sudocode/task/replay",
            json!({
                "sessionId":sid,"cwd":workspace.root,"storageRef":storage
            }),
        )
        .await;
    assert_eq!(
        status["result"]["settled"], 2,
        "latest context restores A's journal: {status}"
    );
    let (_,purged) = restored.send_request("_sudocode/task/purge",json!({
        "sessionId":sid,"cwd":workspace.root,"storageRef":storage,"cleanupId":"finished-contexts"
    })).await;
    assert_eq!(purged["result"]["disposition"], "purged", "{purged}");
    restored.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn acp_task_storage_and_workspace_metadata_coexist() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("task-workspace-metadata");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let (_, created) = client
        .send_request("session/new", json!({"cwd":workspace.root,"mcpServers":[]}))
        .await;
    let sid = created["result"]["sessionId"].as_str().unwrap();
    let metadata = &created["result"]["_meta"]["sudocode"];
    let storage = metadata["storageRef"]
        .as_str()
        .expect("task storage identity");
    let paths = metadata["workspace"].clone();
    assert!(paths["fileRoot"].as_str().is_some(), "{created}");
    assert!(paths["shellRoot"].as_str().is_some(), "{created}");
    let (_, closed) = client
        .send_request("session/close", json!({"sessionId":sid}))
        .await;
    assert!(closed.get("result").is_some(), "{closed}");
    client.shutdown().await;
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert_eq!(
        loaded["result"]["_meta"]["sudocode"]["workspace"], paths,
        "{loaded}"
    );
    let (_, replay) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
        )
        .await;
    assert_eq!(replay["result"]["storageRef"], storage, "{replay}");
    assert_eq!(replay["result"]["pending"], 0, "{replay}");
    assert!(server.captured_requests().await.is_empty());
    client.shutdown().await;
    workspace.cleanup();
}

/// Real shell + ACP boundary: durable result, ACK retry, cold query, suppression
/// and deletion all use one storage identity. The model transport is deterministic.
#[cfg(unix)]
#[tokio::test]
async fn acp_task_completions_terminal_replay_and_purge() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("task-completions");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let (_, probe) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    let storage = probe["result"]["storageRef"]
        .as_str()
        .expect("storage identity")
        .to_owned();
    let mut competitor = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut competitor).await;
    let (_, busy) = competitor
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert!(
        busy["error"].to_string().contains("session_writer_busy"),
        "{busy}"
    );
    competitor.shutdown().await;
    let calls = json!([{"id":"background-1","name":"Bash","input":{"command":"while [ ! -f release-task ]; do sleep 0.05; done; printf 'TASK_DONE<&>'; exit 7","run_in_background":true}}]);
    let (during, parent) = client.send_request("session/prompt", json!({"sessionId":sid,"_meta":{"traceId":"root:1"},"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]})).await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    assert!(during
        .iter()
        .all(|v| v["method"] != "_sudocode/task/completed"));
    fs::write(workspace.root.join("release-task"), "go").unwrap();
    let (_, first) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/task/completed"
        })
        .await
        .unwrap();
    assert_eq!(first["params"]["kind"], "terminal", "{first}");
    assert_eq!(first["params"]["terminal"]["exitCode"], 7);
    assert_eq!(first["params"]["terminal"]["scope"], "command");
    assert_eq!(first["params"]["status"], "failed");
    assert_eq!(first["params"]["originTraceId"], "root:1");
    assert_eq!(first["params"]["parentToolCallId"], "background-1");
    assert!(first["params"]["message"]
        .as_str()
        .unwrap()
        .contains("TASK_DONE&lt;&amp;&gt;"));
    client.send_raw(&json!({"jsonrpc":"2.0","id":first["id"],"error":{"code":-32603,"message":"receipt unavailable"}})).await;
    let (_, retry) = client
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/task/completed"
        })
        .await
        .unwrap();
    assert_eq!(retry["params"], first["params"]);
    // Leave the receipt pending across process exit. Cold replay owns the
    // journal first; session/load must adopt that writer before engine writes.
    client.shutdown().await;
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let (notifications, replay) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
        )
        .await;
    assert_eq!(replay["result"]["pending"], 1, "{replay}");
    let pending = if let Some(value) = notifications
        .into_iter()
        .find(|v| v["method"] == "_sudocode/task/completed")
    {
        value
    } else {
        client
            .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
                v["method"] == "_sudocode/task/completed"
            })
            .await
            .unwrap()
            .1
    };
    assert_eq!(pending["params"], first["params"]);
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert!(
        loaded.get("error").is_none(),
        "pending replay must share its writer: {loaded}"
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":pending["id"],"result":{"completionId":pending["params"]["completionId"],"disposition":"stored"}})).await;
    let (_, suppressed) = client.send_request("_sudocode/task/suppress", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"originTraceId":"root:1"})).await;
    assert_eq!(
        suppressed["result"]["disposition"], "suppressed",
        "{suppressed}"
    );
    let (_, repeated) = client.send_request("_sudocode/task/suppress", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"originTraceId":"root:1"})).await;
    assert_eq!(
        repeated["result"], suppressed["result"],
        "no-op suppression must keep the committed watermark"
    );
    let foreground =
        json!([{"id":"foreground-1","name":"Bash","input":{"command":"printf FOREGROUND_DONE"}}]);
    let (events, parent) = client.send_request("session/prompt", json!({"sessionId":sid,"_meta":{"traceId":"root:2"},"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{foreground}")}]})).await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    assert!(events
        .iter()
        .all(|v| v["method"] != "_sudocode/task/completed"));
    client.shutdown().await;
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let (_, replay) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
        )
        .await;
    assert_eq!(replay["result"]["unresolved"], 0, "{replay}");
    assert_eq!(
        replay["result"]["terminalSources"][0]["stdout"],
        "TASK_DONE<&>"
    );
    assert_eq!(replay["result"]["terminalSources"][0]["exit_code"], 7);
    let (_, rejected) = client.send_request("_sudocode/task/purge", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":"wrong","cleanupId":"delete-1"})).await;
    assert!(rejected.get("error").is_some());
    let (_, purged) = client.send_request("_sudocode/task/purge", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"cleanupId":"delete-1"})).await;
    assert_eq!(purged["result"]["disposition"], "purged", "{purged}");
    client.shutdown().await;
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let (_, duplicate) = client.send_request("_sudocode/task/purge", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"cleanupId":"delete-1"})).await;
    assert_eq!(duplicate["result"], purged["result"]);
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert!(
        loaded.get("error").is_some(),
        "deleted session must not reopen: {loaded}"
    );
    client.shutdown().await;
    workspace.cleanup();
}

/// A real child outlives scode: replay preserves uncertainty and never reruns it.
#[cfg(unix)]
#[tokio::test]
async fn acp_task_completions_interrupted_shell_and_encoded_budget() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("task-interrupted");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let (_, probe) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    let storage = probe["result"]["storageRef"].as_str().unwrap().to_owned();
    let calls = json!([
        {"id":"uncertain-tool","name":"Bash","input":{"command":"printf once >> executions; i=0; while [ ! -f release-unknown ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i+1)); done; printf exited > unknown-exited", "run_in_background":true}},
        {"id":"large-tool","name":"Bash","input":{"command":"i=0; while [ $i -lt 8000 ]; do printf '&中'; i=$((i+1)); done; printf END", "run_in_background":true}}
    ]);
    let (events, parent) = client.send_request("session/prompt", json!({"sessionId":sid,"_meta":{"traceId":"root:1"},"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]})).await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    let completion = if let Some(value) = events
        .into_iter()
        .find(|v| v["method"] == "_sudocode/task/completed")
    {
        value
    } else {
        client
            .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
                v["method"] == "_sudocode/task/completed"
            })
            .await
            .unwrap()
            .1
    };
    assert_eq!(completion["params"]["parentToolCallId"], "large-tool");
    assert!(serde_json::to_vec(&completion["params"]).unwrap().len() <= 65536);
    assert_eq!(completion["params"]["terminal"]["outputTruncated"], true);
    assert!(completion["params"]["message"]
        .as_str()
        .unwrap()
        .contains("&amp;中"));
    client.send_raw(&json!({"jsonrpc":"2.0","id":completion["id"],"result":{"completionId":completion["params"]["completionId"],"disposition":"stored"}})).await;
    assert_eq!(
        fs::read_to_string(workspace.root.join("executions")).unwrap(),
        "once"
    );
    let (_, suppressed) = client.send_request("_sudocode/task/suppress", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"originTraceId":"root:1"})).await;
    assert_eq!(
        suppressed["result"]["disposition"], "suppressed",
        "{suppressed}"
    );
    let (_, live) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
        )
        .await;
    assert_eq!(live["result"]["unresolved"], 0, "{live}");
    assert_eq!(
        live["result"]["running"], 1,
        "suppress must leave the live shell running: {live}"
    );
    client.shutdown().await;
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    // No deliverable completion keeps a cold route alive here. Repeated
    // controls must cross its engine/writer teardown, without a test sleep or
    // retry masking a transient same-process session_writer_busy.
    for _ in 0..32 {
        let (_, replay) = client
            .send_request(
                "_sudocode/task/replay",
                json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
            )
            .await;
        assert_eq!(replay["result"]["executionUnconfirmed"], 1, "{replay}");
        let (_, suppressed) = client.send_request("_sudocode/task/suppress", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"originTraceId":"root:1"})).await;
        assert_eq!(
            suppressed["result"]["disposition"], "suppressed",
            "{suppressed}"
        );
        let (_, replay) = client
            .send_request(
                "_sudocode/task/replay",
                json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
            )
            .await;
        assert_eq!(replay["result"]["unresolved"], 0, "{replay}");
        assert_eq!(
            replay["result"]["executionUnconfirmed"], 1,
            "suppression is not termination: {replay}"
        );
    }
    fs::write(workspace.root.join("release-unknown"), "go").unwrap();
    timeout(Duration::from_secs(5), async {
        while !workspace.root.join("unknown-exited").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the original child must exit without being rerun");
    assert_eq!(
        fs::read_to_string(workspace.root.join("executions")).unwrap(),
        "once"
    );
    client.shutdown().await;

    // Acquiring a v2 lock does not prove an old v1 callback cannot overwrite it.
    let journal = find_session_transcript(&workspace.root, &sid).with_extension("completions.json");
    let mut legacy: Value = serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    legacy["version"] = json!(1);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    fs::write(&journal, &bytes).unwrap();
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let (_, rejected) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert!(
        rejected["error"]
            .to_string()
            .contains("legacy_writer_unconfirmed"),
        "{rejected}"
    );
    assert_eq!(fs::read(&journal).unwrap(), bytes);
    client.shutdown().await;
    workspace.cleanup();
}

/// Concurrent ACP contexts append to the same SID while retaining separate
/// completion ownership. A continuation repairs only its frozen inherited tail.
#[tokio::test]
async fn acp_completions_keep_live_contexts_independent_during_replay_and_load() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("completion-writer");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let spawn = || {
        spawn_stdio_client_with_args_and_env(
            &workspace,
            &["--permission-mode", "danger-full-access"],
            &[("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1")],
        )
    };
    let mut owner = spawn();
    initialize_with_completions(&mut owner).await;
    let sid = scenario_session_new(&mut owner, &workspace.root).await;
    owner.send_request_no_wait("session/prompt", json!({
        "sessionId":sid, "_meta":{"traceId":"live-owner"},
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}ask_user_question_roundtrip")}]
    })).await;
    owner
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "_scode/ask_user_question")
        })
        .await
        .unwrap();
    let transcript = find_session_transcript(&workspace.root, &sid);
    let read_transcript = || {
        runtime::FsBackend::read_snapshot(&runtime::StdFsBackend, &transcript.to_string_lossy())
            .unwrap()
    };
    let journal = completion_context_path(&workspace, &sid);
    // The question can reach ACP before the provider's final usage update is
    // persisted. Wait for that durable boundary while the question keeps the
    // turn open; otherwise the owner itself can change our byte snapshot.
    timeout(Duration::from_secs(10), async {
        loop {
            let saved = String::from_utf8(read_transcript()).unwrap();
            let graph = runtime::history::HistoryGraph::decode(saved.as_bytes()).unwrap();
            let record: runtime::history::HistoryRecord =
                serde_json::from_str(saved.lines().last().unwrap()).unwrap();
            if graph
                .context_session(record.context_id())
                .unwrap()
                .messages()
                .iter()
                .any(|message| {
                    message
                        .usage
                        .as_ref()
                        .is_some_and(|usage| usage.output_tokens > 0)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("owner persisted the final provider response before waiting for the user");
    let before_transcript = read_transcript();
    let before_journal = fs::read(&journal).unwrap();
    let state: Value = serde_json::from_slice(&before_journal).unwrap();
    assert_eq!(state["active_trace"], "live-owner");
    assert_eq!(state["version"], 3);

    let mut competitor = spawn();
    initialize_with_completions(&mut competitor).await;
    for method in ["_sudocode/subagent/replay", "session/load"] {
        let mut params = json!({"sessionId":sid, "cwd":workspace.root});
        if method == "session/load" {
            params["mcpServers"] = json!([]);
        }
        let (_, response) = competitor.send_request(method, params).await;
        assert!(response.get("result").is_some(), "{response}");
        assert!(
            fs::read(&journal).unwrap() == before_journal,
            "competing {method} changed journal"
        );
        assert!(
            read_transcript().starts_with(&before_transcript),
            "competing {method} replaced durable history"
        );
    }
    let contexts = fs::read_dir(transcript.with_extension("completions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .count();
    assert!(
        contexts >= 2,
        "each live writer must own a separate context"
    );
    // Kill + wait proves exit; no timeout substitutes for process ownership.
    owner.shutdown().await;
    let (_, replay) = competitor
        .send_request(
            "_sudocode/subagent/replay",
            json!({
                "sessionId":sid, "cwd":workspace.root
            }),
        )
        .await;
    assert!(replay.get("result").is_some(), "{replay}");
    assert!(
        read_transcript().starts_with(&before_transcript),
        "replay replaced durable history"
    );
    assert_eq!(fs::read(&journal).unwrap(), before_journal);
    let (_, loaded) = competitor
        .send_request(
            "session/load",
            json!({
                "sessionId":sid, "cwd":workspace.root, "mcpServers":[]
            }),
        )
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    assert!(
        read_transcript().starts_with(&before_transcript),
        "loading must preserve the original interrupted history"
    );
    let (_, continued) = competitor
        .send_request(
            "session/prompt",
            json!({
                "sessionId":sid, "_meta":{"traceId":"new-owner"},
                "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}streaming_text")}]
            }),
        )
        .await;
    assert_eq!(continued["result"]["stopReason"], "end_turn", "{continued}");
    let continued_transcript = read_transcript();
    assert!(
        String::from_utf8_lossy(&continued_transcript)
            .contains("Side effects may already have occurred"),
        "the continuation must retain its inherited tool's unknown outcome"
    );
    let requests = server.captured_requests().await;
    let request = &requests.last().unwrap().raw_body;
    assert!(
        request.contains("Side effects may already have occurred")
            && request.contains(&format!("{SCENARIO_PREFIX}ask_user_question_roundtrip")),
        "the resumed model must inherit the written turn and its recovery result: {request}"
    );
    competitor.shutdown().await;
    assert_eq!(fs::read(&journal).unwrap(), before_journal);
    let mut competitor = spawn();
    initialize_with_completions(&mut competitor).await;
    let (_, loaded_again) = competitor
        .send_request(
            "session/load",
            json!({"sessionId":sid, "cwd":workspace.root, "mcpServers":[]}),
        )
        .await;
    assert!(loaded_again.get("result").is_some(), "{loaded_again}");
    assert!(
        read_transcript().starts_with(&continued_transcript),
        "a further restart must retain both contexts and append history"
    );
    competitor.shutdown().await;

    // Unreleased, non-locking v1 records require explicit offline handling.
    let binding = transcript.with_extension("completions.json");
    let mut legacy: Value = serde_json::from_slice(&fs::read(&binding).unwrap()).unwrap();
    legacy["version"] = json!(1);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    fs::write(&binding, &bytes).unwrap();
    let mut reader = spawn();
    initialize_with_completions(&mut reader).await;
    let (_, response) = reader
        .send_request(
            "_sudocode/subagent/replay",
            json!({
                "sessionId":sid, "cwd":workspace.root
            }),
        )
        .await;
    assert!(
        response["error"]
            .to_string()
            .contains("legacy_writer_unconfirmed"),
        "{response}"
    );
    assert_eq!(fs::read(&binding).unwrap(), bytes);
    reader.shutdown().await;
    workspace.cleanup();
}

/// Run explicitly with SCODE_PREVIOUS_BIN pointing to the stage-one build.
/// Both real executables use the same files; no synthetic format conversion.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires a separately built stage-one binary in SCODE_PREVIOUS_BIN"]
async fn acp_completion_upgrade_and_rollback_preserve_delivery() {
    let previous = PathBuf::from(std::env::var_os("SCODE_PREVIOUS_BIN").expect("stage-one binary"));
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = subagent_events_workspace("completion-upgrade", &server.base_url());
    let mut old = spawn_stdio_client_danger_binary(&workspace, &previous);
    initialize_with_completions(&mut old).await;
    let sid = scenario_session_new(&mut old, &workspace.root).await;
    let (_, parent) = old
        .send_request(
            "session/prompt",
            json!({"sessionId":sid,
        "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}subagent_events_background")}],
        "_meta":{"traceId":"upgrade:1"}}),
        )
        .await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    let (_, first) = old
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap();
    let transcript = find_session_transcript(&workspace.root, &sid);
    let path = transcript.with_extension("completions.json");
    let before = frozen_completion_sources(&workspace, &sid, 2).await;
    old.shutdown().await;

    let mut new = spawn_stdio_client_danger(&workspace);
    initialize_with_completions(&mut new).await;
    let (events, replay) = new
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert_eq!(replay["result"]["pending"], 2, "{replay}");
    let retry = if let Some(v) = events
        .into_iter()
        .find(|v| v["method"] == "_sudocode/subagent/completed")
    {
        v
    } else {
        new.recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap()
        .1
    };
    assert_eq!(retry["params"], first["params"]);
    let upgraded: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for field in ["storage_ref", "context", "closed_traces"] {
        assert_eq!(upgraded[field], before[field], "upgrade changed {field}");
    }
    for (id, source) in before["sources"].as_object().unwrap() {
        for (field, value) in source.as_object().unwrap() {
            assert_eq!(
                &upgraded["sources"][id][field], value,
                "upgrade changed source {id}.{field}"
            );
        }
    }
    new.send_raw(&json!({"jsonrpc":"2.0","id":retry["id"],"result":{"completionId":retry["params"]["completionId"],"disposition":"stored"}})).await;
    let (_, second) = new
        .recv_until(SUBAGENT_EVENTS_WAIT, |v| {
            v["method"] == "_sudocode/subagent/completed"
        })
        .await
        .unwrap();
    let expected_second = before["sources"]
        .as_object()
        .unwrap()
        .values()
        .map(|source| &source["payload"])
        .find(|payload| payload["completionId"] != first["params"]["completionId"])
        .unwrap();
    assert_eq!(&second["params"], expected_second);
    new.send_raw(&json!({"jsonrpc":"2.0","id":second["id"],"result":{"completionId":second["params"]["completionId"],"disposition":"stored"}})).await;
    timeout(Duration::from_secs(10), async {
        loop {
            let (_, replay) = new
                .send_request(
                    "_sudocode/subagent/replay",
                    json!({"sessionId":sid,"cwd":workspace.root}),
                )
                .await;
            if replay["result"]["pending"] == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let acknowledged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    new.shutdown().await;

    let mut old = spawn_stdio_client_danger_binary(&workspace, &previous);
    initialize_with_completions(&mut old).await;
    let (_, replay) = old
        .send_request(
            "_sudocode/subagent/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    assert_eq!(replay["result"]["pending"], 0, "{replay}");
    assert_eq!(replay["result"]["settled"], 2, "{replay}");
    let rolled_back: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for (id, source) in before["sources"].as_object().unwrap() {
        for field in source.as_object().unwrap().keys() {
            assert_eq!(
                rolled_back["sources"][id][field], acknowledged["sources"][id][field],
                "rollback changed {id}.{field}"
            );
        }
    }
    assert_eq!(rolled_back["context"], acknowledged["context"]);
    old.shutdown().await;

    let mut new = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut new).await;
    let (_, loaded) = new
        .send_request(
            "session/load",
            json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
        )
        .await;
    assert!(loaded.get("result").is_some(), "{loaded}");
    let calls = json!([{"id":"terminal-upgrade","name":"Bash","input":{"command":"while [ ! -f release-upgrade ]; do sleep 0.05; done; echo TERMINAL_UPGRADE","run_in_background":true}}]);
    let (_, parent) = new.send_request("session/prompt", json!({"sessionId":sid,"_meta":{"traceId":"upgrade:2"},"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]})).await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    fs::write(workspace.root.join("release-upgrade"), "go").unwrap();
    new.recv_until(SUBAGENT_EVENTS_WAIT, |v| {
        v["method"] == "_sudocode/task/completed"
    })
    .await
    .unwrap();
    new.shutdown().await;
    let journal_bytes = fs::read(&path).unwrap();
    let transcript_bytes = fs::read(&transcript).unwrap();
    // Once terminal records exist, stage one cannot represent them. Refuse
    // both replay and transcript writes, even without completion negotiation.
    for negotiated in [true, false] {
        let mut old = spawn_stdio_client_danger_binary(&workspace, &previous);
        if negotiated {
            initialize_with_completions(&mut old).await;
        } else {
            scenario_initialize(&mut old).await;
        }
        if negotiated {
            let (_, rejected) = old
                .send_request(
                    "_sudocode/subagent/replay",
                    json!({"sessionId":sid,"cwd":workspace.root}),
                )
                .await;
            assert!(
                rejected["error"]
                    .to_string()
                    .contains("completion_journal_unsupported"),
                "{rejected}"
            );
        }
        let (_, rejected) = old
            .send_request(
                "session/load",
                json!({"sessionId":sid,"cwd":workspace.root,"mcpServers":[]}),
            )
            .await;
        let rejected = if negotiated {
            rejected
        } else {
            // Unnegotiated load only reads the transcript. Its first attempted
            // write must still reject the unsupported journal before mutation.
            assert!(rejected.get("result").is_some(), "{rejected}");
            old.send_request(
                "session/prompt",
                json!({"sessionId":sid,
                "prompt":[{"type":"text","text":"Do not overwrite newer task state."}]}),
            )
            .await
            .1
        };
        assert!(
            rejected["error"]
                .to_string()
                .contains("completion_journal_unsupported"),
            "{rejected}"
        );
        old.shutdown().await;
        assert!(
            fs::read(&path).unwrap() == journal_bytes,
            "rollback changed unsupported journal"
        );
        assert!(
            fs::read(&transcript).unwrap() == transcript_bytes,
            "rollback changed unsupported transcript"
        );
    }
    workspace.cleanup();
}

/// Purging notifications preserves process ownership until session teardown.
#[cfg(unix)]
#[tokio::test]
async fn acp_purged_background_terminal_is_still_reaped_on_close() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("purged-terminal-ownership");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let _release = CompletionFixtureRelease(workspace.root.join("release-purged"));
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let (_, probe) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    let storage = probe["result"]["storageRef"].clone();
    let calls = json!([{"id":"purged-worker","name":"Bash","input":{"command":"printf '%s' \"$$\" > purged-pid; while [ ! -f release-purged ]; do sleep 0.05; done","run_in_background":true}}]);
    let (_, parent) = client.send_request("session/prompt", json!({"sessionId":sid,"_meta":{"traceId":"purged:1"},"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]})).await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    let pid = timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(pid) = fs::read_to_string(workspace.root.join("purged-pid")) {
                if let Ok(pid) = pid.parse::<i32>() {
                    break nix::unistd::Pid::from_raw(pid);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("real background shell PID");
    let (_, purged) = client.send_request("_sudocode/task/purge", json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage,"cleanupId":"delete-purged-worker"})).await;
    assert_eq!(purged["result"]["disposition"], "purged", "{purged}");
    assert!(
        nix::sys::signal::kill(pid, None).is_ok(),
        "purge only retires notifications"
    );
    let (_, closed) = client
        .send_request("session/close", json!({"sessionId":sid}))
        .await;
    assert!(closed.get("result").is_some(), "{closed}");
    let reaped = timeout(Duration::from_secs(5), async {
        while nix::sys::signal::kill(pid, None) != Err(nix::errno::Errno::ESRCH) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    // Release even on a regression, so this acceptance never leaves its shell.
    fs::write(workspace.root.join("release-purged"), "release").unwrap();
    client.shutdown().await;
    assert!(
        reaped,
        "journal purge discarded ownership before the session could reap its shell"
    );
    workspace.cleanup();
}

/// Query the original fixture identities without treating an absent PID as an error.
#[cfg(windows)]
fn powershell_process_identities(identities: &[String]) -> Vec<String> {
    let ids = identities
        .iter()
        .map(|value| value.split_once(':').unwrap().0)
        .collect::<Vec<_>>()
        .join(",");
    let check = "$ErrorActionPreference = 'Stop'; $ids = @(PROCESS_IDS); Get-Process -ErrorAction Stop | Where-Object { $ids -contains $_.Id -and -not $_.HasExited } | ForEach-Object { '{0}:{1}' -f $_.Id, $_.StartTime.ToUniversalTime().Ticks }".replace("PROCESS_IDS", &ids);
    let output = std::process::Command::new("pwsh")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &check,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "native process query failed ({}): stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_owned())
        .collect()
}

/// Native Windows Job accounting must confirm all generations before publishing cancellation.
#[cfg(windows)]
#[tokio::test]
async fn acp_powershell_cancellation_reaps_owned_job_before_replay() {
    let server = MockAnthropicService::spawn().await.unwrap();
    let workspace = TestWorkspace::new("windows-task-job");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let _release = CompletionFixtureRelease(workspace.root.join("release-native"));
    for (name, next) in [
        ("parent", Some("child")),
        ("child", Some("grandchild")),
        ("grandchild", None),
    ] {
        let stamp = format!(
            r#"$ErrorActionPreference = 'Stop'
$stamp = '{{0}}:{{1}}' -f $PID, (Get-Process -Id $PID).StartTime.ToUniversalTime().Ticks
[System.IO.File]::WriteAllText((Join-Path $PSScriptRoot '{name}.pid.tmp'), $stamp, [System.Text.Encoding]::ASCII)
[System.IO.File]::Move((Join-Path $PSScriptRoot '{name}.pid.tmp'), (Join-Path $PSScriptRoot '{name}.pid'))
"#
        );
        let body = next.map_or_else(
            || "while (-not (Test-Path -LiteralPath (Join-Path $PSScriptRoot 'release-native'))) { Start-Sleep -Milliseconds 50 }\n".to_owned(),
            |next| format!("$shell = (Get-Process -Id $PID).Path\n& $shell -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot '{next}.ps1')\n"),
        );
        fs::write(workspace.root.join(format!("{name}.ps1")), stamp + &body).unwrap();
    }
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let sid = scenario_session_new(&mut client, &workspace.root).await;
    let (_, probe) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root}),
        )
        .await;
    let storage = probe["result"]["storageRef"].clone();
    let calls = json!([{"id":"windows-owned","name":"PowerShell","input":{"command":"Set-ExecutionPolicy -Scope Process Bypass -Force; & ./parent.ps1","run_in_background":true}}]);
    let (started, parent) = client.send_request("session/prompt", json!({"sessionId":sid,"_meta":{"traceId":"windows-owned:1"},"prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}]})).await;
    assert_eq!(parent["result"]["stopReason"], "end_turn", "{parent}");
    let mut last_stamps = Vec::new();
    // Cold-starting three interpreters uses the same startup budget as the
    // server fixture. This does not change the runtime's cancellation deadline.
    let identities = timeout(SERVER_STARTUP_TIMEOUT, async {
        loop {
            let mut identities = Vec::new();
            last_stamps.clear();
            for name in ["parent", "child", "grandchild"] {
                let stamp = fs::read_to_string(workspace.root.join(format!("{name}.pid")));
                last_stamps.push(format!("{name}: {stamp:?}"));
                if let Ok(value) = stamp {
                    let value = value.trim();
                    if value.split_once(':').is_some_and(|(pid, ticks)| {
                        pid.parse::<u32>().is_ok() && ticks.parse::<u64>().is_ok()
                    }) {
                        identities.push(value.to_owned());
                    }
                }
            }
            if identities.len() == 3 {
                break identities;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!("all three native PowerShell processes must start: {error}; stamps={last_stamps:?}; updates={started:?}")
    });
    let running = powershell_process_identities(&identities);
    assert!(
        identities.iter().all(|identity| running.contains(identity)),
        "all three original processes must be alive before close: expected={identities:?}, running={running:?}"
    );
    let (_, closed) = client
        .send_request("session/close", json!({"sessionId":sid}))
        .await;
    assert!(closed.get("result").is_some(), "{closed}");
    let present = powershell_process_identities(&identities);
    assert!(
        identities
            .iter()
            .all(|identity| !present.contains(identity)),
        "close returned while original native processes were present: {present:?}"
    );
    client.shutdown().await;
    let mut client = spawn_stdio_client_danger(&workspace);
    initialize_task_completions(&mut client).await;
    let before = server.captured_requests().await.len();
    let (during, replay) = client
        .send_request(
            "_sudocode/task/replay",
            json!({"sessionId":sid,"cwd":workspace.root,"storageRef":storage}),
        )
        .await;
    assert_eq!(replay["result"]["pending"], 1, "{replay}");
    let completion = match during
        .into_iter()
        .find(|value| is_server_request(value, "_sudocode/task/completed"))
    {
        Some(value) => value,
        None => {
            client
                .recv_until(SUBAGENT_EVENTS_WAIT, |value| {
                    is_server_request(value, "_sudocode/task/completed")
                })
                .await
                .unwrap()
                .1
        }
    };
    assert_eq!(completion["params"]["status"], "cancelled", "{completion}");
    assert_eq!(completion["params"]["terminal"]["shell"], "powershell");
    assert_eq!(completion["params"]["parentToolCallId"], "windows-owned");
    assert_eq!(completion["params"]["originTraceId"], "windows-owned:1");
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "replay must not restart model or process work"
    );
    client.send_raw(&json!({"jsonrpc":"2.0","id":completion["id"],"result":{"completionId":completion["params"]["completionId"],"disposition":"stored"}})).await;
    client.shutdown().await;
    workspace.cleanup();
}

/// Pin the agent name a session started in `dir` answers to.
fn pin_agent_name(dir: &Path, agent: &str) {
    let settings = dir.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings).expect("create project settings");
    fs::write(
        settings.join("settings.json"),
        json!({ "agentName": agent }).to_string(),
    )
    .expect("write project settings");
}

/// A non-hosted ACP process on a machine with no node receives its peers over
/// the same-machine pair root, one receiver per agent it serves.
///
/// Two sessions in two workspaces are two agents. Each peer message reaches
/// the session of the agent it was sent to, never the other one, and names its
/// real sender. Each conversation's read position moves only once the turn the
/// message started has finished.
#[tokio::test]
async fn acp_stdio_receives_peer_messages_on_the_pair_root() {
    const FIRST: &str = "acp-pair-first";
    const SECOND: &str = "acp-pair-second";
    const PEER: &str = "acp-pair-peer";
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("stdio-pair-receive");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let second_dir = workspace.root.join("second");
    fs::create_dir_all(&second_dir).expect("create the second workspace");
    pin_agent_name(&workspace.root, FIRST);
    pin_agent_name(&second_dir, SECOND);

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let mut sessions = Vec::new();
    for (dir, agent) in [(&workspace.root, FIRST), (&second_dir, SECOND)] {
        let (_, created) = client
            .send_request(
                "session/new",
                json!({"cwd": dir.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let peer = &created["result"]["_meta"]["sudocode"]["peer"];
        assert_eq!(
            peer["agent"], agent,
            "session/new reports its agent: {created}"
        );
        assert_eq!(
            peer["namespace"], "disk",
            "with no node record the peers are on the pair root: {created}"
        );
        let id = created["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new returned no sessionId: {created}"))
            .to_string();
        sessions.push((agent, id));
    }

    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    let peer = runtime::mailbox::Mailbox::workspace_local(&pair_root, PEER.to_string());
    for (agent, _) in &sessions {
        peer.send(runtime::agent_mailbox::MailboxEnvelope {
            from: PEER.to_string(),
            to: (*agent).to_string(),
            body: format!("for {agent} PARITY_SCENARIO:single_turn_text"),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("the peer writes on the pair root");
    }

    let mut delivered: Vec<Value> = Vec::new();
    while delivered.len() < sessions.len() {
        let (_, update) = client
            .recv_until(Duration::from_secs(30), |msg| {
                msg["method"] == "session/update"
                    && msg["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                    && msg["params"]["_meta"]["sudocode"]["a2a"]["from"] == PEER
            })
            .await
            .unwrap_or_else(|seen| {
                panic!("a peer message never reached the client; saw {seen:#?}")
            });
        delivered.push(update);
    }
    for (agent, id) in &sessions {
        let update = delivered
            .iter()
            .find(|update| update["params"]["sessionId"] == id.as_str())
            .unwrap_or_else(|| {
                panic!("nothing was delivered to {agent}'s session: {delivered:#?}")
            });
        let text = update["params"]["update"]["content"]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.contains(&format!("for {agent} ")),
            "{agent}'s session got another agent's message: {text}"
        );
    }

    for (agent, _) in &sessions {
        let inbox = runtime::mailbox::Mailbox::workspace_local(&pair_root, (*agent).to_string());
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let position = inbox
                .read_position(PEER)
                .expect("read the agent's position")
                .unwrap_or(0);
            if position > 0 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{agent} never recorded the message after its turn finished"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    client.shutdown().await;
    workspace.cleanup();
}

// ---------------------------------------------------------------------------
// Direct A2A requests (`_sudocode/a2a/*`)
// ---------------------------------------------------------------------------

/// Open a session in `dir` and return its id.
async fn open_direct_session(client: &mut AcpTestClient, dir: &Path) -> String {
    let (_, created) = client
        .send_request(
            "session/new",
            json!({"cwd": dir.to_string_lossy(), "mcpServers": []}),
        )
        .await;
    created["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {created}"))
        .to_string()
}

async fn set_session_mode(client: &mut AcpTestClient, session_id: &str, mode: &str) {
    let (_, resp) = client
        .send_request(
            "session/setPermissionMode",
            json!({ "sessionId": session_id, "permissionMode": mode }),
        )
        .await;
    assert!(
        resp.get("error").is_none(),
        "session/setPermissionMode {mode} should succeed: {resp}"
    );
}

/// A definite refusal is invalid params typed `not_submitted`: nothing ran, so
/// it may be resent.
fn assert_refused(resp: &Value, case: &str) {
    assert_eq!(
        resp["error"]["code"], -32602,
        "{case} must be refused before anything runs: {resp}"
    );
    assert_eq!(
        resp["error"]["data"]["sudocode"]["directA2a"],
        json!({ "version": 1, "outcome": "not_submitted" }),
        "{case} is typed as not submitted: {resp}"
    );
}

/// A failure after admission is an internal error typed `unknown`: the call
/// may have run, so it must not be resent blindly.
fn assert_unknown(resp: &Value, case: &str) {
    assert_eq!(
        resp["error"]["code"], -32603,
        "{case} is not a refusal: {resp}"
    );
    assert_eq!(
        resp["error"]["data"]["outcome"], "unknown",
        "{case} says its outcome is unknown: {resp}"
    );
    assert_eq!(
        resp["error"]["data"]["sudocode"]["directA2a"],
        json!({ "version": 1, "outcome": "unknown" }),
        "{case} is typed as unknown: {resp}"
    );
}

/// What `sender` has written to its conversation with `peer` on the pair root,
/// read straight from the transcript so reading it changes nothing.
fn conversation_log(pair_root: &Path, peer: &str, sender: &str) -> String {
    let path = runtime::mailbox::Mailbox::workspace_local(pair_root, peer.to_string())
        .transcript_path(sender);
    fs::read_to_string(path).unwrap_or_default()
}

/// `_sudocode/a2a/send` runs the session's own `send` tool for the client: the
/// message reaches the peer on the pair root from the session's agent, the
/// model is never asked, and the model's history never sees it.
fn assert_direct_host_audit<'a>(
    reply: &'a Value,
    sid: &str,
    actor: &str,
    root: &Path,
    tool: &str,
) -> &'a Value {
    use sha2::{Digest, Sha256};
    let audit = &reply["result"]["_meta"]["sudocode"]["directA2a"]["audit"];
    assert_eq!(audit["version"], 1, "{reply}");
    assert_eq!(
        audit["sessionId"], sid,
        "the loaded native session: {reply}"
    );
    assert_eq!(
        audit["actor"], actor,
        "the executor's actual sender: {reply}"
    );
    assert_eq!(audit["namespaceRoot"], root.to_string_lossy().as_ref());
    assert_eq!(audit["tool"], tool);
    assert!(audit["commandId"]
        .as_str()
        .is_some_and(|id| id.starts_with("external-a2a-")));
    let mut hash = Sha256::new();
    let content = reply["result"]["content"].as_str().unwrap().as_bytes();
    hash.update(b"sudocode.direct-a2a.result.v1\0");
    hash.update([u8::from(reply["result"]["isError"].as_bool().unwrap())]);
    hash.update((content.len() as u64).to_be_bytes());
    hash.update(content);
    assert_eq!(
        audit["resultSha256"],
        format!("{:x}", hash.finalize()),
        "audit binds actual unchanged tool output and isError, before metadata"
    );
    audit
}

fn assert_direct_not_submitted(reply: &Value, case: &str) {
    assert_refused(reply, case);
    let phase = &reply["error"]["data"]["sudocode"]["directA2a"];
    assert_eq!(phase["version"], 1);
    assert_eq!(phase["outcome"], "not_submitted");
}

#[tokio::test]
async fn direct_a2a_send_reaches_the_peer_without_a_model_turn() {
    const AGENT: &str = "direct-a2a-sender";
    const PEER: &str = "direct-a2a-peer";
    const MARKER: &str = "DIRECT_A2A_MARKER_7c1f";
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("direct-a2a-send");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    pin_agent_name(&workspace.root, AGENT);
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);

    let mut client = spawn_stdio_client(&workspace);
    let (_, init) = client
        .send_request("initialize", json!({ "protocolVersion": 1 }))
        .await;
    assert_eq!(
        init["result"]["_meta"]["sudocode"]["directA2a"],
        json!({ "version": 1, "errorOutcomes": 1, "hostAudit": 1 }),
        "initialize advertises the direct A2A requests and their typed outcomes: {init}"
    );
    assert_eq!(
        init["result"]["_meta"]["sudocode"]["directA2a"]["errorOutcomes"],
        1
    );
    assert_eq!(
        init["result"]["_meta"]["sudocode"]["directA2a"]["hostAudit"],
        1
    );
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;

    let before = server.captured_requests().await.len();
    let (_, sent) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({
                "sessionId": sid,
                "to": PEER,
                "message": format!("{MARKER} hello"),
                "summary": "direct probe"
            }),
        )
        .await;
    assert_eq!(
        sent["result"]["isError"], false,
        "the send tool ran: {sent}"
    );
    assert!(
        sent["result"]["content"]
            .as_str()
            .is_some_and(|content| !content.is_empty()),
        "the response carries the tool's own result: {sent}"
    );
    let log = conversation_log(&pair_root, PEER, AGENT);
    assert!(
        log.contains(MARKER) && log.contains(AGENT),
        "the peer's conversation holds the message from {AGENT}: {log}"
    );
    let sent_audit = assert_direct_host_audit(&sent, &sid, AGENT, &pair_root, "send");

    let (_, listed) = client
        .send_request("_sudocode/a2a/agent_list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(
        listed["result"]["isError"], false,
        "agent_list ran: {listed}"
    );
    let rows: Value =
        serde_json::from_str(listed["result"]["content"].as_str().unwrap_or_default())
            .unwrap_or_else(|e| panic!("agent_list returns the tool's JSON ({e}): {listed}"));
    assert!(
        rows.get("agents").is_some() && rows.get("count").is_some(),
        "agent_list returns the tool's own rows: {rows}"
    );
    let list_audit = assert_direct_host_audit(&listed, &sid, AGENT, &pair_root, "agent_list");
    assert_ne!(sent_audit["commandId"], list_audit["commandId"]);
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "a direct call never asks the model"
    );

    // The next real turn's request carries no trace of the direct send.
    let (_, prompted) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}single_turn_text") }]
            }),
        )
        .await;
    assert!(
        prompted.get("error").is_none(),
        "the prompt ran: {prompted}"
    );
    let requests = server.captured_requests().await;
    let last = requests.last().expect("the prompt reached the model");
    assert!(
        !last.raw_body.contains(MARKER),
        "a direct send must not enter the model's history"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Every refusal is definite and leaves the peer untouched: an unknown
/// session, a `sender` or structured message, a mode that forbids sending,
/// paused tools, a session busy with a turn, and a denied permission prompt.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn direct_a2a_refusals_send_nothing() {
    const AGENT: &str = "direct-a2a-refuser";
    const PEER: &str = "direct-a2a-untouched";
    const MARKER: &str = "DIRECT_A2A_REFUSED_3e9a";
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("direct-a2a-refusals");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    pin_agent_name(&workspace.root, AGENT);
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    let send = |session: &str, extra: Value| {
        let mut params =
            json!({ "sessionId": session, "to": PEER, "message": MARKER, "summary": "probe" });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        params
    };

    for (case, params, reason) in [
        (
            "an unknown session",
            send("no-such-session", json!({})),
            "unknown_session",
        ),
        (
            "a sender override",
            send(&sid, json!({ "sender": "someone-else" })),
            "invalid_params",
        ),
        (
            "a structured message",
            send(&sid, json!({ "message": { "type": "shutdown_request" } })),
            "invalid_params",
        ),
        (
            "a message without a summary",
            json!({ "sessionId": sid, "to": PEER, "message": MARKER }),
            "invalid_params",
        ),
    ] {
        let (_, resp) = client.send_request("_sudocode/a2a/send", params).await;
        assert!(
            resp.get("error").is_some(),
            "{case} must be refused: {resp}"
        );
        assert_direct_not_submitted(&resp, reason);
    }

    // The session starts read-only: the tool runs and the policy refuses it.
    let (_, resp) = client
        .send_request("_sudocode/a2a/send", send(&sid, json!({})))
        .await;
    assert_eq!(
        resp["result"]["isError"], true,
        "read-only mode refuses the send: {resp}"
    );
    assert!(
        resp["result"]["content"]
            .as_str()
            .is_some_and(|text| text.contains("read-only")),
        "the refusal is the permission policy's: {resp}"
    );

    let (_, paused) = client
        .send_request("_sudocode/session/pause_tools", json!({ "sessionId": sid }))
        .await;
    let epoch = paused["result"]["pauseEpoch"].clone();
    let (_, resp) = client
        .send_request("_sudocode/a2a/send", send(&sid, json!({})))
        .await;
    assert!(resp.get("error").is_some(), "paused tools refuse: {resp}");
    assert_direct_not_submitted(&resp, "paused");
    let (_, resumed) = client
        .send_request(
            "_sudocode/session/resume_tools",
            json!({ "sessionId": sid, "pauseEpoch": epoch }),
        )
        .await;
    assert!(resumed.get("error").is_none(), "resume_tools: {resumed}");

    let (prompt_id, permission_id) = park_session_on_permission_prompt(&mut client, &sid).await;
    let (_, resp) = client
        .send_request("_sudocode/a2a/send", send(&sid, json!({})))
        .await;
    assert!(
        resp.get("error").is_some(),
        "a session busy with a turn refuses at once: {resp}"
    );
    assert_direct_not_submitted(&resp, "busy");
    allow_permission(&mut client, &permission_id).await;
    client
        .recv_until(Duration::from_secs(60), |m| is_response_to(m, prompt_id))
        .await
        .unwrap_or_else(|seen| panic!("the parked turn never finished; saw {seen:#?}"));

    set_session_mode(&mut client, &sid, "prompt").await;
    let id = client
        .send_request_no_wait("_sudocode/a2a/send", send(&sid, json!({})))
        .await;
    let (_, permission) = client
        .recv_until(Duration::from_secs(30), |m| {
            is_server_request(m, "session/request_permission")
        })
        .await
        .unwrap_or_else(|seen| panic!("the direct send never asked; saw {seen:#?}"));
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "id": permission["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "reject_once" } }
        }))
        .await;
    let (_, resp) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, id))
        .await
        .unwrap_or_else(|seen| panic!("the denied send never answered; saw {seen:#?}"));
    assert_eq!(
        resp["result"]["isError"], true,
        "a denied permission prompt refuses the send: {resp}"
    );
    assert!(
        resp["result"]["content"]
            .as_str()
            .is_some_and(|text| text.contains("reject_once")),
        "the refusal is the client's denial: {resp}"
    );

    assert!(
        !conversation_log(&pair_root, PEER, AGENT).contains(MARKER),
        "no refused send may reach the peer"
    );
    client.shutdown().await;
    workspace.cleanup();
}

/// A direct send cancelled after its message went out is not a refusal: the
/// response says the outcome is unknown, the peer holds the message exactly
/// once, the model is never asked, and nothing resends it.
#[tokio::test]
async fn direct_a2a_cancel_after_delivery_reports_unknown() {
    const AGENT: &str = "direct-a2a-cancelled";
    const PEER: &str = "direct-a2a-receiver";
    const MARKER: &str = "DIRECT_A2A_UNKNOWN_5b2d";
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("direct-a2a-unknown");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    // Hold the call in its post-tool hook, after the send itself has run.
    let started = workspace.root.join("direct-post.started");
    let release = workspace.root.join("direct-post.release");
    let hook = if cfg!(windows) {
        r"echo started>direct-post.started& for /L %i in (1,1,60) do @if not exist direct-post.release (ping -n 2 127.0.0.1 >nul)"
    } else {
        r"touch direct-post.started; i=0; while [ ! -f direct-post.release ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done"
    };
    let settings = workspace.root.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings).expect("create project settings");
    fs::write(
        settings.join("settings.json"),
        json!({ "agentName": AGENT, "hooks": { "PostToolUse": [hook] } }).to_string(),
    )
    .expect("write project settings");
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let before = server.captured_requests().await.len();

    let id = client
        .send_request_no_wait(
            "_sudocode/a2a/send",
            json!({ "sessionId": sid, "to": PEER, "message": MARKER, "summary": "probe" }),
        )
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the post-tool hook never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        conversation_log(&pair_root, PEER, AGENT)
            .matches(MARKER)
            .count(),
        1,
        "the send ran before its post-tool hook"
    );
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": sid }
        }))
        .await;
    let answered = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, id))
        .await;
    let _ = fs::write(&release, "");
    let (_, resp) =
        answered.unwrap_or_else(|seen| panic!("the cancelled send never answered; saw {seen:#?}"));
    assert_unknown(&resp, "a call cancelled after it ran");

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        conversation_log(&pair_root, PEER, AGENT)
            .matches(MARKER)
            .count(),
        1,
        "nothing resends a message whose outcome is unknown"
    );
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "a direct call never asks the model"
    );
    client.shutdown().await;
    workspace.cleanup();
}

/// A pause issued while a direct send's worker is still writing waits for that
/// worker: direct calls take the session's operation fence as a turn's tools
/// do. The worker is held mid-write on a full FIFO standing in for the peer's
/// transcript, so it is genuinely in flight when the pause arrives, and control
/// is reported only after the test lets the write finish.
#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn direct_a2a_pause_waits_for_an_in_flight_send() {
    use nix::{libc, sys::stat::Mode, unistd::mkfifo};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;

    const AGENT: &str = "direct-a2a-fenced";
    const PEER: &str = "direct-a2a-held";
    const MARKER: &str = "DIRECT_A2A_FENCED_91c4";
    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("direct-a2a-fence");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    let started = workspace.root.join("direct-pre.started");
    let settings = workspace.root.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings).expect("create project settings");
    fs::write(
        settings.join("settings.json"),
        json!({ "agentName": AGENT, "hooks": { "PreToolUse": ["touch direct-pre.started"] } })
            .to_string(),
    )
    .expect("write project settings");
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);

    // The send appends to this path; as a full pipe it blocks the append.
    let transcript = runtime::mailbox::Mailbox::workspace_local(&pair_root, PEER.to_string())
        .transcript_path(AGENT);
    let transcript = Path::new(&transcript);
    fs::create_dir_all(transcript.parent().expect("transcript dir"))
        .expect("create transcript dir");
    mkfifo(transcript, Mode::S_IRUSR | Mode::S_IWUSR).expect("mkfifo");
    let mut pipe = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(transcript)
        .expect("open the fifo");
    for chunk in [65_536, 4096, 512, 64, 8, 1] {
        let block = vec![b'#'; chunk];
        loop {
            match pipe.write(&block) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill the pipe: {error}"),
            }
        }
    }

    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let before = server.captured_requests().await.len();

    let send_id = client
        .send_request_no_wait(
            "_sudocode/a2a/send",
            json!({ "sessionId": sid, "to": PEER, "message": MARKER, "summary": "probe" }),
        )
        .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the pre-tool hook never ran"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The hook runs just before dispatch; give the worker time to reach the
    // blocked write. Too little time can only fail this test, never pass it.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let pause_id = client
        .send_request_no_wait("_sudocode/session/pause_tools", json!({ "sessionId": sid }))
        .await;
    let mut seen = match client
        .recv_until(Duration::from_secs(3), |m| is_response_to(m, pause_id))
        .await
    {
        Ok((_, early)) => {
            panic!("pause reported control while the send's worker was still writing: {early}")
        }
        Err(seen) => seen,
    };

    // Release the worker by draining the pipe until its line arrives.
    let mut drained = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let line_end = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes);
        text.find(MARKER)
            .is_some_and(|at| text[at..].contains('\n'))
    };
    while !line_end(&drained) {
        assert!(
            std::time::Instant::now() < deadline,
            "the send never finished its write"
        );
        let mut buf = [0u8; 65_536];
        match pipe.read(&mut buf) {
            Ok(n) => drained.extend_from_slice(&buf[..n]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("drain the pipe: {error}"),
        }
    }

    let (more, paused) = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, pause_id))
        .await
        .unwrap_or_else(|seen| panic!("pause never reported control; saw {seen:#?}"));
    seen.extend(more);
    assert_eq!(
        paused["result"]["isQuiescent"], true,
        "pause reports control once the worker has finished: {paused}"
    );
    let sent = match seen.into_iter().find(|m| is_response_to(m, send_id)) {
        Some(sent) => sent,
        None => {
            client
                .recv_until(Duration::from_secs(30), |m| is_response_to(m, send_id))
                .await
                .unwrap_or_else(|seen| panic!("the send never answered; saw {seen:#?}"))
                .1
        }
    };
    assert_unknown(&sent, "a send the pause stopped after admission");

    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut buf = [0u8; 65_536];
    if let Ok(n) = pipe.read(&mut buf) {
        drained.extend_from_slice(&buf[..n]);
    }
    assert_eq!(
        String::from_utf8_lossy(&drained).matches(MARKER).count(),
        1,
        "the message was written once and never resent"
    );
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "a direct call never asks the model"
    );
    let (_, resumed) = client
        .send_request(
            "_sudocode/session/resume_tools",
            json!({ "sessionId": sid, "pauseEpoch": paused["result"]["pauseEpoch"] }),
        )
        .await;
    assert!(resumed.get("error").is_none(), "resume_tools: {resumed}");
    client.shutdown().await;
    workspace.cleanup();
}

/// What `sender` has appended to its conversation with a credential's agent on
/// a node, from `cursor`, read through that agent's own credential: `(from,
/// body)` pairs and the next cursor. The node client is synchronous, so it
/// runs on a blocking thread.
async fn node_conversation(
    endpoint: &str,
    credential: &str,
    sender: &str,
    cursor: u64,
    block_ms: u64,
) -> (Vec<(String, String)>, u64) {
    let (endpoint, credential, sender) = (
        endpoint.to_string(),
        credential.to_string(),
        sender.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        let credential = runtime::nexus_mailbox::AgentCredential::load(&credential)
            .expect("load the receiver credential bundle");
        let agent = credential.agent.clone();
        let client = runtime::nexus_mailbox::Config {
            endpoint,
            agent: credential.agent,
            tls: credential.tls,
        }
        .connect()
        .expect("dial the node as the receiver");
        let inbox =
            runtime::mailbox::Mailbox::over_nexus_mtls(client, agent, String::new(), "root");
        inbox
            .ensure_conversation(&sender)
            .expect("provision the conversation");
        let (envelopes, next) = inbox
            .poll_conversation(&sender, cursor, block_ms)
            .expect("read the conversation on the node");
        (
            envelopes
                .into_iter()
                .map(|envelope| (envelope.from, envelope.body))
                .collect(),
            next,
        )
    })
    .await
    .expect("node read task")
}

/// Read the conversation on the node from `cursor` until a body containing
/// `marker` has arrived, returning everything read and the next cursor.
async fn node_conversation_until(
    endpoint: &str,
    credential: &str,
    sender: &str,
    mut cursor: u64,
    marker: &str,
) -> (Vec<(String, String)>, u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut stored = Vec::new();
    while !stored
        .iter()
        .any(|(_, body): &(String, String)| body.contains(marker))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "{marker} never reached the node; read {stored:?}"
        );
        let (more, next) = node_conversation(endpoint, credential, sender, cursor, 2_000).await;
        stored.extend(more);
        cursor = next;
    }
    (stored, cursor)
}

/// An ACP client's direct send through a real nexus node: the node stamps the
/// session's own authenticated identity on exactly one append, nothing falls
/// back to a local mailbox, and the model is never asked. A call cancelled
/// after its append answers that its outcome is unknown, and nothing resends
/// it. `e2e/nexus-a2a/run.sh` supplies the daemon and both credentials;
/// without them this test reports that it was skipped.
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn direct_a2a_through_a_node_appends_once_as_the_session() {
    let vars = [
        "NEXUS_A2A_TEST_ENDPOINT",
        "NEXUS_A2A_TEST_DIRECT_SENDER_CREDENTIAL",
        "NEXUS_A2A_TEST_DIRECT_RECEIVER_CREDENTIAL",
    ]
    .map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    let [Some(endpoint), Some(sender_credential), Some(receiver_credential)] = vars else {
        eprintln!(
            "SKIP(nexus): set NEXUS_A2A_TEST_ENDPOINT and the direct-send credentials - \
             e2e/nexus-a2a/run.sh does"
        );
        return;
    };
    let sender = runtime::nexus_mailbox::AgentCredential::load(&sender_credential)
        .expect("load the sender credential bundle")
        .agent;
    let receiver = runtime::nexus_mailbox::AgentCredential::load(&receiver_credential)
        .expect("load the receiver credential bundle")
        .agent;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let sent_marker = format!("DIRECT_NODE_SENT_{nonce}");
    let held_marker = format!("DIRECT_NODE_HELD_{nonce}");
    let (_, tail) = node_conversation(&endpoint, &receiver_credential, &sender, 0, 0).await;

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("direct-a2a-node");
    workspace.create();
    workspace.write_sudocode_json(&server.base_url());
    // Once armed, the post-tool hook holds a call after its append, so a
    // cancel lands after the message has gone out.
    let hook = if cfg!(windows) {
        r"if exist direct-node.arm (echo started>direct-node.started& for /L %i in (1,1,60) do @if not exist direct-node.release (ping -n 2 127.0.0.1 >nul))"
    } else {
        r"if [ -f direct-node.arm ]; then touch direct-node.started; i=0; while [ ! -f direct-node.release ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; fi"
    };
    let settings = workspace.root.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings).expect("create project settings");
    fs::write(
        settings.join("settings.json"),
        json!({ "agentName": sender, "hooks": { "PostToolUse": [hook] } }).to_string(),
    )
    .expect("write project settings");
    // The session attaches the way a machine does: a node record in its home
    // and the credential minted for its own name.
    let home = node_home::NodeHome::new(
        "direct-a2a-node",
        &endpoint,
        &sender,
        Path::new(&sender_credential),
    );
    let [(home_key, home_value), (profile_key, profile_value)] = home.env();
    let mut client = spawn_stdio_client_with_args_and_env(
        &workspace,
        &[],
        &[
            (home_key, home_value.as_str()),
            (profile_key, profile_value.as_str()),
        ],
    );
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let before = server.captured_requests().await.len();

    let (_, sent) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({ "sessionId": sid, "to": receiver, "message": sent_marker, "summary": "node probe" }),
        )
        .await;
    assert_eq!(
        sent["result"]["isError"], false,
        "the send ran through the node: {sent}"
    );
    let (stored, tail) =
        node_conversation_until(&endpoint, &receiver_credential, &sender, tail, &sent_marker).await;
    let delivered: Vec<_> = stored
        .iter()
        .filter(|(_, body)| body.contains(&sent_marker))
        .collect();
    assert_eq!(delivered.len(), 1, "one append on the node: {stored:?}");
    assert_eq!(
        delivered[0].0, sender,
        "the node stamps the session's authenticated identity: {stored:?}"
    );
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    for local in [
        runtime::agent_mailbox::inbox_path_under(&workspace.root, &receiver),
        PathBuf::from(
            runtime::mailbox::Mailbox::workspace_local(&pair_root, receiver.clone())
                .transcript_path(&sender),
        ),
    ] {
        assert!(
            !local.exists(),
            "a session on a node must not fall back to {}",
            local.display()
        );
    }

    let (_, listed) = client
        .send_request("_sudocode/a2a/agent_list", json!({ "sessionId": sid }))
        .await;
    assert_eq!(
        listed["result"]["isError"], false,
        "agent_list ran through the node: {listed}"
    );

    fs::write(workspace.root.join("direct-node.arm"), "").expect("arm the hook");
    let held_id = client
        .send_request_no_wait(
            "_sudocode/a2a/send",
            json!({ "sessionId": sid, "to": receiver, "message": held_marker, "summary": "node probe" }),
        )
        .await;
    let started = workspace.root.join("direct-node.started");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the post-tool hook never held the call"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (stored, tail) =
        node_conversation_until(&endpoint, &receiver_credential, &sender, tail, &held_marker).await;
    client
        .send_raw(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": sid }
        }))
        .await;
    let answered = client
        .recv_until(Duration::from_secs(30), |m| is_response_to(m, held_id))
        .await;
    let _ = fs::write(workspace.root.join("direct-node.release"), "");
    let (_, resp) =
        answered.unwrap_or_else(|seen| panic!("the cancelled send never answered; saw {seen:#?}"));
    assert_unknown(&resp, "a call cancelled after its append on the node");

    tokio::time::sleep(Duration::from_secs(2)).await;
    let (later, _) = node_conversation(&endpoint, &receiver_credential, &sender, tail, 0).await;
    let held = stored
        .iter()
        .chain(later.iter())
        .filter(|(_, body)| body.contains(&held_marker))
        .count();
    assert_eq!(
        held, 1,
        "the held message was appended once and never resent"
    );
    assert_eq!(
        server.captured_requests().await.len(),
        before,
        "a direct call never asks the model"
    );
    client.shutdown().await;
    workspace.cleanup();
}
