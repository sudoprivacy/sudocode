//! ACP integration tests exercising both stdio and WebSocket transports.
//!
//! Each transport runs the same suite of scenarios to verify protocol parity
//! between the SDK-based stdio server and the axum-based WebSocket server.
//!
//! `#![cfg(unix)]` because the ACP stdio server's subprocess handshake
//! (spawn `scode acp`, wait for the "server ready" line on stderr, then
//! exchange JSON-RPC over stdio) hangs on Windows: locally on Win10 +
//! MSVC every scenario panics with `stderr closed before server ready`,
//! and on CI three tests in this file (`acp_stdio_integration`,
//! `acp_stdio_exits_on_stdin_close`, `acp_ws_integration`) caused the
//! windows-latest cargo-test job to wedge for nearly three hours
//! before being cancelled. Either the stderr-pipe contract is racing
//! ConPTY/MinGW handles or the ACP server binary itself doesn't
//! finish init on Windows; either way it's far out of scope for the
//! "wire PTY testing into the matrix" PR. Tracked as a follow-up.

#![cfg(unix)]

#[path = "common/openai_compat_mock.rs"]
mod openai_compat_mock;

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
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

    /// Seed `<config_home>/cache/model-capabilities.json` with a text-only
    /// fixture model so push_images' `vision_capable(...)` returns false and
    /// the VLM-route branch fires. Regression guard for the class of bug
    /// where `run_acp_server` forgets to call `model_capabilities::load` —
    /// without the load call, this fixture never reaches the OnceLock and
    /// vision_capable falls back to the optimistic default (true), so the
    /// wrong-model VLM route never fires. Real-e2e caught this bug on
    /// 2026-07-01; this fixture keeps it from recurring silently.
    fn seed_text_only_test_fixture(&self, model_id: &str) {
        let cache_dir = self.config_home.join("cache");
        fs::create_dir_all(&cache_dir).expect("cache dir");
        // Minimal ModelCapabilitiesFile shape — one text-only test model plus
        // a sane default so the file passes model_capabilities::load's
        // parse_capabilities_json ("must contain a 'default' entry" invariant).
        let json = serde_json::json!({
            "updated_at": 0,
            "default": {"context_window": 200000, "max_output_tokens": 64000},
            "models": {
                model_id: {
                    "context_window": 131072,
                    "max_output_tokens": 64000,
                    "vision_supported": false,
                },
            },
        });
        fs::write(cache_dir.join("model-capabilities.json"), json.to_string())
            .expect("write model-capabilities.json");
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

// ---------------------------------------------------------------------------
// Scenario runner
// ---------------------------------------------------------------------------

async fn run_all_scenarios(client: &mut AcpTestClient, workspace: &TestWorkspace) {
    scenario_initialize(client).await;
    let session_id = scenario_session_new(client, &workspace.root).await;
    scenario_session_prompt_streaming(client, &session_id).await;
    scenario_session_prompt_with_image_attachment(client, &session_id).await;
    scenario_session_list(client, &session_id).await;
    scenario_session_load_not_supported(client).await;
    scenario_unknown_method(client).await;
    scenario_slash_command_model(client, &session_id).await;
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
    run_all_scenarios(&mut client, &workspace).await;
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
///  a) `model_capabilities::load` missing in run_acp_server (SSOT cache
///     never populated → vision_capable falls back to optimistic default
///     → push_images takes native branch → mock gets 0 requests → fail).
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
    // cache seed MUST use the wire name (CI eprintln verified on 2026-07-01).
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
    workspace.seed_text_only_test_fixture(WIRE_MODEL);

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
        .expect("sessionId string")
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
    run_all_scenarios(&mut client, &workspace).await;
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
    }
}

/// Like [`spawn_stdio_client_danger`] but also enables a global
/// `--allowedTools` allow-list, to verify per-session injected MCP tools stay
/// available under an allow-list (they are added to it at runtime build time).
fn spawn_stdio_client_danger_with_allowed(
    workspace: &TestWorkspace,
    allowed: &str,
) -> AcpTestClient {
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
    }
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
                    "command": "python3",
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

/// Injected mcp survives a model switch: handle_acp_model_switch rebuilds the
/// runtime and reuses the session's mcp_servers (stored on AcpCliSession), so
/// the dummy is respawned (proof rewritten) and the tool still works.
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
        "command": "python3",
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
                    "command": "python3",
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
                    "command": "python3",
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
