//! The real ACP process owns delivery while a real foreground shell is running.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::{fs, time::Duration};

const METHOD: &str = "_sudocode/session/input";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=";

struct Host {
    terminal: PtySession,
    session: String,
    id: u64,
}

impl Host {
    fn new(env: &TestEnv) -> Self {
        Self::with_permissions(env, true)
    }

    fn with_permissions(env: &TestEnv, allow_tools: bool) -> Self {
        let mut terminal = if allow_tools {
            env.spawn(&["--dangerously-skip-permissions", "acp"])
        } else {
            env.spawn(&["--permission-mode", "workspace-write", "acp"])
        };
        terminal.resize(40, 4096).unwrap();
        terminal.set_default_timeout(common::LIVE_TURN_BUDGET);
        let mut host = Self {
            terminal,
            session: String::new(),
            id: 0,
        };
        let init = host.request("initialize", json!({"protocolVersion":1}));
        assert_eq!(init["_meta"]["sudocode"]["inputDelivery"]["version"], 1);
        assert_eq!(init["_meta"]["sudocode"]["runtimeStatus"]["version"], 1);
        let created = host.request(
            "session/new",
            json!({"cwd":env.workspace_root(),"mcpServers":[]}),
        );
        host.session = created["sessionId"].as_str().unwrap().to_owned();
        assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
        host
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let response = self.response(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn response(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        self.terminal
            .send(&format!(
                "{}{newline}",
                json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
            ))
            .unwrap();
        let response = self
            .terminal
            .expect(&format!(
                r#"\{{"jsonrpc":"2.0","(?:result|error)":[^\r\n]*,"id":{}\}}"#,
                self.id
            ))
            .unwrap_or_else(|error| {
                panic!(
                    "ACP reply: {error}; {}",
                    self.terminal.render(|screen| screen.contents())
                )
            });
        let response: Value = response
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find(|value| {
                value["id"] == self.id
                    && (value.get("result").is_some() || value.get("error").is_some())
            })
            .unwrap_or_else(|| panic!("response line missing: {response}"));
        response
    }

    fn input(&mut self, mut action: Value) -> Value {
        action["sessionId"] = json!(self.session);
        self.request(METHOD, action)
    }

    fn submit(&mut self, client: &str, text: &str, delivery: &str) -> Value {
        self.input(json!({"action":"submit","clientId":client,"delivery":delivery,"content":[{"type":"text","text":text}]}))
    }

    fn snapshot(&mut self) -> Value {
        self.input(json!({"action":"snapshot"}))["state"].clone()
    }

    fn runtime_status(&mut self) -> Value {
        self.request(
            "_sudocode/session/status",
            json!({"sessionId":self.session}),
        )
    }

    fn idle(&mut self) -> Value {
        let deadline = std::time::Instant::now() + common::LIVE_TURN_BUDGET;
        loop {
            let state = self.snapshot();
            if state["running"] == false {
                return state;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session did not stop: {state}"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn close(&mut self) {
        self.request("session/close", json!({"sessionId":self.session}));
    }
}

struct Release(std::path::PathBuf);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join("release-input-tool"), "cleanup");
    }
}

fn working(env: &TestEnv) -> (Host, Release) {
    working_with_mode(env, false)
}

fn working_with_mode(env: &TestEnv, background: bool) -> (Host, Release) {
    // Publish readiness only after the progress file can be read. ACP replies
    // may otherwise finish before the shell gets to its first loop iteration.
    fs::write(env.workspace_root().join("input-work.sh"), ": > input-tool-ticks\nprintf started > input-tool-started\ni=0\nwhile [ ! -f release-input-tool ] && [ $i -lt 1200 ]; do\n printf x >> input-tool-ticks\n sleep 0.1\n i=$((i+1))\ndone\nprintf ORIGINAL_INPUT_TOOL_RESULT > input-tool-finished\n").unwrap();
    let mut host = Host::new(env);
    let input =
        json!({"command":"sh input-work.sh","timeout":180000,"run_in_background":background});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"input-shell","name":"Bash","input":input}])
        )
    } else {
        format!("Run the Bash command exactly once with these literal inputs: {input}. If backgrounded, finish your turn immediately and wait for the completion notification. The test harness releases it. Do not create the release file, modify the script or start another shell. Later input is FYI; continue the original task unless needed. Report the actual result after it finishes.")
    };
    host.submit("initial", &prompt, "steer");
    common::expect_screen(
        &host.terminal,
        |_| env.workspace_root().join("input-tool-started").exists(),
        common::LIVE_TURN_BUDGET,
        "foreground shell started",
    );
    (host, Release(env.workspace_root().to_path_buf()))
}

#[test]
fn host_flush_preserves_the_running_tool_and_all_recalled_attachments() {
    let env = TestEnv::new("host-input-flush");
    let (mut host, _release) = working(&env);
    assert_eq!(
        host.runtime_status()["foreground"],
        json!({"state":"running"})
    );
    host.submit(
        "first",
        "HOST_INPUT_FIRST: FYI only; continue the original task.",
        "queue",
    );
    host.input(json!({"action":"submit","clientId":"image","delivery":"queue","content":[{"type":"text","text":"HOST_INPUT_IMAGE: attached reference, continue."},{"type":"image","mimeType":"image/png","data":PNG}]}));
    let recalled = host.input(json!({"action":"recall"}));
    assert_eq!(recalled["recalled"].as_array().unwrap().len(), 2);
    assert_eq!(recalled["recalled"][0]["clientId"], "first");
    assert_eq!(recalled["recalled"][1]["content"][1]["data"], PNG);
    assert!(host.snapshot()["pending"].as_array().unwrap().is_empty());
    for item in recalled["recalled"].as_array().unwrap() {
        host.input(json!({"action":"submit","clientId":item["clientId"],"content":item["content"],"delivery":"queue"}));
    }
    let flushed = host.input(json!({"action":"flush_or_cancel"}));
    assert_eq!(flushed["cancelled"], false);
    assert_eq!(flushed["state"]["pending"].as_array().unwrap().len(), 2);
    assert!(flushed["state"]["pending"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| item["delivery"] == "steer"));
    let before = fs::read(env.workspace_root().join("input-tool-ticks"))
        .unwrap()
        .len();
    std::thread::sleep(Duration::from_millis(350));
    assert!(
        fs::read(env.workspace_root().join("input-tool-ticks"))
            .unwrap()
            .len()
            > before,
        "flush cancelled the real shell"
    );
    fs::write(env.workspace_root().join("release-input-tool"), "release").unwrap();
    let idle = host.idle();
    assert!(idle["pending"].as_array().unwrap().is_empty(), "{idle}");
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("input-tool-finished")).unwrap(),
        "ORIGINAL_INPUT_TOOL_RESULT"
    );
    if env.is_mock() {
        let bodies = env.captured_message_bodies();
        let last: Value = serde_json::from_str(bodies.last().unwrap()).unwrap();
        let wire = last["messages"].to_string();
        assert!(
            wire.contains("HOST_INPUT_FIRST") && wire.contains("HOST_INPUT_IMAGE"),
            "{last}"
        );
        assert!(
            wire.contains("image/png"),
            "queued attachment did not reach the model: {last}"
        );
    }
    host.close();
}

#[test]
fn host_cancel_keeps_queued_input_until_an_explicit_flush() {
    let env = TestEnv::new("host-input-cancel");
    let (mut host, _release) = working(&env);
    host.submit(
        "after-cancel",
        "HOST_AFTER_CANCEL: reply briefly; do not run any tools.",
        "queue",
    );
    let newline = if cfg!(windows) { "\r" } else { "\n" };
    host.terminal
        .send(&format!(
            "{}{newline}",
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":host.session}})
        ))
        .unwrap();
    let paused = host.idle();
    assert_eq!(paused["paused"], true, "{paused}");
    assert_eq!(paused["pending"].as_array().unwrap().len(), 1);
    assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
    assert!(!env.workspace_root().join("input-tool-finished").exists());
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            1,
            "queued input auto-resumed after cancellation"
        );
    }
    let flushed = host.input(json!({"action":"flush_or_cancel"}));
    assert_eq!(flushed["cancelled"], false);
    // The control receipt precedes the automatic turn. Wait for its admission first.
    host.terminal
        .expect(r#""kind":"state","state":\{"activeTurnId":[1-9]"#)
        .unwrap();
    let done = host.idle();
    assert!(done["pending"].as_array().unwrap().is_empty(), "{done}");
    if env.is_mock() {
        assert!(env
            .captured_message_bodies()
            .last()
            .unwrap()
            .contains("HOST_AFTER_CANCEL"));
    }
    host.close();
}

#[test]
fn native_turn_rejects_destructive_lifecycle_changes_without_blocking_cancel() {
    let env = TestEnv::new("host-input-lifecycle");
    let (mut host, _release) = working(&env);
    for (method, params) in [
        (
            "session/load",
            json!({"sessionId":host.session,"cwd":env.workspace_root(),"mcpServers":[]}),
        ),
        (
            "session/setPermissionMode",
            json!({"sessionId":host.session,"permissionMode":"workspace-write"}),
        ),
        (
            "session/set_model",
            json!({"sessionId":host.session,"modelId":"claude-sonnet-4-6"}),
        ),
        (
            "session/prompt",
            json!({"sessionId":host.session,"prompt":[{"type":"text","text":"/clear"}]}),
        ),
    ] {
        let response = host.response(method, params);
        assert!(
            response["error"]["data"]
                .as_str()
                .is_some_and(|message| message.contains("busy")),
            "{method}: {response}"
        );
    }
    assert_eq!(host.snapshot()["running"], true);
    let cancelled = host.input(json!({"action":"flush_or_cancel"}));
    assert_eq!(cancelled["cancelled"], true);
    host.idle();
    assert!(!env.workspace_root().join("input-tool-finished").exists());
    host.close();
}

#[test]
fn native_turn_delivers_permission_requests_without_a_prompt_rpc_listener() {
    let env = TestEnv::new("host-input-permission");
    let mut host = Host::with_permissions(&env, false);
    let input = json!({"command":"printf HOST_PERMISSION_GRANTED > host-permission-proof.txt"});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"input-permission","name":"Bash","input":input}])
        )
    } else {
        format!("Run Bash exactly once with these literal inputs: {input}. Wait for human approval; do not substitute another tool. After it succeeds, report success briefly.")
    };
    host.submit("permission-input", &prompt, "steer");
    let permission = host
        .terminal
        .expect(r#"\{"jsonrpc":"2.0","method":"session/request_permission",[^\r\n]*\}"#)
        .unwrap();
    let request: Value = permission
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|value| value["method"] == "session/request_permission")
        .expect("permission request");
    assert_eq!(request["params"]["sessionId"], host.session);
    assert_eq!(
        host.runtime_status()["foreground"],
        json!({"state":"awaiting_input","waiting_for":"permission"})
    );
    assert!(!env
        .workspace_root()
        .join("host-permission-proof.txt")
        .exists());
    let newline = if cfg!(windows) { "\r" } else { "\n" };
    host.terminal.send(&format!("{}{newline}", json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))).unwrap();
    host.idle();
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("host-permission-proof.txt")).unwrap(),
        "HOST_PERMISSION_GRANTED"
    );
    assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
    host.close();
}

#[test]
fn native_host_background_completion_wakes_an_idle_model_without_human_input() {
    let env = TestEnv::new("host-background-wake");
    let (mut host, _release) = working_with_mode(&env, true);
    host.idle();
    let status = host.runtime_status();
    assert_eq!(status["foreground"], json!({"state":"idle"}));
    assert_eq!(status["background"]["terminals"], 1, "{status}");
    fs::write(env.workspace_root().join("release-input-tool"), "release").unwrap();
    host.terminal
        .expect(r#""kind":"committed"[^\r\n]*"source":"background""#)
        .unwrap();
    let idle = host.idle();
    assert!(idle["pending"].as_array().unwrap().is_empty(), "{idle}");
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("input-tool-finished")).unwrap(),
        "ORIGINAL_INPUT_TOOL_RESULT"
    );
    if env.is_mock() {
        assert!(env
            .captured_message_bodies()
            .last()
            .unwrap()
            .contains("task-notification"));
    }
    host.close();
}

#[test]
fn native_host_queued_input_starts_after_the_current_tool_and_turn_finish() {
    let env = TestEnv::new("host-next-turn");
    let (mut host, _release) = working(&env);
    host.submit(
        "next-turn",
        "HOST_NEXT_TURN: no more tools; reply briefly.",
        "queue",
    );
    fs::write(env.workspace_root().join("release-input-tool"), "release").unwrap();
    host.terminal
        .expect(r#""clientId":"next-turn"[^\r\n]*"kind":"committed""#)
        .unwrap();
    let idle = host.idle();
    assert!(idle["pending"].as_array().unwrap().is_empty(), "{idle}");
    assert!(env.workspace_root().join("input-tool-finished").exists());
    if env.is_mock() {
        assert!(env
            .captured_message_bodies()
            .last()
            .unwrap()
            .contains("HOST_NEXT_TURN"));
    }
    host.close();
}
