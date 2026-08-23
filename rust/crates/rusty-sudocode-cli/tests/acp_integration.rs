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
    for flag in ["systemPromptOverride", "systemPromptAppend"] {
        assert_eq!(
            result["_meta"]["sudocode"][flag].as_bool(),
            Some(true),
            "initialize must advertise _meta.sudocode.{flag}"
        );
    }
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
    scenario_session_load_unknown_session_errors(client).await;
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
/// is parked on a permission prompt must not start (no response, no
/// `session/update`) until the first turn has finished.
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

    let prompt_2 = client
        .send_request_no_wait(
            "session/prompt",
            json!({
                "sessionId": session_a,
                "prompt": [{ "type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text") }]
            }),
        )
        .await;

    // While turn 1 is parked, turn 2 must produce nothing at all.
    let leaked = client
        .recv_until(Duration::from_secs(3), |m| {
            is_response_to(m, prompt_2)
                || m.get("method").and_then(Value::as_str) == Some("session/update")
        })
        .await;
    assert!(
        leaked.is_err(),
        "second prompt on the same session must not run while the first is parked; got: {:?}",
        leaked.ok().map(|(_, m)| m)
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

    // Validation: empty / non-string values are invalid_params for either key.
    assert_bad_prompt_meta_rejected(&mut client, &root, OVERRIDE).await;

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
