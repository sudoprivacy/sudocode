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
    /// Everything the last reply's read consumed, the reply and what preceded it.
    /// An event written concurrently with that reply can be in here and nowhere
    /// else: the terminal does not show it again.
    last_read: String,
}

impl Host {
    fn new(env: &TestEnv) -> Self {
        Self::with_permissions(env, true)
    }

    fn with_permissions(env: &TestEnv, allow_tools: bool) -> Self {
        Self::with_options(env, allow_tools, &[])
    }

    fn with_options(env: &TestEnv, allow_tools: bool, extra: &[(&str, &str)]) -> Self {
        let mut terminal = if allow_tools {
            env.spawn_with_env(&["--dangerously-skip-permissions", "acp"], extra)
        } else {
            env.spawn_with_env(&["--permission-mode", "workspace-write", "acp"], extra)
        };
        terminal.resize(40, 4096).unwrap();
        terminal.set_default_timeout(common::LIVE_TURN_BUDGET);
        let mut host = Self {
            terminal,
            session: String::new(),
            id: 0,
            last_read: String::new(),
        };
        let init = host.request("initialize", &json!({"protocolVersion":1}));
        assert_eq!(init["_meta"]["sudocode"]["inputDelivery"]["version"], 1);
        assert_eq!(init["_meta"]["sudocode"]["runtimeStatus"]["version"], 1);
        assert_eq!(init["_meta"]["sudocode"]["attention"]["version"], 1);
        let created = host.request(
            "session/new",
            &json!({"cwd":env.workspace_root(),"mcpServers":[]}),
        );
        created["sessionId"]
            .as_str()
            .unwrap()
            .clone_into(&mut host.session);
        assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
        host
    }

    fn request(&mut self, method: &str, params: &Value) -> Value {
        let response = self.response(method, params);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }

    fn response(&mut self, method: &str, params: &Value) -> Value {
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
                    self.terminal.render(common::screen_contents)
                )
            });
        self.last_read.clone_from(&response);
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
        self.request(METHOD, &action)
    }

    fn cancel(&mut self) {
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        self.terminal.send(&format!(
            "{}{newline}",
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.session}})
        )).unwrap();
    }

    fn incoming_request(&mut self, method: &str) -> Value {
        let message = self
            .terminal
            .expect(&format!(
                r#"\{{"jsonrpc":"2.0","method":"{method}",[^\r\n]*\}}"#
            ))
            .unwrap();
        message
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find(|value| value["method"] == method)
            .unwrap_or_else(|| panic!("missing {method} request: {message}"))
    }

    fn respond(&mut self, request: &Value, result: &Value) {
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        self.terminal
            .send(&format!(
                "{}{newline}",
                json!({"jsonrpc":"2.0","id":request["id"],"result":result})
            ))
            .unwrap();
    }

    fn submit(&mut self, client: &str, text: &str, delivery: &str) -> Value {
        self.input(json!({"action":"submit","clientId":client,"delivery":delivery,"content":[{"type":"text","text":text}]}))
    }

    fn snapshot(&mut self) -> Value {
        self.input(json!({"action":"snapshot"}))["state"].clone()
    }

    fn attention(&mut self) -> Value {
        self.request(
            "_sudocode/session/attention",
            &json!({"sessionId":self.session}),
        )
    }

    fn runtime_status(&mut self) -> Value {
        self.request(
            "_sudocode/session/status",
            &json!({"sessionId":self.session}),
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
        self.request("session/close", &json!({"sessionId":self.session}));
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
    // Likewise, publish completion only after the full output is written; a
    // shell redirection creates an empty file before printf fills it.
    fs::write(env.workspace_root().join("input-work.sh"), ": > input-tool-ticks\nprintf started > input-tool-started\ni=0\nwhile [ ! -f release-input-tool ] && [ $i -lt 1200 ]; do\n printf x >> input-tool-ticks\n sleep 0.1\n i=$((i+1))\ndone\nprintf ORIGINAL_INPUT_TOOL_RESULT > input-tool-finished.tmp\nmv input-tool-finished.tmp input-tool-finished\n").unwrap();
    let mut host = Host::new(env);
    let input =
        json!({"command":"sh input-work.sh","timeout":180_000,"run_in_background":background});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"input-shell","name":"Bash","input":input}])
        )
    } else {
        format!(
            "Run the Bash command exactly once with these literal inputs: {input}. If backgrounded, finish your turn immediately and wait for the completion notification. The test harness releases it. Do not create the release file, modify the script or start another shell. Later input is FYI; continue the original task unless needed. Report the actual result after it finishes."
        )
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
    assert_eq!(
        host.snapshot()["pending"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
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
    // The shell appends a tick every 100 ms until released. A loaded host can
    // delay one well past a fixed window, so wait within the shared budget: a
    // cancelled shell never ticks again and still fails here.
    let ticks = env.workspace_root().join("input-tool-ticks");
    let before = fs::read(&ticks).unwrap().len();
    common::expect_screen(
        &host.terminal,
        |_| fs::read(&ticks).is_ok_and(|bytes| bytes.len() > before),
        common::DEFAULT_TIMEOUT,
        "flush cancelled the real shell",
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

/// Whether `text` holds a state notification with a started turn: the same line
/// `"kind":"state","state":{"activeTurnId":[1-9]` matches on the terminal.
fn admits_a_turn(text: &str) -> bool {
    const STATE: &str = r#""kind":"state","state":{"activeTurnId":"#;
    text.match_indices(STATE)
        .any(|(at, _)| matches!(text[at + STATE.len()..].chars().next(), Some('1'..='9')))
}

#[test]
fn host_cancel_keeps_queued_input_until_an_explicit_flush() {
    let env = TestEnv::new("host-input-cancel");
    let (mut host, _release) = working(&env);
    let mut observer = Host::new(&env);
    host.submit(
        "after-cancel",
        "HOST_AFTER_CANCEL: reply briefly; do not run any tools.",
        "queue",
    );
    host.cancel();
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
    // The flush admits the queued input before it replies, but the turn's state
    // notification and the reply are written by different tasks, so either can
    // reach the wire first. Wait for the admission unless reading the reply
    // already consumed it.
    if !admits_a_turn(&host.last_read) {
        host.terminal
            .expect(r#""kind":"state","state":\{"activeTurnId":[1-9]"#)
            .unwrap();
    }
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
    observer.close();
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
        let response = host.response(method, &params);
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
    let mut observer = Host::new(&env);
    let input = json!({"command":"printf HOST_PERMISSION_GRANTED > host-permission-proof.txt"});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"input-permission","name":"Bash","input":input}])
        )
    } else {
        format!(
            "Run Bash exactly once with these literal inputs: {input}. Wait for human approval; do not substitute another tool. After it succeeds, report success briefly."
        )
    };
    host.submit("permission-input", &prompt, "steer");
    let request = host.incoming_request("session/request_permission");
    assert_eq!(request["params"]["sessionId"], host.session);
    let attention = host.attention();
    let requests = attention["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 1, "{attention}");
    assert_eq!(requests[0]["phase"], "presented");
    assert_eq!(requests[0]["reason"], "permission");
    assert_eq!(requests[0]["agent"], Value::Null);
    assert!(requests[0]["summary"]
        .as_str()
        .unwrap()
        .contains("HOST_PERMISSION_GRANTED"));
    assert_eq!(
        host.runtime_status()["foreground"],
        json!({"state":"awaiting_input","waiting_for":"permission"})
    );
    assert!(!env
        .workspace_root()
        .join("host-permission-proof.txt")
        .exists());
    host.respond(
        &request,
        &json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
    );
    host.idle();
    let cleared = host.attention();
    assert_eq!(cleared["requests"], json!([]));
    assert!(cleared["revision"].as_u64().unwrap() > attention["revision"].as_u64().unwrap());
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("host-permission-proof.txt")).unwrap(),
        "HOST_PERMISSION_GRANTED"
    );
    assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
    host.close();
    observer.close();
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

/// A background shell that finishes while a turn is still running is steered
/// into that turn, as the REPL does: it reaches the model at the turn's next
/// model boundary, not as a new turn after it ends.
#[test]
fn native_host_background_completion_steers_into_the_running_turn() {
    struct ReleaseBoth(std::path::PathBuf);
    impl Drop for ReleaseBoth {
        fn drop(&mut self) {
            for file in ["release-bg", "release-fg"] {
                let _ = fs::write(self.0.join(file), "cleanup");
            }
        }
    }
    let env = TestEnv::new("host-background-steer");
    if !env.is_mock() {
        return;
    }
    let root = env.workspace_root();
    let _release = ReleaseBoth(root.to_path_buf());
    fs::write(
        root.join("bg-work.sh"),
        "while [ ! -f release-bg ]; do sleep 0.1; done\necho BG_STEER_DONE\n",
    )
    .unwrap();
    fs::write(
        root.join("fg-work.sh"),
        "printf started > fg-started\nwhile [ ! -f release-fg ]; do sleep 0.1; done\necho FG_STEER_DONE\n",
    )
    .unwrap();
    let mut host = Host::new(&env);
    let batch = json!([
        {"id":"bg","name":"Bash","input":{"command":"sh bg-work.sh","timeout":180_000,"run_in_background":true}},
        {"id":"fg","name":"Bash","input":{"command":"sh fg-work.sh","timeout":180_000}},
    ]);
    host.submit(
        "initial",
        &format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}"),
        "steer",
    );
    common::expect_screen(
        &host.terminal,
        |_| root.join("fg-started").exists(),
        common::LIVE_TURN_BUDGET,
        "foreground shell started",
    );

    // The turn is held open by the foreground shell; the background one ends.
    fs::write(root.join("release-bg"), "release").unwrap();
    let deadline = std::time::Instant::now() + common::LIVE_TURN_BUDGET;
    let pending = loop {
        let state = host.snapshot();
        let background: Vec<Value> = state["pending"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["source"] == "background")
            .cloned()
            .collect();
        if !background.is_empty() {
            break background;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no completion: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0]["delivery"], "steer", "{pending:?}");

    fs::write(root.join("release-fg"), "release").unwrap();
    let before_commit = host
        .terminal
        .expect(r#""kind":"committed"[^\r\n]*"source":"background""#)
        .unwrap();
    assert!(
        !before_commit.contains(r#""kind":"turn_complete""#),
        "the completion must commit inside the running turn"
    );
    host.idle();
    let requests = env.captured_message_bodies();
    assert_eq!(requests.len(), 2, "one turn: the batch, then its follow-up");
    let follow_up = requests.last().unwrap();
    assert!(follow_up.contains("task-notification"), "{follow_up}");
    assert!(follow_up.contains("FG_STEER_DONE"), "{follow_up}");
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

impl Host {
    fn discovered(&mut self) -> Value {
        let response = self.request(
            "_sudocode/a2a/agent_list",
            &json!({"sessionId":self.session}),
        );
        assert_eq!(response["isError"], false, "{response}");
        serde_json::from_str(response["content"].as_str().expect("tool content"))
            .expect("directory JSON")
    }
}

#[test]
fn runtime_status_reads_real_quiet_work_and_independent_background_counts() {
    for background in [false, true] {
        let env = TestEnv::new(if background {
            "directory-background"
        } else {
            "directory-quiet"
        });
        let (mut first, _release) = working_with_mode(&env, background);
        let mut observer = Host::new(&env);
        if background {
            first.idle();
        }
        let direct = first.runtime_status();
        assert_eq!(
            direct["foreground"]["state"],
            if background { "idle" } else { "running" }
        );
        // agent_list reports one active bit per Agent and no runtime rows.
        let listing = observer.discovered();
        assert!(listing.get("runtime_discovery").is_none(), "{listing}");
        assert!(
            listing["agents"]
                .as_array()
                .expect("agents")
                .iter()
                .all(|row| row["active"].is_boolean() && row.get("runtime").is_none()),
            "{listing}"
        );
        if background {
            assert_eq!(direct["background"]["terminals"], 1);
        }
        let before = fs::read(env.workspace_root().join("input-tool-ticks"))
            .unwrap()
            .len();
        let deadline = std::time::Instant::now() + common::LIVE_TURN_BUDGET;
        loop {
            if fs::read(env.workspace_root().join("input-tool-ticks"))
                .unwrap()
                .len()
                > before
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "status observation stalled the real shell"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        fs::write(env.workspace_root().join("release-input-tool"), "done").unwrap();
        common::expect_screen(
            &first.terminal,
            |_| env.workspace_root().join("input-tool-finished").exists(),
            common::LIVE_TURN_BUDGET,
            "released shell finished independently of foreground idle",
        );
        first.idle();
        assert_eq!(
            fs::read_to_string(env.workspace_root().join("input-tool-finished")).unwrap(),
            "ORIGINAL_INPUT_TOOL_RESULT"
        );
        first.close();
        observer.close();
    }
}

#[test]
fn question_wait_keeps_independent_tools_and_background_work_visible() {
    let env = TestEnv::new_mock("directory-question-independent-work");
    let (mut first, _release) = working_with_mode(&env, true);
    first.idle();
    let mut observer = Host::new(&env);
    fs::write(
        env.workspace_root().join("question-fixture.txt"),
        "DIRECTORY_SIBLING_RESULT",
    )
    .unwrap();
    let batch = json!([
        {"id":"directory-question","name":"AskUserQuestion","input":{"questions":[{"id":"q1","prompt":"DIRECTORY_PENDING_QUESTION"}]}},
        {"id":"directory-read","name":"Bash","input":{"command":"cat question-fixture.txt"}},
    ]);
    first.submit(
        "question-with-work",
        &format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}"),
        "steer",
    );
    let question = first.incoming_request("_scode/ask_user_question");
    common::expect_screen(
        &first.terminal,
        |screen| screen.contains("DIRECTORY_SIBLING_RESULT"),
        common::LIVE_TURN_BUDGET,
        "independent read finishes before the question is answered",
    );
    let direct = first.runtime_status();
    assert_eq!(
        direct["foreground"],
        json!({"state":"awaiting_input","waiting_for":"question"})
    );
    assert_eq!(direct["background"]["terminals"], 1);
    let ticks = env.workspace_root().join("input-tool-ticks");
    let before = fs::read(&ticks).unwrap().len();
    common::expect_screen(
        &first.terminal,
        |_| fs::read(&ticks).is_ok_and(|bytes| bytes.len() > before),
        common::LIVE_TURN_BUDGET,
        "background shell continues while the foreground owns user input",
    );
    first.respond(
        &question,
        &json!({"answers":[{"id":"q1","value":"continue","label":null}]}),
    );
    first.idle();
    fs::write(env.workspace_root().join("release-input-tool"), "done").unwrap();
    common::expect_screen(
        &first.terminal,
        |_| env.workspace_root().join("input-tool-finished").exists(),
        common::LIVE_TURN_BUDGET,
        "independent background work finishes after release",
    );
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("input-tool-finished")).unwrap(),
        "ORIGINAL_INPUT_TOOL_RESULT"
    );
    first.close();
    observer.close();
}

#[test]
fn attention_tracks_queued_questions_and_clears_cancelled_requests() {
    let env = TestEnv::new_mock("attention-queued-questions");
    let mut host = Host::new(&env);
    let batch = json!([
        {"id":"one","name":"AskUserQuestion","input":{"questions":[{"id":"q1","prompt":"FIRST_ATTENTION_REQUEST"}]}},
        {"id":"two","name":"AskUserQuestion","input":{"questions":[{"id":"q2","prompt":"SECOND_ATTENTION_REQUEST"}]}}
    ]);
    host.submit(
        "two-questions",
        &format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}"),
        "steer",
    );
    let first = host.incoming_request("_scode/ask_user_question");
    let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
    let waiting = loop {
        let state = host.attention();
        if state["requests"].as_array().unwrap().len() == 2 {
            break state;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "second request never queued: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(waiting["requests"][0]["phase"], "presented");
    assert_eq!(waiting["requests"][1]["phase"], "queued");
    host.respond(&first, &json!({"answers":[{"id":"q1","value":"continue"}]}));
    let _second = host.incoming_request("_scode/ask_user_question");
    let next = host.attention();
    assert_eq!(next["requests"].as_array().unwrap().len(), 1);
    assert_eq!(next["requests"][0]["id"], waiting["requests"][1]["id"]);
    assert_eq!(next["requests"][0]["phase"], "presented");
    host.cancel();
    host.idle();
    assert_eq!(host.attention()["requests"], json!([]));
    host.close();
}

#[test]
fn attention_background_child_can_request_approval_after_parent_turn_finishes() {
    assert_background_child_approval(false, false);
}

#[test]
fn attention_automatically_backgrounded_child_retains_approval_owner() {
    assert_background_child_approval(true, false);
}

#[test]
fn attention_foreground_cancel_does_not_revoke_a_detached_childs_reply_route() {
    assert_background_child_approval(false, true);
}

fn assert_background_child_approval(automatic: bool, cancel_parent: bool) {
    let env = TestEnv::new_mock("attention-background-child");
    fs::write(
        env.workspace_root()
            .join(mock_anthropic_service::SUBAGENT_CHILD_READ_PATH),
        "CHILD_APPROVAL_CONTENT",
    )
    .unwrap();
    let mut host = Host::with_options(&env, false, &[("SUDOCODE_AGENT_AUTO_BG_SECS", "1")]);
    host.request(
        "session/setPermissionMode",
        &json!({"sessionId":host.session,"permissionMode":"prompt"}),
    );
    let mut batch = json!([{"id":"detached-child","name":"agent_spawn","input":{
        "description":"child approval owner", "subagent_type":"general-purpose", "model":"inherit", "run_in_background":!automatic,
        "prompt":"PARITY_SCENARIO:subagent_slow_child Read the notes"
    }}]);
    if cancel_parent {
        batch
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"parent-hold","name":"Bash","input":{
                "command":"echo ATTENTION_PARENT_HOLD"
            }}));
    }
    host.submit(
        "parent",
        &format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}"),
        "steer",
    );
    let spawn = host.incoming_request("session/request_permission");
    assert!(spawn["params"]["toolCall"]["rawInput"]
        .as_str()
        .unwrap()
        .contains("run_in_background"));
    host.respond(
        &spawn,
        &json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
    );
    if cancel_parent {
        let pending_parent = host.incoming_request("session/request_permission");
        assert!(pending_parent["params"]["toolCall"]["rawInput"]
            .as_str()
            .unwrap()
            .contains("ATTENTION_PARENT_HOLD"));
        host.cancel();
    }
    host.idle();
    assert_eq!(
        host.attention()["requests"],
        json!([]),
        "child request must happen after the parent ended"
    );
    let child = host.incoming_request("session/request_permission");
    assert!(child["params"]["toolCall"]["rawInput"]
        .as_str()
        .unwrap()
        .contains(mock_anthropic_service::SUBAGENT_CHILD_READ_PATH));
    let state = host.attention();
    assert_eq!(state["requests"].as_array().unwrap().len(), 1, "{state}");
    assert_eq!(state["requests"][0]["agent"]["background"], true);
    assert!(state["requests"][0]["agent"]["agent_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));
    assert_eq!(state["requests"][0]["phase"], "presented");
    assert_eq!(host.runtime_status()["foreground"]["state"], "idle");
    host.respond(
        &child,
        &json!({"outcome":{"outcome":"selected","optionId":"allow_once"}}),
    );
    let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
    loop {
        if host.attention()["requests"] == json!([]) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "answered child request did not clear"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    host.close();
}
