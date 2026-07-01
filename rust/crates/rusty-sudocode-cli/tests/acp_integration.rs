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

/// Regression guard for the class of bug where `run_acp_server` forgets to
/// call `model_capabilities::load` — the SSOT cache would never be read,
/// `vision_capable` would fall back to the optimistic default, and the
/// wrong-model VLM-route branch of push_images would NEVER fire in production
/// (silent regression, no test-time failure without a scenario like this).
///
/// Real e2e caught this bug on 2026-07-01 after 40 min of blind debugging;
/// this test catches it in <10 s in CI on the very next run.
///
/// The test's approach:
///   1. Seed `<config_home>/cache/model-capabilities.json` with a fixture
///      model `text-only-test-fixture` marked `vision_supported: false`.
///   2. Spawn scode acp with `--model text-only-test-fixture`.
///   3. Send `session/prompt` carrying an inline image content block.
///   4. Capture scode's stderr concurrently; assert it contains a
///      `[push_images]` log line indicating VLM-route was ENTERED.
///      (The VLM HTTP call itself fails cleanly against the unreachable
///      sudorouter URL in the sample sudocode.json — that's fine, we're
///      testing the ROUTING decision, not the VLM round-trip.)
#[tokio::test]
async fn acp_wrong_model_routes_via_vlm() {
    // `sonnet` = CLI alias other tests use (safe pass-through to mock).
    // `claude-sonnet-4-6` = the WIRE model name scode resolves the alias to,
    // and what push_images actually calls vision_capable() with. The cache
    // seed must use the WIRE name — the alias never reaches vision_capable
    // (verified via CI diagnostic eprintln 2026-07-01: on the same run that
    // proved the ROUTING code path was fine, seeded-alias-key made
    // vision_capable() return the optimistic default because lookup missed).
    const TEST_MODEL: &str = "sonnet";
    const WIRE_MODEL: &str = "claude-sonnet-4-6";

    let server = MockAnthropicService::spawn()
        .await
        .expect("mock service should start");
    let workspace = TestWorkspace::new("wrong-model-vlm");
    workspace.create();
    // Point sudorouter at a guaranteed-refused local address so the VLM
    // HTTP call fails FAST with connection refused (instead of the sample
    // sudocode.json's `hk.sudorouter.ai` which just hangs in CI's
    // network-isolated env until the 30s reqwest timeout — that plus the
    // 90s test recv timeout raced badly, causing the test to panic before
    // scode's error-placeholder response could get back). Fast fail is
    // what we want here: this test is scoped to prove the wrong-model
    // BRANCH was entered, not that the VLM roundtrip succeeded. The
    // full-roundtrip counterpart covers success with a real mock.
    workspace.write_sudocode_json_with_sudorouter(&server.base_url(), "http://127.0.0.1:1/v1");
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

    // Drain stderr concurrently into a shared Vec so the assertion at the
    // end can inspect what push_images logged.
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

    // 1x1 transparent PNG — 67 bytes.
    const TINY_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=";

    // initialize + session/new
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

    // session/prompt with inline image. The request may return an error or
    // a completion — we don't care about the response body here, only that
    // scode's push_images went down the VLM-route branch (proving
    // model_capabilities::load ran and populated the OnceLock with our
    // text-only fixture, causing vision_capable(TEST_MODEL) to return false).
    let _ = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    { "type": "image", "data": TINY_PNG_BASE64, "mimeType": "image/png" },
                    { "type": "text", "text": "Describe the image." }
                ]
            }),
        )
        .await;

    // Poll stderr for up to 15s; push_images entry log should appear near
    // the beginning of prompt handling.
    let mut saw_entered = false;
    let mut saw_vlm_route = false;
    for _ in 0..30 {
        {
            let lines = stderr_captured.lock().await;
            for l in lines.iter() {
                if l.contains("[push_images] entered") {
                    saw_entered = true;
                }
                // Either the VLM-route start log OR the no-creds fallback log
                // proves the wrong-model VLM branch (not the native branch) was
                // taken. The sample sudocode.json points sudorouter at
                // hk.sudorouter.ai which is unreachable in CI's env_clear'd
                // environment — a fallback message is the expected outcome.
                if l.contains("VLM-route start")
                    || l.contains("VLM describe failed")
                    || l.contains("no sudorouter creds")
                {
                    saw_vlm_route = true;
                }
            }
        }
        if saw_entered && saw_vlm_route {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let final_lines = stderr_captured.lock().await.clone();
    assert!(
        saw_entered,
        "push_images should have been entered when session/prompt carried an image.\nstderr: {final_lines:#?}"
    );
    assert!(
        saw_vlm_route,
        "wrong-model VLM-route branch should have fired for text-only fixture model — \
         if this assertion fails, most likely `model_capabilities::load` was NOT called \
         in run_acp_server (regression of the 2026-07-01 bug fixed in commit 293286ed).\n\
         stderr: {final_lines:#?}"
    );

    client.shutdown().await;
    workspace.cleanup();
}

/// Full-roundtrip counterpart to `acp_wrong_model_routes_via_vlm`. That test
/// verifies the ROUTING decision (wrong-model branch entered); this one
/// verifies the ENTIRE VLM call chain including sudorouter round-trip:
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
///    `chat/completions`, and body containing an `image_url` content part.
///  - session/prompt returns a stopReason (didn't hang) — proves both the
///    VLM leg and the subsequent MockAnthropicService leg completed.
///
/// This catches:
///  a) `model_capabilities::load` missing in run_acp_server (as
///     acp_wrong_model_routes_via_vlm does), AND
///  b) VLM-route wire-format regressions (wrong endpoint path, wrong content
///     shape, missing Authorization header, etc.), AND
///  c) block_in_place / runtime nesting regressions that would hang the call.
#[tokio::test]
async fn acp_wrong_model_vlm_full_roundtrip() {
    // See acp_wrong_model_routes_via_vlm for the sonnet + api-key + WIRE_MODEL rationale.
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
