//! Durable questions over the real engine/ACP boundary and a real PTY.
#[path = "support/acp_pty.rs"]
mod acp_pty;
mod common;
#[path = "support/node_home.rs"]
mod node_home;
#[path = "support/request_evidence.rs"]
mod request_evidence;

use acp_pty::Host;
use common::TestEnv;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const METHOD: &str = "_sudocode/session/interactions";
const ENABLED: &[(&str, &str)] = &[("SUDOCODE_EXPERIMENT_NONBLOCKING_INTERACTIONS", "1")];

#[test]
fn questions_command_uses_help_completion_and_explicit_cancellation() {
    let env = TestEnv::new_mock("interaction-command-registry");
    let mut terminal = env.spawn_with_env(&["--permission-mode", "danger-full-access"], ENABLED);
    terminal.resize(60, 140).unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "command registry ready");
    submit(&mut terminal, "/help");
    terminal
        .expect("Review pending questions and approvals")
        .unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "help finished");
    assert!(
        env.captured_message_bodies().is_empty(),
        "help should not invoke a model"
    );

    let marker = common::turn_status_marker(&terminal);
    submit(&mut terminal, &question_prompt(&env));
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "unanswered question published",
    );
    finish_mock_question(&env);
    let journal_path = interaction_journals(&env)
        .into_iter()
        .find(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            journal["records"]
                .as_array()
                .is_some_and(|records| !records.is_empty())
        })
        .unwrap();
    let journal: Value = serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    let request_id = journal["records"][0]["id"].as_str().unwrap();
    let before = env.captured_message_bodies().len();
    terminal.send("/que").unwrap();
    common::expect_input_line(&terminal, "/que", env.timeout(), "questions prefix typed");
    terminal.send("\t").unwrap();
    common::expect_input_line(
        &terminal,
        "/questions",
        env.timeout(),
        "questions completed from registry",
    );
    terminal.send("\r").unwrap();
    common::expect_screen(
        &terminal,
        |s| s.contains("Which colour?") && s.contains("[1] red"),
        env.timeout(),
        "completed command opens review",
    );
    terminal.send("\x1b").unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "dismissed review returns to chat");
    submit(&mut terminal, "/questions cancel");
    terminal.expect("Usage: /questions").unwrap();
    common::expect_input_line_cleared(
        &terminal,
        env.timeout(),
        "invalid cancellation remains local",
    );
    assert_eq!(
        env.captured_message_bodies().len(),
        before,
        "navigation and invalid commands must not trigger inference"
    );
    let still_pending: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(
        still_pending["records"][0]["status"], "pending",
        "dismissal and invalid cancellation must retain the request"
    );

    let marker = common::turn_status_marker(&terminal);
    submit(&mut terminal, &format!("/questions cancel {request_id}"));
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "explicit cancellation delivered",
    );
    let completed: Value = serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(completed["records"][0]["status"], "delivered");
    assert_eq!(completed["records"][0]["reply"]["kind"], "cancelled");
    assert!(env
        .captured_message_bodies()
        .last()
        .unwrap()
        .contains("Cancelled by the user"));
    submit(&mut terminal, "/exit");
    assert_eq!(terminal.expect_eof().unwrap(), 0);
}

#[test]
fn questions_command_explains_disabled_interactions_without_inference() {
    let env = TestEnv::new_mock("interaction-command-disabled");
    let mut terminal = env.spawn_with_env(
        &[],
        &[("SUDOCODE_EXPERIMENT_NONBLOCKING_INTERACTIONS", "0")],
    );
    common::expect_input_line_cleared(&terminal, env.timeout(), "disabled interactions ready");
    submit(&mut terminal, "/questions");
    terminal
        .expect("/questions requires nonblockingInteractions to be enabled")
        .unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "disabled command returned");
    assert_eq!(env.captured_message_bodies(), Vec::<String>::new());
    submit(&mut terminal, "/exit");
    assert_eq!(terminal.expect_eof().unwrap(), 0);
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn delegated_interaction(permission: bool, close_before_reply: bool) {
    let env = TestEnv::new(if close_before_reply {
        "worker-close"
    } else if permission {
        "worker-permission"
    } else {
        "worker-question"
    });
    let store = env.workspace_root().join("worker-results");
    let log = env.workspace_root().join("worker-requests.jsonl");
    let agents = env.workspace_root().join(".sudocode/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("human-fixture.md"),
        "---\nname: human-fixture\ndescription: Exercise delegated human input.\ntools: [bash, AskUserQuestion]\n---\nFollow the supplied task and report the actual tool result.\n").unwrap();
    std::fs::write(
        env.workspace_root().join("interaction-reference.txt"),
        "IndependentWorkerReadMarker",
    )
    .unwrap();
    std::fs::write(env.workspace_root().join("once.py"),
        "from pathlib import Path\np = Path(__file__).resolve().parent / 'worker-effect.txt'\nwith p.open('ab') as f: f.write(b'WorkerEffectMarker\\n')\nprint('WorkerEffectMarker')\n").unwrap();
    let python = common::resolve_python()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    let script = env
        .workspace_root()
        .join("once.py")
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    let child_calls = env.workspace_root().join("child-calls.json");
    let child_call = if permission {
        json!({"id":"child-operation", "name":"bash", "input":{"command":format!("'{python}' '{script}'")}})
    } else {
        json!({"id":"child-question", "name":"AskUserQuestion", "input":{"questions":[{"id":"q1", "prompt":"Which worker value?", "kind":"text"}]}})
    };
    std::fs::write(&child_calls, json!([child_call]).to_string()).unwrap();
    let child_prompt = if env.is_live() {
        if permission {
            format!("Call bash exactly once with command: '{python}' '{script}'. Report its actual result. Do not use any other tools or retry the command.")
        } else {
            "Call AskUserQuestion exactly once with one required text field: id q1, prompt 'Which worker value?'. Then report the exact answer without further tools.".into()
        }
    } else {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!({"calls_path":child_calls})
        )
    };
    let child_input =
        json!({"agent":"human-fixture", "run_in_background":false, "prompt":child_prompt});
    let mut host = Host::with_options(
        &env,
        false,
        &[
            ENABLED[0],
            ("SUDOCODE_AGENT_AUTO_BG_SECS", "0"),
            ("SUDOCODE_AGENT_STORE", store.to_str().unwrap()),
            ("SCODE_LOG_PATH", log.to_str().unwrap()),
        ],
    );
    control(&mut host, json!({"action":"configure", "enabled":true}));
    let parent_prompt = tool_batch(
        &env,
        &json!([
            {"id":"delegate-human", "name":"agent_spawn", "input":child_input},
            {"id":"parent-independent", "name":"read_file", "input":{"path":"interaction-reference.txt"}}
        ]),
        &format!("Call agent_spawn exactly once with this input: {child_input}. When it returns, independently read interaction-reference.txt and report the contents. Do not wait for the child, call pid_output, answer its question, or execute its command yourself. End your turn after the independent read."),
    );
    host.submit("delegate", &parent_prompt, "steer");
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    let pending = loop {
        let state = snapshot(&mut host);
        if let Some(request) = state["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["origin"]["kind"] == "worker")
        {
            break request.clone();
        }
        assert!(
            Instant::now() < deadline,
            "worker did not publish its request: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    host.idle();
    assert!(
        captured_tool_result_contains(&log, "IndependentWorkerReadMarker"),
        "parent did not continue independent work while its synchronous child awaited input"
    );
    assert!(!env.workspace_root().join("worker-effect.txt").exists());
    let worker = pending["origin"]["agent_id"].as_str().unwrap();
    let wait_prompt = tool_batch(
        &env,
        &json!([
            {"id":"wait-for-human-worker", "name":"pid_output", "input":{"pid":worker, "block":true, "timeout_ms":60000}}
        ]),
        &format!("Call pid_output exactly once with pid '{worker}', block true, timeout_ms 60000. Report its returned status and end the turn. Do not retry, poll, answer the worker, or use other tools."),
    );
    host.submit("chat-while-worker-waits", &wait_prompt, "steer");
    host.idle();
    assert!(
        captured_tool_result_contains(&log, "awaiting_human"),
        "blocking pid_output must return a human-wait status, not wait for its timeout"
    );
    assert_eq!(snapshot(&mut host)["requests"][0]["status"], "pending");
    finish_mock_question(&env);
    let reply = if permission {
        json!({"kind":"permission", "approved":true, "reason":"Approve the captured child operation once"})
    } else {
        json!({"kind":"answers", "answers":[{"id":"q1", "value":"WorkerChosenBlueUnique", "label":"WorkerChosenBlueUnique"}]})
    };
    let action = json!({"action":"reply", "interaction_id":pending["id"], "revision":pending["revision"], "reply":reply});
    if close_before_reply {
        let session = host.session.clone();
        host.close();
        drop(host);
        let mut host = resume(&env, &session);
        let state = control(&mut host, json!({"action":"configure", "enabled":true}));
        assert!(
            state["requests"].as_array().unwrap().is_empty(),
            "closed worker request was not retired: {state}"
        );
        let mut retired_reply = action;
        retired_reply["sessionId"] = json!(host.session);
        let rejected = host.response(METHOD, &retired_reply);
        assert!(
            rejected.get("error").is_some(),
            "old worker approval was accepted: {rejected}"
        );
        assert!(!env.workspace_root().join("worker-effect.txt").exists());
        host.close();
        return;
    }
    control(&mut host, action.clone());
    let manifest_path = store.join(format!("{worker}.json"));
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        if manifest["status"] == "completed" {
            break;
        }
        assert_ne!(manifest["status"], "failed", "{manifest}");
        assert!(
            Instant::now() < deadline,
            "worker did not finish: {manifest}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(snapshot(&mut host)["requests"], json!([]));
    control(&mut host, action);
    if permission {
        assert_eq!(
            std::fs::read_to_string(env.workspace_root().join("worker-effect.txt")).unwrap(),
            "WorkerEffectMarker\n"
        );
    } else {
        let session =
            std::fs::read_to_string(store.join(format!("{worker}.session.jsonl"))).unwrap();
        assert!(
            session.contains("WorkerChosenBlueUnique"),
            "the answer never reached the original worker"
        );
    }
    host.close();
}

fn captured_tool_result_contains(log: &std::path::Path, marker: &str) -> bool {
    request_evidence::accepted_messages(log).iter().any(|body| {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|message| {
                message["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|block| {
                        block["type"] == "tool_result"
                            && block["content"].to_string().contains(marker)
                    })
            })
    })
}

#[test]
fn delegated_permission_releases_parent_wait_and_resumes_only_the_child() {
    delegated_interaction(true, false);
}

#[test]
fn delegated_question_uses_session_inbox_without_reading_parent_stdin() {
    delegated_interaction(false, false);
}

#[test]
fn closing_parent_retires_worker_approval_and_resume_cannot_replay_it() {
    delegated_interaction(true, true);
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn direct_approval(approved: bool) {
    let env = TestEnv::new_mock(if approved {
        "direct-approval"
    } else {
        "direct-denial"
    });
    node_home::pin_agent_name(env.workspace_root(), "interaction-direct-actor");
    let log = env.workspace_root().join("direct-requests.jsonl");
    let mut host = Host::with_options(
        &env,
        false,
        &[ENABLED[0], ("SCODE_LOG_PATH", log.to_str().unwrap())],
    );
    control(&mut host, json!({"action":"configure", "enabled":true}));
    host.request(
        "session/setPermissionMode",
        &json!({"sessionId":host.session, "permissionMode":"prompt"}),
    );
    let submitted = host.request(
        "_sudocode/a2a/send",
        &json!({
            "sessionId":host.session, "to":"interaction-direct-peer",
            "message":"ExactDirectApprovalMarker", "summary":"A pending direct call",
        }),
    );
    let pending = snapshot(&mut host)["requests"][0].clone();
    assert_eq!(submitted["pendingInteraction"]["id"], pending["id"]);
    assert_eq!(pending["origin"]["kind"], "direct");
    assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
    // This acquires the normal host lane: a pending permission must release it.
    host.request(
        "session/setPermissionMode",
        &json!({"sessionId":host.session, "permissionMode":"prompt"}),
    );
    let root = runtime::mailbox::local_pair_root_in(env.config_home());
    let path = runtime::mailbox::Mailbox::workspace_local(&root, "interaction-direct-peer".into())
        .transcript_path("interaction-direct-actor");
    assert!(!std::fs::read_to_string(&path)
        .unwrap_or_default()
        .contains("ExactDirectApprovalMarker"));
    let reply = json!({
        "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
        "reply":{"kind":"permission", "approved":approved, "reason":"Explicit test decision"},
    });
    control(&mut host, reply.clone());
    wait_for_delivery(&mut host);
    let result = control(
        &mut host,
        json!({"action":"result", "interaction_id":pending["id"]}),
    );
    assert_eq!(result["status"], "delivered");
    assert_eq!(result["result"]["is_error"], !approved, "{result}");
    assert_eq!(
        result["result"]["audit"]["commandId"],
        submitted["_meta"]["sudocode"]["directA2a"]["audit"]["commandId"]
    );
    assert_eq!(
        result["result"]["audit"]["actor"],
        "interaction-direct-actor"
    );
    let transcript = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        transcript.matches("ExactDirectApprovalMarker").count(),
        usize::from(approved)
    );
    control(&mut host, reply.clone());
    host.idle();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap_or_default(),
        transcript,
        "duplicate approval cannot resend"
    );

    let session = host.session.clone();
    host.close();
    drop(host);
    let mut host = Host::with_options(
        &env,
        false,
        &[ENABLED[0], ("SCODE_LOG_PATH", log.to_str().unwrap())],
    );
    host.close();
    host.request(
        "session/load",
        &json!({"sessionId":session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    host.session = session;
    control(&mut host, json!({"action":"configure", "enabled":true}));
    control(&mut host, reply);
    assert_eq!(
        control(
            &mut host,
            json!({"action":"result", "interaction_id":pending["id"]})
        ),
        result
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap_or_default(),
        transcript,
        "reopening cannot resend"
    );
    host.close();
    assert_eq!(env.captured_message_bodies(), Vec::<String>::new());
    // The mock rejects requests lacking a routing marker before capture, so
    // also inspect transport evidence: even a failed inference is forbidden.
    for line in std::fs::read_to_string(&log).unwrap().lines() {
        let event: Value = serde_json::from_str(line).unwrap();
        if event["event"] == "request_debug" {
            assert!(
                !event["attributes"]["body"]["messages"].is_array(),
                "direct approval attempted model inference"
            );
        }
    }
}

#[test]
fn direct_host_permission_returns_pending_and_executes_once_without_inference() {
    direct_approval(true);
}

#[test]
fn direct_host_permission_denial_is_durable_without_inference() {
    direct_approval(false);
}

fn control(host: &mut Host, mut action: Value) -> Value {
    action["sessionId"] = json!(host.session);
    host.request(METHOD, &action)
}

fn snapshot(host: &mut Host) -> Value {
    control(host, json!({"action":"snapshot"}))
}

fn wait_for_request(host: &mut Host, label: &str) -> Value {
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        let state = snapshot(host);
        if let Some(request) = state["requests"].as_array().unwrap().first() {
            return request.clone();
        }
        let inputs = host.snapshot();
        assert!(
            inputs["running"] != false || !inputs["pending"].as_array().unwrap().is_empty(),
            "{label} turn ended without publishing a request: {state}; input state: {inputs}"
        );
        assert!(
            Instant::now() < deadline,
            "{label} was not published: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn ask(env: &TestEnv, host: &mut Host) -> Value {
    control(host, json!({"action":"configure", "enabled":true}));
    let prompt = question_prompt(env);
    host.submit("ask-colour", &prompt, "steer");
    let pending = wait_for_request(host, "question");
    // This is the critical difference from a waiting tool future: the turn
    // reaches its next model request and finishes with the question unanswered.
    host.idle();
    finish_mock_question(env);
    assert_eq!(snapshot(host)["requests"][0]["status"], "pending");
    if env.is_mock() {
        let requests = env.captured_message_bodies();
        assert!(
            requests.len() >= 2,
            "model continuation waited for an answer"
        );
        assert!(requests.last().unwrap().contains("pending"));
        assert!(requests
            .last()
            .unwrap()
            .contains(pending["id"].as_str().unwrap()));
    }
    pending
}

#[test]
fn legacy_prompt_accepts_a_durable_reply_while_its_tool_is_running() {
    let env = TestEnv::new_mock("legacy-prompt-durable-reply");
    let mut host = Host::with_options(&env, true, ENABLED);
    control(&mut host, json!({"action":"configure", "enabled":true}));
    let prompted = host.request(
        "session/prompt",
        &json!({
            "sessionId":host.session,
            "prompt":[{"type":"text","text":question_prompt(&env)}]
        }),
    );
    assert_eq!(prompted["stopReason"], "end_turn");
    finish_mock_question(&env);
    let pending = snapshot(&mut host)["requests"][0].clone();
    assert_eq!(pending["status"], "pending");

    std::fs::write(env.workspace_root().join("shared-input.sh"),
        "printf started > shared-input-started\ni=0\nwhile [ ! -f release-shared-input ] && [ $i -lt 1200 ]; do\n sleep 0.1\n i=$((i+1))\ndone\nprintf LegacyPromptToolFinished\n").unwrap();
    let _release = ReleaseSharedInput(env.workspace_root().to_path_buf());
    let prompt = tool_batch(
        &env,
        &json!([{"id":"legacy-held-tool","name":"bash","input":{
            "command":"sh shared-input.sh","timeout":180_000,"run_in_background":false
        }}]),
        "",
    );
    let running = host.send_request(
        "session/prompt",
        &json!({
            "sessionId":host.session,"prompt":[{"type":"text","text":prompt}]
        }),
    );
    common::expect_screen(
        &host.terminal,
        |_| env.workspace_root().join("shared-input-started").exists(),
        common::LIVE_TURN_BUDGET,
        "legacy prompt is executing its independent tool",
    );
    let replied = control(&mut host, answer(&pending));
    assert_eq!(replied["requests"][0]["status"], "resolved", "{replied}");
    assert!(!env.workspace_root().join("release-shared-input").exists());
    assert_eq!(host.runtime_status()["foreground"]["state"], "running");
    finish_mock_question(&env);
    std::fs::write(env.workspace_root().join("release-shared-input"), "release").unwrap();
    let finished = host.receive(running);
    assert_eq!(finished["result"]["stopReason"], "end_turn", "{finished}");
    wait_for_delivery(&mut host);
    assert!(env
        .captured_message_bodies()
        .iter()
        .any(|body| { body.contains("interaction_reply") && body.contains("blue") }));
    host.close();
}

fn question_prompt(env: &TestEnv) -> String {
    std::fs::write(
        env.workspace_root().join("interaction-reference.txt"),
        "Independent work is available.",
    )
    .unwrap();
    if env.is_live() {
        return "Use AskUserQuestion exactly once to ask 'Which colour?' with field id q1 and red then blue as options and values. Then independently read interaction-reference.txt and report its contents. Do not wait for my colour choice or guess it. Do not ask another question. Acknowledge later chat and interaction_reply inputs briefly without further tools.".into();
    }
    let calls = env.workspace_root().join("interaction-calls.json");
    std::fs::write(&calls, json!([
        {"id":"question-colour", "name":"AskUserQuestion", "input":{"questions":[{
            "id":"q1", "prompt":"Which colour?", "options":[{"label":"red","value":"red"},{"label":"blue","value":"blue"}]
        }]}},
        {"id":"independent-read", "name":"read_file", "input":{"path":"interaction-reference.txt"}}
    ]).to_string()).unwrap();
    mock_tool_batch_prompt(env)
}

fn finish_mock_question(env: &TestEnv) {
    if env.is_mock() {
        // The provider fixture's next turn has no more tool calls. The real
        // runtime must still deliver the saved decision and keep its identity.
        std::fs::write(env.workspace_root().join("interaction-calls.json"), "[]").unwrap();
    }
}

fn tool_batch(env: &TestEnv, calls: &Value, live: &str) -> String {
    if env.is_live() {
        return live.to_owned();
    }
    let path = env.workspace_root().join("interaction-calls.json");
    std::fs::write(&path, calls.to_string()).unwrap();
    mock_tool_batch_prompt(env)
}

fn mock_tool_batch_prompt(env: &TestEnv) -> String {
    format!(
        "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
        json!({"calls_path":env.workspace_root().join("interaction-calls.json")})
    )
}

fn wait_for_delivery(host: &mut Host) {
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        if snapshot(host)["requests"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "reply was not committed");
        std::thread::sleep(Duration::from_millis(30));
    }
    host.idle();
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn plan_review(action: &str) {
    let env = TestEnv::new(&format!("nonblocking-plan-{action}"));
    let plan_file = env.workspace_root().join("review-plan.md");
    let log = env.workspace_root().join("plan-requests.jsonl");
    std::fs::write(
        env.workspace_root().join("interaction-reference.txt"),
        "Independent plan research.",
    )
    .unwrap();
    let mut host = Host::with_options(
        &env,
        true,
        &[
            ENABLED[0],
            ("SUDOCODE_PLAN_FILE", plan_file.to_str().unwrap()),
            ("SCODE_LOG_PATH", log.to_str().unwrap()),
        ],
    );
    control(&mut host, json!({"action":"configure", "enabled":true}));
    let mut plan_content = String::from(
        "# Implementation\nWrite ApprovedImplementationMarker to implementation.txt when explicitly asked after approval.",
    );
    if env.is_mock() {
        // Clearing exploration must discard the original prompt. Keep the
        // mock's routing marker in the retained fixture document. Spaces must
        // surround it because the decision carries this document inside JSON:
        // escaped newlines are not whitespace to the mock's token parser.
        // A literal empty batch survives JSON encoding without escaped paths
        // or quotes: this acknowledgement must not issue another tool call.
        plan_content.push_str(
            "\n\nMock fixture PARITY_SCENARIO:tool_concurrency TOOL_BATCH:[] continuation.",
        );
    }
    let prompt = tool_batch(&env, &json!([
        {"id":"plan-review", "name":"write_plan", "input":{
            "context":"An isolated approval test.",
            "constraints":"Wait for approval before implementation.",
            "acceptance":"Create implementation.txt containing ApprovedImplementationMarker.",
            "content":plan_content
        }},
        {"id":"plan-independent-read", "name":"read_file", "input":{"path":"interaction-reference.txt"}},
        {"id":"plan-premature-write", "name":"write_file", "input":{"path":"implementation.txt", "content":"ApprovedImplementationMarker"}}
    ]), "Call write_plan exactly once with a short plan to create implementation.txt containing ApprovedImplementationMarker. Include context, constraints, acceptance and content; require approval before implementation. Then read interaction-reference.txt independently and report its contents. Do not create implementation.txt yet. After an interaction_reply, acknowledge it without tools until I explicitly ask you to implement. Do not put DiscardExplorationMarker in the plan.");
    host.submit(
        "request-plan",
        &format!("DiscardExplorationMarker\n{prompt}"),
        "steer",
    );
    let pending = wait_for_request(&mut host, "plan");
    host.idle();
    finish_mock_question(&env);
    assert_eq!(pending["status"], "pending");
    assert_eq!(
        pending["request"]["request"]["presentation"]["PlanReview"]["plan_file"],
        json!(plan_file),
    );
    let document = std::fs::read_to_string(&plan_file).unwrap();
    assert_eq!(
        pending["request"]["request"]["description"]["Markdown"], document,
        "review uses the exact saved document",
    );
    assert!(
        !env.workspace_root().join("implementation.txt").exists(),
        "implementation must not run before approval",
    );
    let requests = if env.is_mock() {
        env.captured_message_bodies()
    } else {
        request_evidence::accepted_messages(&log)
            .iter()
            .map(Value::to_string)
            .collect()
    };
    assert!(requests
        .last()
        .unwrap()
        .contains("Independent plan research."));
    if env.is_mock() {
        assert!(
            requests.last().unwrap().contains("awaiting approval"),
            "the engine must reject the premature write, even with full tool permissions",
        );
    }
    let decision = json!({
        "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
        "reply":{"kind":"answers", "answers":[{"id":"action", "value":action, "label":null}]},
    });
    // Editing the file out of band invalidates the captured approval. Restoring
    // its exact bytes makes the original immutable review eligible again.
    std::fs::write(&plan_file, format!("{document}\nChanged after review.\n")).unwrap();
    let mut stale = decision.clone();
    stale["sessionId"] = json!(host.session);
    assert!(host.response(METHOD, &stale).get("error").is_some());
    assert_eq!(snapshot(&mut host)["requests"][0]["status"], "pending");
    std::fs::write(&plan_file, &document).unwrap();
    control(&mut host, decision.clone());
    wait_for_delivery(&mut host);
    let requests: Vec<Value> = if env.is_mock() {
        env.captured_message_bodies()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect()
    } else {
        request_evidence::accepted_messages(&log)
    };
    let messages = requests.last().unwrap()["messages"].to_string();
    assert!(
        messages.contains("interaction_reply"),
        "approval continuation did not reach the model; messages: {messages}; host: {}",
        host.terminal.render(common::screen_contents),
    );
    assert!(messages.contains("ApprovedImplementationMarker"));
    assert_eq!(messages.contains("DiscardExplorationMarker"), action == "2");

    let prompt = tool_batch(&env, &json!([
        {"id":"plan-approved-write", "name":"write_file", "input":{"path":"implementation.txt", "content":"ApprovedImplementationMarker"}}
    ]), "Now implement the approved plan: use write_file to create implementation.txt with exactly ApprovedImplementationMarker. No other tools. Then stop.");
    host.submit("implement-approved-plan", &prompt, "steer");
    host.idle();
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("implementation.txt")).unwrap(),
        "ApprovedImplementationMarker",
    );
    finish_mock_question(&env);
    let before = env.is_mock().then(|| env.captured_message_bodies().len());
    control(&mut host, decision);
    host.idle();
    if let Some(before) = before {
        assert_eq!(
            env.captured_message_bodies().len(),
            before,
            "duplicate approval must not clear context or execute another turn"
        );
    }
    host.close();
}

#[test]
fn plan_review_allows_reads_and_gates_writes_until_exact_approval() {
    plan_review("2");
}

#[test]
fn plan_approval_can_clear_exploration_without_losing_the_approved_document() {
    plan_review("1");
}

fn request_permission(env: &TestEnv, host: &mut Host) -> Value {
    control(host, json!({"action":"configure", "enabled":true}));
    std::fs::write(
        env.workspace_root().join("permission-reference.txt"),
        "Work continues while approval waits.",
    )
    .unwrap();
    let prompt = tool_batch(env, &json!([
        {"id":"deferred-append", "name":"bash", "input":{"command":"printf approved >> permission-count.txt"}},
        {"id":"permission-independent-read", "name":"read_file", "input":{"path":"permission-reference.txt"}}
    ]), "Use bash exactly once to run `printf approved >> permission-count.txt`. If it needs permission, leave the request pending and do not retry. Independently use read_file on permission-reference.txt and report its contents. On later interaction_reply inputs, report approved_operation_result without executing any more tools.");
    host.submit("request-permission", &prompt, "steer");
    let pending = wait_for_request(host, "permission request");
    host.idle();
    finish_mock_question(env);
    assert_eq!(pending["request"]["kind"], "permission");
    assert!(!env.workspace_root().join("permission-count.txt").exists());
    if env.is_mock() {
        let requests = env.captured_message_bodies();
        assert!(requests
            .last()
            .unwrap()
            .contains("Work continues while approval waits."));
        assert!(requests.last().unwrap().contains("pending_approval"));
    }
    pending
}

#[test]
fn permission_reply_resumes_the_captured_operation_once_without_a_model_retry() {
    let env = TestEnv::new("nonblocking-permission-approve");
    let mut host = Host::with_options(&env, false, ENABLED);
    let pending = request_permission(&env, &mut host);
    let reply = json!({
        "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
        "reply":{"kind":"permission", "approved":true, "reason":null},
    });
    control(&mut host, reply.clone());
    wait_for_delivery(&mut host);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("permission-count.txt")).unwrap(),
        "approved"
    );
    if env.is_mock() {
        let requests = env.captured_message_bodies();
        assert!(requests
            .last()
            .unwrap()
            .contains("approved_operation_result"));
    }
    let session = host.session.clone();
    host.close();
    drop(host);
    // Reopening with the same restrictive permission mode retains the completed
    // decision; even an identical repeated approval cannot append another time.
    let mut host = Host::with_options(&env, false, ENABLED);
    host.close();
    host.request(
        "session/load",
        &json!({"sessionId":session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    host.session = session;
    control(&mut host, json!({"action":"configure", "enabled":true}));
    control(&mut host, reply);
    host.idle();
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("permission-count.txt")).unwrap(),
        "approved"
    );
    host.close();
}

#[test]
fn approval_saved_during_tool_pause_resumes_only_when_control_is_returned() {
    let env = TestEnv::new("nonblocking-permission-paused");
    let mut host = Host::with_options(&env, false, ENABLED);
    let pending = request_permission(&env, &mut host);
    let paused = host.request(
        "_sudocode/session/pause_tools",
        &json!({"sessionId":host.session}),
    );
    assert_eq!(paused["isQuiescent"], true);
    let before = env.is_mock().then(|| env.captured_message_bodies().len());
    let saved = control(
        &mut host,
        json!({
            "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
            "reply":{"kind":"permission", "approved":true, "reason":null},
        }),
    );
    assert_eq!(saved["requests"][0]["status"], "resolved");
    host.idle();
    assert!(!env.workspace_root().join("permission-count.txt").exists());
    if let Some(before) = before {
        assert_eq!(
            env.captured_message_bodies().len(),
            before,
            "saving approval must not bypass tool pause"
        );
    }
    host.request(
        "_sudocode/session/resume_tools",
        &json!({"sessionId":host.session, "pauseEpoch":paused["pauseEpoch"]}),
    );
    wait_for_delivery(&mut host);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("permission-count.txt")).unwrap(),
        "approved"
    );
    host.close();
}

#[test]
fn denied_permission_never_executes_the_captured_operation() {
    let env = TestEnv::new("nonblocking-permission-deny");
    let mut host = Host::with_options(&env, false, ENABLED);
    let pending = request_permission(&env, &mut host);
    control(
        &mut host,
        json!({
            "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
            "reply":{"kind":"permission", "approved":false, "reason":"Do not run it"},
        }),
    );
    wait_for_delivery(&mut host);
    assert!(!env.workspace_root().join("permission-count.txt").exists());
    host.close();
}

#[test]
fn interrupted_permission_claim_is_reported_without_repeating_its_effect() {
    let env = TestEnv::new("nonblocking-permission-uncertain");
    let mut host = Host::with_options(&env, false, ENABLED);
    let pending = request_permission(&env, &mut host);
    let session = host.session.clone();
    host.close();
    drop(host);

    // Recreate the interruption window after a side effect and before its
    // result is durable. Only this disposable session and fixture are changed.
    let journal_path = interaction_journals(&env)
        .into_iter()
        .find(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            journal["session_id"] == session
        })
        .unwrap();
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    let record = journal["records"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["id"] == pending["id"])
        .unwrap();
    record["revision"] = json!(pending["revision"].as_u64().unwrap() + 1);
    record["status"] = json!("resolved");
    record["reply"] = json!({"kind":"permission", "approved":true, "reason":null});
    record["execution"] = json!({"state":"claimed"});
    std::fs::write(journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    std::fs::write(
        env.workspace_root().join("permission-count.txt"),
        "approved",
    )
    .unwrap();

    let mut host = Host::with_options(&env, false, ENABLED);
    host.close();
    host.request(
        "session/load",
        &json!({"sessionId":session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    host.session = session;
    control(&mut host, json!({"action":"configure", "enabled":true}));
    wait_for_delivery(&mut host);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("permission-count.txt")).unwrap(),
        "approved"
    );
    if env.is_mock() {
        let requests = env.captured_message_bodies();
        assert!(requests.last().unwrap().contains("outcome is unknown"));
    }
    host.close();
}

#[test]
fn terminal_permission_review_preserves_the_draft_and_executes_only_after_approval() {
    let env = TestEnv::new("nonblocking-terminal-permission");
    let mut terminal = env.spawn_with_env(&["--permission-mode", "workspace-write"], ENABLED);
    terminal.resize(60, 140).unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "permission chat ready");
    let prompt = tool_batch(&env, &json!([
        {"id":"terminal-append", "name":"bash", "input":{"command":"printf approved >> permission-count.txt"}}
    ]), "Use bash exactly once to run `printf approved >> permission-count.txt`. If permission is pending, do not retry it or ask a question. On a later interaction_reply, report approved_operation_result without further tools.");
    let marker = common::turn_status_marker(&terminal);
    submit(&mut terminal, &prompt);
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "permission request releases the model turn",
    );
    finish_mock_question(&env);
    assert!(!env.workspace_root().join("permission-count.txt").exists());
    terminal.send("UnsentPermissionDraft").unwrap();
    common::expect_screen(
        &terminal,
        |screen| common::input_line_of(screen).contains("UnsentPermissionDraft"),
        env.timeout(),
        "permission leaves draft available",
    );
    terminal.send("\x1bq").unwrap();
    common::expect_screen(
        &terminal,
        |screen| screen.contains("Allow this tool call?") && screen.contains("Allow once"),
        env.timeout(),
        "explicit approval review",
    );
    let marker = common::turn_status_marker(&terminal);
    terminal.send("1").unwrap();
    common::expect_turn_complete_with_input(
        &terminal,
        &marker,
        "UnsentPermissionDraft",
        common::LIVE_TURN_BUDGET,
        "approved operation resumes",
    );
    common::expect_screen(
        &terminal,
        |screen| common::input_line_of(screen).contains("UnsentPermissionDraft"),
        env.timeout(),
        "approval does not submit draft",
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("permission-count.txt")).unwrap(),
        "approved"
    );
    terminal.send("\x15").unwrap();
    submit(&mut terminal, "/exit");
    assert_eq!(terminal.expect_eof().unwrap(), 0);
}

#[test]
fn terminal_cancel_before_reply_commit_retains_it_without_repeating_the_operation() {
    let env = TestEnv::new("nonblocking-terminal-cancelled-approval");
    let log = env
        .workspace_root()
        .join("cancelled-approval-requests.jsonl");
    let mut terminal = env.spawn_with_env(
        &["--permission-mode", "workspace-write"],
        &[ENABLED[0], ("SCODE_LOG_PATH", log.to_str().unwrap())],
    );
    terminal.resize(60, 140).unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "permission chat ready");
    let command = "printf approved >> permission-count.txt; while true; do sleep 1; done";
    let prompt = tool_batch(&env, &json!([
        {"id":"cancelled-append", "name":"bash", "input":{"command":command}}
    ]), &format!("Use bash exactly once to run `{command}`. I will cancel it after the file appears. If approval is pending, end your turn without retrying. On later interaction_reply inputs, report the saved result briefly without using any tools or retrying the operation."));
    let marker = common::turn_status_marker(&terminal);
    submit(&mut terminal, &prompt);
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "permission pending",
    );
    finish_mock_question(&env);
    terminal.send("\x1bq").unwrap();
    common::expect_screen(
        &terminal,
        |view| view.contains("Allow this tool call?"),
        env.timeout(),
        "approval picker",
    );
    terminal.send("1").unwrap();
    let count = env.workspace_root().join("permission-count.txt");
    common::expect_screen(
        &terminal,
        |_| count.exists(),
        env.timeout(),
        "approved operation starts",
    );
    terminal.send("\x03").unwrap();
    common::expect_screen(
        &terminal,
        |view| view.contains("queued: Answered request"),
        env.timeout(),
        "uncommitted approval stays queued",
    );
    let journal_path = interaction_journals(&env).pop().expect("session journal");
    let journal: Value = serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    let record = journal["records"].as_array().unwrap().first().unwrap();
    assert_eq!(
        record["status"], "resolved",
        "reply must not be acknowledged before prompt commit"
    );
    assert_eq!(
        record["execution"]["state"], "finished",
        "the live executor must save its cancellation result before yielding"
    );
    let marker = common::turn_status_marker(&terminal);
    terminal.send("\x1b").unwrap();
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "explicit retry consumes retained reply",
    );
    assert_eq!(
        std::fs::read_to_string(count).unwrap(),
        "approved",
        "cancelled operation must not execute twice"
    );
    let journal: Value = serde_json::from_slice(&std::fs::read(journal_path).unwrap()).unwrap();
    assert_eq!(journal["records"][0]["status"], "delivered");
    let requests: Vec<Value> = if env.is_mock() {
        env.captured_message_bodies()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect()
    } else {
        request_evidence::accepted_messages(&log)
    };
    assert!(requests.last().unwrap()["messages"]
        .to_string()
        .contains("Command interrupted by user"));
    submit(&mut terminal, "/exit");
    assert_eq!(terminal.expect_eof().unwrap(), 0);
}

fn submit(session: &mut pty_expect::PtySession, text: &str) {
    session.send(&format!("\x1b[200~{text}\x1b[201~")).unwrap();
    common::expect_screen(
        session,
        |view| !common::input_line_of(view).is_empty(),
        common::DEFAULT_TIMEOUT,
        "draft entered",
    );
    session.send("\r").unwrap();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn terminal_question_preserves_chat_draft_and_cursor_until_explicit_review() {
    let env = TestEnv::new("nonblocking-terminal-question");
    let log = env.workspace_root().join("interaction-requests.jsonl");
    let mut terminal = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[ENABLED[0], ("SCODE_LOG_PATH", log.to_str().unwrap())],
    );
    terminal.resize(60, 140).unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "initial chat input");
    let marker = common::turn_status_marker(&terminal);
    submit(&mut terminal, &question_prompt(&env));
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "independent work finishes without an answer",
    );
    finish_mock_question(&env);
    common::expect_input_line_cleared(
        &terminal,
        env.timeout(),
        "question does not take the composer",
    );

    terminal.send("DraftKeepAB").unwrap();
    common::expect_screen(
        &terminal,
        |s| common::input_line_of(s).contains("DraftKeepAB"),
        env.timeout(),
        "unsent chat draft",
    );
    terminal.send("\x1b[D\x1b[D\x1bq").unwrap();
    common::expect_screen(
        &terminal,
        |s| s.contains("Which colour?") && s.contains("[1] red"),
        env.timeout(),
        "explicit question focus",
    );
    terminal.send("\x1b").unwrap();
    common::expect_screen(
        &terminal,
        |s| common::input_line_of(s).contains("DraftKeepAB"),
        env.timeout(),
        "Escape restores chat draft",
    );
    terminal.send("X").unwrap();
    common::expect_screen(
        &terminal,
        |s| common::input_line_of(s).contains("DraftKeepXAB"),
        env.timeout(),
        "review preserves edit cursor",
    );
    let marker = common::turn_status_marker(&terminal);
    terminal.send("\r").unwrap();
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "ordinary chat works while the question is pending",
    );

    let journals = interaction_journals(&env);
    assert_eq!(journals.len(), 1, "one original session owns the question");
    let journal: Value = serde_json::from_slice(&std::fs::read(&journals[0]).unwrap()).unwrap();
    let original_session = journal["session_id"].as_str().unwrap();
    submit(&mut terminal, "/clear --confirm");
    common::expect_screen(
        &terminal,
        |s| s.contains("Session cleared"),
        env.timeout(),
        "fresh session selected",
    );
    common::expect_input_line_cleared(&terminal, env.timeout(), "fresh session composer");
    terminal.send("\x1bq").unwrap();
    common::expect_screen(
        &terminal,
        |s| s.contains("No matching question"),
        env.timeout(),
        "old question stays in its original session",
    );
    submit(
        &mut terminal,
        &format!("/session switch {original_session}"),
    );
    common::expect_screen(
        &terminal,
        |s| s.contains("Session switched"),
        env.timeout(),
        "original session restored",
    );
    common::expect_input_line_cleared(&terminal, env.timeout(), "restored session composer");

    terminal.send("UnsentAfterAnswer").unwrap();
    common::expect_screen(
        &terminal,
        |s| common::input_line_of(s).contains("UnsentAfterAnswer"),
        env.timeout(),
        "next chat draft",
    );
    terminal.send("\x1bq").unwrap();
    common::expect_screen(
        &terminal,
        |s| s.contains("Which colour?") && s.contains("[2] blue"),
        env.timeout(),
        "reopen unanswered question",
    );
    let marker = common::turn_status_marker(&terminal);
    terminal.send("2").unwrap();
    common::expect_turn_complete_with_input(
        &terminal,
        &marker,
        "UnsentAfterAnswer",
        common::LIVE_TURN_BUDGET,
        "late answer starts continuation",
    );
    common::expect_screen(
        &terminal,
        |s| common::input_line_of(s).contains("UnsentAfterAnswer"),
        env.timeout(),
        "answer leaves unsent chat intact",
    );

    let requests: Vec<Value> = if env.is_mock() {
        env.captured_message_bodies()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect()
    } else {
        request_evidence::accepted_messages(&log)
    };
    assert!(
        requests.len() >= 4,
        "question, independent continuation, chat, and answer reached the model"
    );
    let messages = requests.last().unwrap()["messages"].to_string();
    assert!(
        messages.contains("Independent work is available."),
        "independent read result reached model"
    );
    assert!(
        messages.contains("DraftKeepXAB"),
        "ordinary chat reached model while pending"
    );
    assert!(
        messages.contains("interaction_reply") && messages.contains("blue"),
        "late answer reached model"
    );
    assert!(
        !messages.contains("UnsentAfterAnswer"),
        "review never submits the chat draft"
    );
    terminal.send("\x15").unwrap();
    submit(&mut terminal, "/clear --confirm");
    common::expect_screen(
        &terminal,
        |view| view.contains("Session cleared"),
        env.timeout(),
        "leave original session before deletion",
    );
    common::expect_input_line_cleared(&terminal, env.timeout(), "new session ready for deletion");
    submit(
        &mut terminal,
        &format!("/session delete {original_session} --force"),
    );
    common::expect_screen(
        &terminal,
        |view| view.contains("Session deleted"),
        env.timeout(),
        "delete original session",
    );
    common::expect_screen(
        &terminal,
        |_| !journals[0].exists(),
        env.timeout(),
        "interaction journal retires with its session",
    );
    submit(&mut terminal, "/exit");
    assert_eq!(terminal.expect_eof().unwrap(), 0);
}

fn answer(pending: &Value) -> Value {
    json!({
        "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
        "reply":{"kind":"answers", "answers":[{"id":"q1", "value":"blue", "label":"blue"}]},
    })
}

#[test]
fn unanswered_question_allows_continuation_and_reply_is_consumed_once() {
    let env = TestEnv::new("nonblocking-question-reply");
    let mut host = Host::with_options(&env, true, ENABLED);
    let pending = ask(&env, &mut host);

    let mut stale = answer(&pending);
    stale["revision"] = json!(999);
    stale["sessionId"] = json!(host.session);
    assert!(host.response(METHOD, &stale).get("error").is_some());
    assert_eq!(snapshot(&mut host)["requests"][0]["status"], "pending");

    let mut wrong_kind = answer(&pending);
    wrong_kind["sessionId"] = json!(host.session);
    wrong_kind["reply"] = json!({"kind":"permission", "approved":true, "reason":null});
    assert!(
        host.response(METHOD, &wrong_kind).get("error").is_some(),
        "a question answer cannot become a permission grant"
    );
    assert_eq!(snapshot(&mut host)["requests"][0]["status"], "pending");

    let reply = answer(&pending);
    control(&mut host, reply.clone());
    // Observe the durable delivery receipt, not just successful RPC admission.
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        if snapshot(&mut host)["requests"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        assert!(Instant::now() < deadline, "answer was not committed");
        std::thread::sleep(Duration::from_millis(30));
    }
    host.idle();
    let before = env.is_mock().then(|| env.captured_message_bodies().len());
    control(&mut host, reply);
    host.idle();
    if let Some(before) = before {
        let requests = env.captured_message_bodies();
        assert_eq!(
            requests.len(),
            before,
            "duplicate reply started another turn"
        );
        let last: Value = serde_json::from_str(requests.last().unwrap()).unwrap();
        let messages = last["messages"].to_string();
        assert!(messages.contains("interaction_reply") && messages.contains("blue"));
    }
    let session = host.session.clone();
    host.close();
    drop(host);

    // Recreate the commit-before-ack crash window on this disposable session:
    // the real transcript retains its input receipt, while the sidecar still
    // says resolved. Reopening must reconcile it without another model request.
    let journal_path = interaction_journals(&env)
        .into_iter()
        .find(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            journal["session_id"] == session
        })
        .expect("saved interaction journal in isolated workspace");
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    let record = journal["records"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["id"] == pending["id"])
        .unwrap();
    assert_eq!(record["status"], "delivered");
    record["status"] = json!("resolved");
    std::fs::write(journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let before = env.is_mock().then(|| env.captured_message_bodies().len());
    let mut resumed = resume(&env, &session);
    let restored = control(&mut resumed, json!({"action":"configure", "enabled":true}));
    assert!(
        restored["requests"].as_array().unwrap().is_empty(),
        "committed receipt prevents redelivery: {restored}"
    );
    resumed.idle();
    if let Some(before) = before {
        assert_eq!(
            env.captured_message_bodies().len(),
            before,
            "recovery must not start another model turn"
        );
    }
    resumed.close();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected question, queued compaction and chat journey together."
)]
fn question_reply_and_later_chat_survive_a_refused_queued_compaction() {
    let env = TestEnv::new_mock("interaction-queued-compaction");
    let mut terminal = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[ENABLED[0], ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    terminal.resize(60, 140).unwrap();
    common::expect_input_line_cleared(&terminal, env.timeout(), "initial chat input");
    let marker = common::turn_status_marker(&terminal);
    submit(&mut terminal, &question_prompt(&env));
    common::expect_turn_complete_after(
        &terminal,
        &marker,
        common::LIVE_TURN_BUDGET,
        "independent work finishes with an unanswered question",
    );
    finish_mock_question(&env);
    let journals = interaction_journals(&env);
    assert_eq!(journals.len(), 1);

    // Hold a real tool so the queued command, later chat and explicit reply
    // reach the same coordinator before the turn can finish.
    std::fs::write(env.workspace_root().join("shared-input.sh"),
        "printf started > shared-input-started\ni=0\nwhile [ ! -f release-shared-input ] && [ $i -lt 1200 ]; do\n sleep 0.1\n i=$((i+1))\ndone\nprintf SharedInputToolFinished\n").unwrap();
    let _release = ReleaseSharedInput(env.workspace_root().to_path_buf());
    let prompt = tool_batch(
        &env,
        &json!([{"id":"held-question-tool","name":"bash","input":{
            "command":"sh shared-input.sh","timeout":180_000,"run_in_background":false
        }}]),
        "",
    );
    submit(&mut terminal, &prompt);
    common::expect_screen(
        &terminal,
        |_| env.workspace_root().join("shared-input-started").exists(),
        common::LIVE_TURN_BUDGET,
        "independent tool is running",
    );
    submit(&mut terminal, "/compact");
    common::expect_screen(
        &terminal,
        |view| view.contains("queued: /compact"),
        env.timeout(),
        "compaction waits for the running tool",
    );
    terminal.send("ChatAfterQuestionCompaction").unwrap();
    common::expect_input_line(
        &terminal,
        "ChatAfterQuestionCompaction",
        env.timeout(),
        "later chat draft",
    );
    terminal.send("\t").unwrap();
    common::expect_screen(
        &terminal,
        |view| {
            common::input_line_of(view).is_empty()
                && view.contains("queued: ChatAfterQuestionCompaction")
        },
        env.timeout(),
        "later chat waits behind compaction",
    );
    terminal.send("\x1bq").unwrap();
    common::expect_screen(
        &terminal,
        |view| view.contains("Which colour?") && view.contains("[2] blue"),
        env.timeout(),
        "question can be reviewed while the tool is running",
    );
    terminal.send("2").unwrap();
    common::expect_screen(
        &terminal,
        |_| {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(&journals[0]).unwrap()).unwrap();
            saved["records"][0]["status"] == "resolved"
        },
        env.timeout(),
        "reply is durable before the independent tool finishes",
    );
    finish_mock_question(&env);
    std::fs::write(env.workspace_root().join("release-shared-input"), "release").unwrap();
    // This short conversation is entirely protected recent history. A
    // refused compaction must retain the later chat and pause it until the
    // user explicitly resumes; the answer already belongs to the session.
    common::expect_screen(
        &terminal,
        |view| {
            view.contains("retention_limited")
                && view.contains("history preserved")
                && view.contains("queued: ChatAfterQuestionCompaction")
        },
        env.timeout(),
        "refused compaction preserves and pauses the later chat",
    );
    assert!(env
        .captured_message_bodies()
        .iter()
        .all(|body| !body.contains("ChatAfterQuestionCompaction")));
    let saved: Value = serde_json::from_slice(&std::fs::read(&journals[0]).unwrap()).unwrap();
    assert_eq!(saved["records"][0]["status"], "delivered");
    terminal.send("\x1b").unwrap();
    common::expect_screen(
        &terminal,
        |_| {
            env.captured_message_bodies()
                .iter()
                .any(|body| body.contains("ChatAfterQuestionCompaction"))
        },
        common::LIVE_TURN_BUDGET,
        "explicit resume delivers the chat retained after compaction refusal",
    );
    let screen = common::expect_screen(
        &terminal,
        |view| view.contains("Estimated tokens") && !view.contains("queued: "),
        env.timeout(),
        "the preserved queue drains after explicit resume",
    );
    let report = screen.find("Estimated tokens").unwrap();
    assert!(
        screen
            .find("ChatAfterQuestionCompaction")
            .is_some_and(|message| message > report),
        "later chat must run after the compaction report:\n{screen}"
    );
    let bodies = env.captured_message_bodies();
    assert!(bodies.iter().all(|body| !body.contains("/compact")));
    assert!(bodies
        .iter()
        .any(|body| body.contains("interaction_reply") && body.contains("blue")));
    let saved: Value = serde_json::from_slice(&std::fs::read(&journals[0]).unwrap()).unwrap();
    assert_eq!(saved["records"].as_array().unwrap().len(), 1);
    assert_eq!(saved["records"][0]["status"], "delivered");
    common::expect_input_line_cleared(&terminal, env.timeout(), "idle before exit");
    submit(&mut terminal, "/exit");
    assert_eq!(terminal.expect_eof().unwrap(), 0);
}

fn interaction_journals(env: &TestEnv) -> Vec<std::path::PathBuf> {
    let mut directories = vec![env.workspace_root().to_path_buf()];
    let mut journals = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                directories.push(entry.path());
            } else if entry
                .file_name()
                .to_string_lossy()
                .ends_with(".interactions.json")
            {
                journals.push(entry.path());
            }
        }
    }
    journals
}

/// Hold the same native commit lock used by the real process, without a fake
/// backend or a production delay hook. The fixture starts from one root context.
fn lock_interaction_writer(
    env: &TestEnv,
    session: &str,
) -> std::sync::Arc<dyn runtime::fs_backend::FsLock> {
    use runtime::fs_backend::{FsBackend, StdFsBackend};
    let journal = interaction_journals(env)
        .into_iter()
        .find(|path| {
            let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            value["session_id"] == session
        })
        .unwrap();
    let name = journal.file_name().unwrap().to_str().unwrap();
    let transcript = name.strip_suffix(".interactions.json").unwrap();
    let path = journal.with_file_name(format!("{transcript}.writer.lock"));
    StdFsBackend
        .try_lock(&path.to_string_lossy(), 30_000)
        .unwrap()
        .expect("the completed fixture turn released the native writer")
}

#[test]
fn contended_interaction_storage_keeps_status_and_chat_responsive_with_bounded_controls() {
    let env = TestEnv::new_mock("interaction-storage-contention");
    let mut host = Host::with_options(&env, true, ENABLED);
    let pending = ask(&env, &mut host);
    let lock = lock_interaction_writer(&env, &host.session);
    let mut reply = answer(&pending);
    reply["sessionId"] = json!(host.session);
    let reply_id = host.send_request(METHOD, &reply);

    // Saturation establishes that the control lane is occupied, rather than
    // relying on a sleep to guess when the first storage call has started.
    let mut outstanding = std::collections::HashSet::new();
    outstanding.insert(reply_id);
    for _ in 0..48 {
        outstanding.insert(host.send_request(
            METHOD,
            &json!({
                "sessionId":host.session,"action":"snapshot",
            }),
        ));
    }
    host.terminal.set_default_timeout(Duration::from_secs(2));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let response = host.receive_next();
        outstanding.remove(&response["id"].as_u64().unwrap());
        assert_ne!(
            response["id"], reply_id,
            "a held writer must not acknowledge a reply: {response}"
        );
        if response.get("error").is_some() {
            assert!(
                response
                    .to_string()
                    .contains("Interaction controls are busy"),
                "{response}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unbounded controls never refused saturation"
        );
    }
    assert_eq!(host.runtime_status()["foreground"]["state"], "idle");
    let admitted = host.submit("chat-during-storage-wait", "StorageWaitChatMarker", "steer");
    assert!(
        admitted.get("state").is_some(),
        "chat was not admitted: {admitted}"
    );
    let inputs = host.snapshot();
    assert!(
        inputs["running"] == true
            || inputs["pending"]
                .as_array()
                .unwrap()
                .iter()
                .any(|input| input["clientId"] == "chat-during-storage-wait"),
        "{inputs}"
    );
    drop(lock);
    host.terminal.set_default_timeout(common::LIVE_TURN_BUDGET);
    for id in outstanding {
        let response = host.receive(id);
        if id == reply_id {
            assert!(response.get("error").is_none(), "{response}");
        } else if response.get("error").is_some() {
            assert!(
                response
                    .to_string()
                    .contains("Interaction controls are busy"),
                "{response}"
            );
        }
    }
    wait_for_delivery(&mut host);
    let requests = env.captured_message_bodies();
    let last: Value = serde_json::from_str(requests.last().unwrap()).unwrap();
    let messages = last["messages"].to_string();
    assert!(
        messages.contains("StorageWaitChatMarker"),
        "queued chat was lost"
    );
    assert!(messages.contains("interaction_reply") && messages.contains("blue"));
    let before = requests.len();
    control(&mut host, answer(&pending));
    host.idle();
    assert_eq!(
        env.captured_message_bodies().len(),
        before,
        "duplicate answer started another turn"
    );
    host.close();
}

#[test]
fn contended_turn_admission_keeps_cancel_and_lifecycle_controls_responsive() {
    contended_turn_admission(false);
}

#[test]
fn contended_turn_admission_preserves_queued_input_on_explicit_cancel() {
    contended_turn_admission(true);
}

fn contended_turn_admission(explicit_cancel: bool) {
    let env = TestEnv::new_mock("interaction-turn-storage-contention");
    let mut host = Host::with_options(&env, true, ENABLED);
    let pending = ask(&env, &mut host);
    let lock = lock_interaction_writer(&env, &host.session);
    let before = env.captured_message_bodies().len();
    host.terminal.set_default_timeout(Duration::from_secs(2));
    host.submit(
        "blocked-admission",
        "CancelledBeforeAdmissionMarker",
        "steer",
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while host.runtime_status()["foreground"]["state"] != "running" {
        assert!(Instant::now() < deadline, "turn admission did not start");
    }
    assert_eq!(
        env.captured_message_bodies().len(),
        before,
        "the model must wait for durable turn admission"
    );
    let replacement = host.response(
        "session/load",
        &json!({"sessionId":host.session,"cwd":env.workspace_root(),"mcpServers":[]}),
    );
    assert!(
        replacement["error"]["data"]
            .as_str()
            .is_some_and(|message| message.contains("busy")),
        "session replacement must refuse while admission owns the turn: {replacement}"
    );
    if explicit_cancel {
        host.submit("later-input", "PreservedLaterInputMarker", "queue");
        let flushed = host.input(json!({"action":"flush_or_cancel"}));
        assert_eq!(flushed["cancelled"], false, "{flushed}");
        assert_eq!(flushed["state"]["pending"].as_array().unwrap().len(), 2);
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        host.terminal
            .send(&format!(
                "{}{newline}",
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":host.session}})
            ))
            .unwrap();
    } else {
        let cancelled = host.input(json!({"action":"flush_or_cancel"}));
        assert_eq!(cancelled["cancelled"], true, "{cancelled}");
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = host.snapshot();
        if state["paused"] == true {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancellation was not admitted: {state}"
        );
    }
    drop(lock);
    host.terminal.set_default_timeout(common::LIVE_TURN_BUDGET);
    host.idle();
    assert_eq!(
        env.captured_message_bodies().len(),
        before,
        "cancellation during admission must not start a model request"
    );
    assert_eq!(snapshot(&mut host)["requests"][0]["status"], "pending");
    if explicit_cancel {
        let queued = host.snapshot();
        assert_eq!(queued["paused"], true, "{queued}");
        assert!(
            queued["pending"].as_array().unwrap().iter().any(|item| {
                item["clientId"] == "later-input" && item["display"] == "PreservedLaterInputMarker"
            }),
            "{queued}"
        );
    }
    control(&mut host, answer(&pending));
    host.input(json!({"action":"flush"}));
    wait_for_delivery(&mut host);
    assert_eq!(snapshot(&mut host)["requests"], json!([]));
    if explicit_cancel {
        let bodies = env.captured_message_bodies();
        assert!(bodies.last().unwrap().contains("PreservedLaterInputMarker"));
    }
    host.close();
}

#[test]
fn restoring_an_older_context_recovers_receipts_committed_in_another_context() {
    use runtime::fs_backend::{FsBackend, RecordLogMode, StdFsBackend};
    use runtime::history::{HistoryEvent, HistoryGraph, HistoryLog, HistoryRecord, RecordOutcome};

    let env = TestEnv::new("interaction-older-context");
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
    ];
    let mut host = Host::with_options(&env, true, &flags);
    let pending = ask(&env, &mut host);
    let journal_path = interaction_journals(&env)
        .into_iter()
        .find(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            journal["session_id"] == host.session
        })
        .unwrap();
    let transcript = journal_path
        .parent()
        .unwrap()
        .join(runtime::session_control::TRANSCRIPT_FILE);
    let before_answer = HistoryGraph::decode(&std::fs::read(&transcript).unwrap())
        .unwrap()
        .latest_commit()
        .unwrap()
        .to_owned();
    control(&mut host, answer(&pending));
    wait_for_delivery(&mut host);
    let session = host.session.clone();
    host.close();
    drop(host);

    // Reproduce a commit-before-ACK loss, then resume a deliberately older
    // completed continuation. Its projection does not contain the answer;
    // the authoritative history still proves that another context consumed it.
    let mut journal: Value =
        serde_json::from_slice(&std::fs::read(&journal_path).unwrap()).unwrap();
    let record = journal["records"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|record| record["id"] == pending["id"])
        .unwrap();
    assert_eq!(record["status"], "delivered");
    record["status"] = json!("resolved");
    std::fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    let log = StdFsBackend
        .record_log(&transcript.to_string_lossy(), RecordLogMode::Existing)
        .unwrap();
    let mut history = HistoryLog::new(log);
    let branch = HistoryRecord::branch(before_answer);
    assert_eq!(
        history.commit(&branch, Duration::from_secs(5)).unwrap(),
        RecordOutcome::Applied
    );
    let finish = HistoryRecord::new(
        branch.context_id().into(),
        Some(branch.id().into()),
        HistoryEvent::CommitTurn {
            trace_id: "fixture-older-completed-context".into(),
        },
    );
    assert_eq!(
        history.commit(&finish, Duration::from_secs(5)).unwrap(),
        RecordOutcome::Applied
    );
    assert!(!history
        .graph()
        .completed_session(None)
        .unwrap()
        .messages()
        .iter()
        .flat_map(|message| &message.inputs)
        .any(|input| matches!(
            input.source,
            runtime::turn_input::InputSource::Interaction(_)
        )));
    drop(history);
    let before = env.is_mock().then(|| env.captured_message_bodies().len());
    let mut resumed = resume(&env, &session);
    let state = control(&mut resumed, json!({"action":"configure", "enabled":true}));
    assert!(
        state["requests"].as_array().unwrap().is_empty(),
        "answer from another context was scheduled again: {state}"
    );
    resumed.idle();
    if let Some(before) = before {
        assert_eq!(
            env.captured_message_bodies().len(),
            before,
            "receipt recovery started another model turn"
        );
    }
    resumed.close();
}

fn resume(env: &TestEnv, session: &str) -> Host {
    let mut resumed = Host::with_options(env, true, ENABLED);
    resumed.close();
    resumed.request(
        "session/load",
        &json!({"sessionId":session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    session.clone_into(&mut resumed.session);
    resumed
}

#[test]
fn another_loaded_context_cannot_overwrite_a_committed_answer() {
    let env = TestEnv::new("nonblocking-question-writer-conflict");
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
    ];
    let mut first = Host::with_options(&env, true, &flags);
    let pending = ask(&env, &mut first);
    let mut stale = Host::with_options(&env, true, &flags);
    stale.close();
    stale.request(
        "session/load",
        &json!({"sessionId":first.session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    stale.session.clone_from(&first.session);
    let restored = control(&mut stale, json!({"action":"configure", "enabled":true}));
    assert_eq!(restored["requests"][0], pending);

    control(&mut first, answer(&pending));
    wait_for_delivery(&mut first);
    let mut conflicting = answer(&pending);
    conflicting["reply"]["answers"][0]["value"] = json!("red");
    conflicting["reply"]["answers"][0]["label"] = json!("red");
    conflicting["sessionId"] = json!(stale.session);
    let rejected = stale.response(METHOD, &conflicting);
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert!(
        rejected.to_string().contains("stale or already resolved"),
        "{rejected}"
    );

    let journal = interaction_journals(&env)
        .into_iter()
        .find_map(|path| {
            let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            (value["session_id"] == first.session).then_some(value)
        })
        .unwrap();
    let saved = journal["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pending["id"])
        .unwrap();
    assert_eq!(saved["reply"]["answers"][0]["value"], "blue");
    assert_eq!(saved["status"], "delivered");
    stale.close();
    first.close();
}

#[test]
fn unanswered_question_survives_session_close_and_load() {
    let env = TestEnv::new("nonblocking-question-resume");
    let mut first = Host::with_options(&env, true, ENABLED);
    let pending = ask(&env, &mut first);
    let session = first.session.clone();
    first.close();
    drop(first);

    let mut resumed = resume(&env, &session);
    let state = control(&mut resumed, json!({"action":"configure", "enabled":true}));
    assert_eq!(
        state["requests"][0], pending,
        "resume changed the pending request"
    );
    control(
        &mut resumed,
        json!({
            "action":"reply", "interaction_id":pending["id"], "revision":pending["revision"],
            "reply":{"kind":"cancelled", "reason":"No longer needed"},
        }),
    );
    resumed.idle();
    resumed.close();
}

#[test]
fn nonblocking_interactions_require_explicit_experimental_enablement() {
    let env = TestEnv::new("nonblocking-question-disabled");
    let mut host = Host::with_options(
        &env,
        true,
        &[("SUDOCODE_EXPERIMENT_NONBLOCKING_INTERACTIONS", "0")],
    );
    let response = host.response(
        METHOD,
        &json!({
            "sessionId":host.session, "action":"configure", "enabled":true,
        }),
    );
    assert!(response.get("error").is_some(), "{response}");
    host.close();
}
struct ReleaseSharedInput(std::path::PathBuf);

impl Drop for ReleaseSharedInput {
    fn drop(&mut self) {
        let _ = std::fs::write(self.0.join("release-shared-input"), "cleanup");
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn shared_answer_consumption(cancel_before_consumption: bool) {
    use runtime::history::{HistoryGraph, HistoryRecord, RecordOutcome};

    let env = TestEnv::new(if cancel_before_consumption {
        "interaction-queued-context"
    } else {
        "interaction-steering-context"
    });
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
    ];
    let mut first = Host::with_options(&env, true, &flags);
    let pending = ask(&env, &mut first);
    let mut other = Host::with_options(&env, true, &flags);
    other.close();
    other.request(
        "session/load",
        &json!({"sessionId":first.session,"cwd":env.workspace_root(),"mcpServers":[]}),
    );
    other.session.clone_from(&first.session);
    control(&mut other, json!({"action":"configure","enabled":true}));

    // Keep the second context between tool dispatch and the next input commit.
    // The real shell supplies a readiness barrier; timing does not decide who
    // wins the answer. Cancellation also exercises the next initial batch.
    std::fs::write(env.workspace_root().join("shared-input.sh"),
        "printf started > shared-input-started\ni=0\nwhile [ ! -f release-shared-input ] && [ $i -lt 1200 ]; do\n sleep 0.1\n i=$((i+1))\ndone\nprintf SharedInputToolFinished\n").unwrap();
    let _release = ReleaseSharedInput(env.workspace_root().to_path_buf());
    let input = json!({"command":"sh shared-input.sh","timeout":180_000,"run_in_background":false});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"shared-input-shell","name":"bash","input":input}])
        )
    } else {
        format!("Call bash exactly once with this literal input: {input}. The test harness releases the script. Do not change it or create the release file. Later input is information only. Report the actual result after it finishes, without further tools.")
    };
    other.submit("shared-shell", &prompt, "steer");
    common::expect_screen(
        &other.terminal,
        |_| env.workspace_root().join("shared-input-started").exists(),
        common::LIVE_TURN_BUDGET,
        "shared context shell started",
    );
    control(&mut other, answer(&pending));
    let queued = other.snapshot();
    assert!(
        queued["pending"].as_array().unwrap().iter().any(|item| {
            item["clientId"]
                .as_str()
                .is_some_and(|id| id.starts_with("interaction:"))
        }),
        "answer was not queued behind the running tool: {queued}"
    );
    other.submit(
        "human-after-answer",
        "RetainedHumanContextMarker: acknowledge this message briefly without tools.",
        "queue",
    );
    if cancel_before_consumption {
        let newline = if cfg!(windows) { "\r" } else { "\n" };
        other.terminal.send(&format!("{}{newline}", json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":other.session}}))).unwrap();
        let paused = other.idle();
        assert_eq!(paused["paused"], true, "{paused}");
        assert_eq!(paused["pending"].as_array().unwrap().len(), 2, "{paused}");
    }

    // An identical answer is idempotent across controllers. Only this context
    // can consume it while the other remains behind its real execution barrier.
    control(&mut first, answer(&pending));
    wait_for_delivery(&mut first);
    std::fs::write(env.workspace_root().join("release-shared-input"), "release").unwrap();
    if cancel_before_consumption {
        other.input(json!({"action":"flush"}));
    }
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        let state = other.snapshot();
        if state["running"] == false && state["pending"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "duplicate answer stranded the input queue: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(snapshot(&mut other)["requests"], json!([]));
    let session = first.session.clone();
    other.close();
    first.close();
    drop(other);
    drop(first);
    let transcript = interaction_journals(&env)
        .into_iter()
        .find_map(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            (journal["session_id"] == session).then(|| {
                path.parent()
                    .unwrap()
                    .join(runtime::session_control::TRANSCRIPT_FILE)
            })
        })
        .unwrap();
    let bytes = std::fs::read(&transcript).unwrap();
    let graph = HistoryGraph::decode(&bytes).unwrap();
    let mut committed_answers = 0;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: HistoryRecord = serde_json::from_slice(line).unwrap();
        if graph.outcome(&record).unwrap() != Some(RecordOutcome::Applied) {
            continue;
        }
        let value: Value = serde_json::from_slice(line).unwrap();
        if let Some(inputs) = value["event"]["message"]["inputs"].as_array() {
            committed_answers += inputs
                .iter()
                .filter(|input| {
                    input["source"] == "interaction" && input["interaction"]["id"] == pending["id"]
                })
                .count();
        }
    }
    assert_eq!(
        committed_answers, 1,
        "the answer must enter canonical history once"
    );
    let latest = graph.completed_session(None).unwrap();
    assert!(
        latest
            .messages()
            .iter()
            .flat_map(|message| &message.inputs)
            .any(|input| {
                input.source == runtime::turn_input::InputSource::Human
                    && input.display.contains("RetainedHumanContextMarker")
            }),
        "the later human message must survive reconciliation"
    );
    if env.is_mock() {
        assert!(env
            .captured_message_bodies()
            .iter()
            .any(|body| body.contains("RetainedHumanContextMarker")));
    }
}

#[test]
fn concurrent_context_retires_consumed_steering_and_preserves_human_queue() {
    shared_answer_consumption(false);
}

#[test]
fn concurrent_context_retires_consumed_initial_answer_after_cancellation() {
    shared_answer_consumption(true);
}
#[test]
fn independently_loaded_context_checks_the_current_plan_before_writing() {
    let env = TestEnv::new("interaction-current-plan");
    let plan_path = env.workspace_root().join("shared-review.md");
    let log = env.workspace_root().join("shared-plan-requests.jsonl");
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
        ("SUDOCODE_PLAN_FILE", plan_path.to_str().unwrap()),
        ("SCODE_LOG_PATH", log.to_str().unwrap()),
    ];
    std::fs::write(
        env.workspace_root().join("shared-plan-reference.txt"),
        "SharedPlanReadMarker",
    )
    .unwrap();
    let mut first = Host::with_options(&env, true, &flags);
    control(&mut first, json!({"action":"configure","enabled":true}));
    let mut other = Host::with_options(&env, true, &flags);
    other.close();
    other.request(
        "session/load",
        &json!({"sessionId":first.session,"cwd":env.workspace_root(),"mcpServers":[]}),
    );
    other.session.clone_from(&first.session);
    assert_eq!(
        control(&mut other, json!({"action":"configure","enabled":true}))["requests"],
        json!([])
    );
    let plan_input = json!({"context":"Shared approval authority.","constraints":"Require approval before writes.",
        "acceptance":"shared-plan-effect.txt contains SharedPlanEffectMarker.",
        "content":"Create shared-plan-effect.txt containing SharedPlanEffectMarker after approval."});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"shared-plan","name":"write_plan","input":plan_input}])
        )
    } else {
        format!("Call write_plan exactly once with this input: {plan_input}. End your turn with the review unanswered. Do not implement or use other tools. Acknowledge later interaction_reply input briefly without tools.")
    };
    first.submit("publish-shared-plan", &prompt, "steer");
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    let pending = loop {
        let state = snapshot(&mut first);
        if let Some(request) = state["requests"].as_array().unwrap().first() {
            break request.clone();
        }
        assert!(Instant::now() < deadline, "plan was not published: {state}");
        std::thread::sleep(Duration::from_millis(30));
    };
    first.idle();
    let write = json!({"path":"shared-plan-effect.txt","content":"SharedPlanEffectMarker"});
    let attempt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([
                {"id":"other-premature-write","name":"write_file","input":write},
                {"id":"other-independent-read","name":"read_file","input":{"path":"shared-plan-reference.txt"}}
            ])
        )
    } else {
        format!("Call write_file exactly once with {write}. Whether it succeeds or is denied, read shared-plan-reference.txt and report both actual outcomes. Do not retry, revise the plan, or use another write mechanism.")
    };
    other.submit("other-context-write", &attempt, "steer");
    other.idle();
    assert!(
        !env.workspace_root().join("shared-plan-effect.txt").exists(),
        "an older context bypassed the newer plan"
    );
    assert!(captured_tool_result_contains(&log, "awaiting approval"));
    assert!(
        captured_tool_result_contains(&log, "SharedPlanReadMarker"),
        "read-only work should continue"
    );

    control(
        &mut first,
        json!({"action":"reply","interaction_id":pending["id"],"revision":pending["revision"],
        "reply":{"kind":"answers","answers":[{"id":"action","value":"2","label":"Approve, keep context"}]}}),
    );
    wait_for_delivery(&mut first);
    let implement = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"other-approved-write","name":"write_file","input":write}])
        )
    } else {
        format!("The exact plan has now been approved. Call write_file once with {write}, then report its actual result without other tools.")
    };
    other.submit("other-context-approved-write", &implement, "steer");
    other.idle();
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("shared-plan-effect.txt")).unwrap(),
        "SharedPlanEffectMarker"
    );
    other.close();
    first.close();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn a_live_permission_claim_cannot_be_retired_as_an_interruption_by_another_context() {
    let env = TestEnv::new("interaction-live-permission-owner");
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
    ];
    std::fs::write(
        env.workspace_root().join("shared-plan-reference.txt"),
        "IndependentPermissionOwnerRead",
    )
    .unwrap();
    std::fs::write(env.workspace_root().join("shared-input.sh"),
        "printf ClaimedOnce >> shared-permission-count\nprintf started > shared-input-started\ni=0\nwhile [ ! -f release-shared-input ] && [ $i -lt 1200 ]; do\n sleep 0.1\n i=$((i+1))\ndone\nprintf ExactApprovedOperationResult\n").unwrap();
    let mut first = Host::with_options(&env, false, &flags);
    control(&mut first, json!({"action":"configure","enabled":true}));
    let input = json!({"command":"sh shared-input.sh","timeout":180_000,"run_in_background":false});
    let prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"shared-permission-shell","name":"bash","input":input}])
        )
    } else {
        format!("Call bash exactly once with this literal input: {input}. If permission is pending, end your turn. The test harness will approve and release the script; never change it, create the release file, or retry the tool. When the exact operation resumes, report its actual result without further tools.")
    };
    first.submit("pending-permission", &prompt, "steer");
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    let pending = loop {
        let state = snapshot(&mut first);
        if let Some(request) = state["requests"].as_array().unwrap().first() {
            break request.clone();
        }
        assert!(
            Instant::now() < deadline,
            "permission was not published: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    };
    first.idle();
    assert!(!env
        .workspace_root()
        .join("shared-permission-count")
        .exists());
    let mut other = Host::with_options(&env, false, &flags);
    other.close();
    other.request(
        "session/load",
        &json!({"sessionId":first.session,"cwd":env.workspace_root(),"mcpServers":[]}),
    );
    other.session.clone_from(&first.session);
    control(&mut other, json!({"action":"configure","enabled":true}));
    let decision = json!({"action":"reply","interaction_id":pending["id"],"revision":pending["revision"],
        "reply":{"kind":"permission","approved":true,"reason":"Approve the exact operation once."}});
    // Drop the release guard before either terminal host if an assertion fails.
    let _release = ReleaseSharedInput(env.workspace_root().to_path_buf());
    control(&mut first, decision.clone());
    common::expect_screen(
        &first.terminal,
        |_| env.workspace_root().join("shared-input-started").exists(),
        common::LIVE_TURN_BUDGET,
        "approved operation started before the competing delivery",
    );
    control(&mut other, decision);
    // A waiting foreign decision must not occupy the main execution lane.
    let read = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"foreign-owner-read","name":"read_file","input":{"path":"shared-plan-reference.txt"}}])
        )
    } else {
        "Independently read shared-plan-reference.txt and report its exact content. Do not execute or retry the approved shell, change its release file, or classify its outcome yet.".into()
    };
    other.submit("independent-owner-read", &read, "steer");
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        let state = other.snapshot();
        if state["running"] == false && state["pending"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a foreign claim blocked ordinary chat: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    let waiting = snapshot(&mut other);
    let saved = waiting["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == pending["id"])
        .expect("another context consumed a live claim as an interruption");
    assert_eq!(saved["status"], "resolved");
    assert_eq!(saved["execution"]["state"], "claimed");
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("shared-permission-count")).unwrap(),
        "ClaimedOnce"
    );
    std::fs::write(env.workspace_root().join("release-shared-input"), "release").unwrap();
    wait_for_delivery(&mut first);
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("shared-permission-count")).unwrap(),
        "ClaimedOnce"
    );
    let journal = interaction_journals(&env)
        .into_iter()
        .find_map(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            (journal["session_id"] == first.session).then_some(journal)
        })
        .unwrap();
    let saved = journal["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == pending["id"])
        .unwrap();
    assert_eq!(saved["status"], "delivered");
    assert_eq!(saved["execution"]["state"], "finished");
    if env.is_mock() {
        assert!(env
            .captured_message_bodies()
            .iter()
            .any(|body| body.contains("IndependentPermissionOwnerRead")));
        assert!(env
            .captured_message_bodies()
            .iter()
            .any(|body| body.contains("ExactApprovedOperationResult")));
    }
    other.close();
    first.close();
}

fn publish_plan_review(env: &TestEnv, host: &mut Host, id: &str, content: &str) -> Value {
    let input = json!({"context":"Approval lifecycle acceptance.",
        "constraints":"Require explicit approval before implementation.",
        "acceptance":"The approved effect is written only after the current review is approved.",
        "content":content});
    let prompt = tool_batch(env, &json!([{"id":id,"name":"write_plan","input":input}]),
        &format!("Call write_plan exactly once with this literal input: {input}. Do no other work this turn. Leave the review pending. Later interaction replies should only be acknowledged until another explicit instruction."));
    host.submit(id, &prompt, "steer");
    host.idle();
    finish_mock_question(env);
    let state = snapshot(host);
    let requests = state["requests"].as_array().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "only the current plan should need review: {state}"
    );
    requests[0].clone()
}

fn plan_reply(pending: &Value, value: &str) -> Value {
    json!({"action":"reply","interaction_id":pending["id"],"revision":pending["revision"],
        "reply":{"kind":"answers","answers":[{"id":"action","value":value,"label":null}]}})
}

fn dismissed_plan_keeps_write_gate(cancel: bool) {
    let env = TestEnv::new(if cancel {
        "plan-cancelled-gate"
    } else {
        "plan-exit-gate"
    });
    let plan = env.workspace_root().join("dismissed-plan.md");
    let log = env.workspace_root().join("dismissed-plan-requests.jsonl");
    std::fs::write(
        env.workspace_root().join("dismissed-plan-reference.txt"),
        "ReadAfterDismissalMarker",
    )
    .unwrap();
    let flags = [
        ENABLED[0],
        ("SUDOCODE_PLAN_FILE", plan.to_str().unwrap()),
        ("SCODE_LOG_PATH", log.to_str().unwrap()),
    ];
    let mut host = Host::with_options(&env, true, &flags);
    control(&mut host, json!({"action":"configure","enabled":true}));
    let pending = publish_plan_review(
        &env,
        &mut host,
        "dismissed-plan",
        "Create dismissed-effect.txt after approval.",
    );
    let decision = if cancel {
        json!({"action":"reply","interaction_id":pending["id"],"revision":pending["revision"],
            "reply":{"kind":"cancelled","reason":"Do not implement this plan."}})
    } else {
        plan_reply(&pending, "4")
    };
    control(&mut host, decision);
    wait_for_delivery(&mut host);
    let prompt = tool_batch(&env, &json!([
        {"id":"dismissed-write","name":"write_file","input":{"path":"dismissed-effect.txt","content":"MustNotBeWritten"}},
        {"id":"dismissed-read","name":"read_file","input":{"path":"dismissed-plan-reference.txt"}}
    ]), "Test the current engine policy: attempt write_file once for dismissed-effect.txt with MustNotBeWritten, then read dismissed-plan-reference.txt. Report both actual results. Do not revise the plan, retry a refused write, or use another tool.");
    host.submit("work-after-dismissal", &prompt, "steer");
    host.idle();
    assert!(
        !env.workspace_root().join("dismissed-effect.txt").exists(),
        "dismissing a review must not authorize its implementation"
    );
    assert!(captured_tool_result_contains(&log, "awaiting approval"));
    assert!(captured_tool_result_contains(
        &log,
        "ReadAfterDismissalMarker"
    ));
    host.close();
}

#[test]
fn cancelling_plan_review_does_not_authorize_implementation() {
    dismissed_plan_keeps_write_gate(true);
}

#[test]
fn exiting_plan_review_does_not_authorize_implementation() {
    dismissed_plan_keeps_write_gate(false);
}

#[test]
fn revised_plan_retires_queued_approval_without_clearing_context_or_losing_chat() {
    superseded_plan_preserves_later_chat(false);
}

#[test]
fn pruned_plan_approval_returns_delivery_without_clearing_context_or_losing_chat() {
    superseded_plan_preserves_later_chat(true);
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn superseded_plan_preserves_later_chat(prune: bool) {
    let env = TestEnv::new(if prune {
        "plan-pruned-queue"
    } else {
        "plan-superseded-queue"
    });
    let plan = env.workspace_root().join("superseded-plan.md");
    let log = env.workspace_root().join("superseded-plan-requests.jsonl");
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
        ("SUDOCODE_PLAN_FILE", plan.to_str().unwrap()),
        ("SCODE_LOG_PATH", log.to_str().unwrap()),
    ];
    let mut first = Host::with_options(&env, true, &flags);
    control(&mut first, json!({"action":"configure","enabled":true}));
    let mut other = Host::with_options(&env, true, &flags);
    other.close();
    other.request(
        "session/load",
        &json!({"sessionId":first.session,"cwd":env.workspace_root(),"mcpServers":[]}),
    );
    other.session.clone_from(&first.session);
    control(&mut other, json!({"action":"configure","enabled":true}));
    // Start the real execution barrier before publishing a plan gates writes.
    std::fs::write(env.workspace_root().join("shared-input.sh"),
        "printf started > shared-input-started\ni=0\nwhile [ ! -f release-shared-input ] && [ $i -lt 1200 ]; do\n sleep 0.1\n i=$((i+1))\ndone\nprintf OldPlanContextMustSurvive\n").unwrap();
    let _release = ReleaseSharedInput(env.workspace_root().to_path_buf());
    let input = json!({"command":"sh shared-input.sh","timeout":180_000,"run_in_background":false});
    let shell_prompt = if env.is_mock() {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!([{"id":"plan-barrier","name":"bash","input":input}])
        )
    } else {
        format!("Call bash exactly once with {input}. The harness releases the script. Do not modify it, create its release file or call other tools. Report the actual result when complete; subsequent chat requires no tools.")
    };
    other.submit("plan-barrier", &shell_prompt, "steer");
    common::expect_screen(
        &other.terminal,
        |_| env.workspace_root().join("shared-input-started").exists(),
        common::LIVE_TURN_BUDGET,
        "plan review execution barrier",
    );
    let old = publish_plan_review(
        &env,
        &mut first,
        "old-plan",
        "Create superseded-effect.txt with OldPlanEffect after approval.",
    );
    let old_reply = plan_reply(&old, "1");
    control(&mut other, old_reply.clone());
    assert!(other.snapshot()["pending"]
        .as_array()
        .unwrap()
        .iter()
        .any(|input| input["clientId"]
            .as_str()
            .is_some_and(|id| id.starts_with("interaction:"))));
    other.submit(
        "human-after-old-plan",
        "RetainedAfterPlanRevision: acknowledge briefly without tools.",
        "queue",
    );
    let mut current = publish_plan_review(
        &env,
        &mut first,
        "revised-plan",
        "Create superseded-effect.txt with CurrentPlanEffect after approval.",
    );
    if prune {
        first.close();
        let path = interaction_journals(&env)
            .into_iter()
            .find(|path| {
                let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                journal["session_id"] == first.session
            })
            .unwrap();
        let mut journal: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let records = journal["records"].as_array_mut().unwrap();
        let closed = records
            .iter()
            .find(|item| item["id"] == old["id"])
            .unwrap()
            .clone();
        assert_eq!(closed["status"], "superseded");
        // Seed only prior closed history in this isolated restart fixture.
        // The next real write_plan call must perform the actual bounded prune;
        // the still-running sibling retains the original queued approval.
        for index in 0..127 {
            let mut prior = closed.clone();
            prior["id"] = json!(format!("00000000-0000-4000-8000-{index:012x}"));
            prior["source"] = json!(format!("closed-plan-fixture-{index}"));
            records.insert(records.len() - 1, prior);
        }
        journal["revision"] = json!(journal["revision"].as_u64().unwrap() + 1);
        let temporary = path.with_extension("prune-fixture.tmp");
        std::fs::write(&temporary, serde_json::to_vec(&journal).unwrap()).unwrap();
        std::fs::rename(&temporary, &path).unwrap();
        first.request(
            "session/load",
            &json!({"sessionId":first.session,"cwd":env.workspace_root(),"mcpServers":[]}),
        );
        control(&mut first, json!({"action":"configure","enabled":true}));
        current = publish_plan_review(
            &env,
            &mut first,
            "pruning-trigger-plan",
            "Create superseded-effect.txt with CurrentPlanEffect after approval.",
        );
        let pruned: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            !pruned["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["id"] == old["id"]),
            "the real plan publication must prune the oldest closed receipt"
        );
        assert_eq!(pruned["records"].as_array().unwrap().len(), 129);
    }
    assert_ne!(current["id"], old["id"]);
    let mut retired_reply = old_reply;
    retired_reply["sessionId"] = json!(first.session);
    assert!(
        first
            .response(METHOD, &retired_reply)
            .get("error")
            .is_some(),
        "the old approval cannot be reused"
    );
    std::fs::write(env.workspace_root().join("release-shared-input"), "release").unwrap();
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
    loop {
        let state = other.snapshot();
        if state["running"] == false && state["pending"].as_array().unwrap().is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "stale plan stranded ordinary input: {state}"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!env.workspace_root().join("superseded-effect.txt").exists());
    let requests = request_evidence::accepted_messages(&log);
    let last = requests.last().unwrap()["messages"].to_string();
    assert!(
        last.contains("RetainedAfterPlanRevision"),
        "later chat was lost: {last}"
    );
    assert!(
        last.contains("OldPlanContextMustSurvive"),
        "obsolete approval cleared context: {last}"
    );
    assert!(
        !last.contains("interaction_reply"),
        "obsolete approval reached inference: {last}"
    );
    assert_eq!(snapshot(&mut first)["requests"][0]["id"], current["id"]);
    let session = first.session.clone();
    other.close();
    first.close();
    let journal = interaction_journals(&env)
        .into_iter()
        .map(|path| serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap())
        .find(|journal| journal["session_id"] == session)
        .unwrap();
    let old_record = journal["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == old["id"]);
    if prune {
        assert!(
            old_record.is_none(),
            "returning delivery must not recreate a receipt or acknowledge a missing answer"
        );
    } else {
        let old_record = old_record.unwrap();
        assert_eq!(old_record["status"], "superseded");
        assert_eq!(
            old_record["reply"]["answers"][0]["value"], "1",
            "preserve the original answer for audit"
        );
    }
}

#[derive(Clone, Copy)]
enum ApprovalFreshnessCase {
    RestrictivePolicy,
    DenyingHook,
    ChangedInput,
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn deferred_approval_uses_current_policy_and_hooks(case: ApprovalFreshnessCase) {
    let env = TestEnv::new("deferred-approval-freshness");
    let log = env
        .workspace_root()
        .join("approval-freshness-requests.jsonl");
    let hook_marker = env.workspace_root().join("change-approval-hook");
    if !matches!(case, ApprovalFreshnessCase::RestrictivePolicy) {
        let response = if matches!(case, ApprovalFreshnessCase::DenyingHook) {
            // Hook feedback is carried on stdout by the existing hook contract.
            "printf 'FreshHookDeniedApproval\\n'\nexit 2\n".to_owned()
        } else {
            format!(
                "printf '%s\\n' '{}'\n",
                json!({"hookSpecificOutput":{"updatedInput":{
                    "command":"printf mutated >> mutated-permission-count.txt"
                }}})
            )
        };
        std::fs::write(
            env.workspace_root().join("approval-freshness-hook.sh"),
            format!("if [ -f change-approval-hook ]; then\n  printf 'observed\\n' >> approval-hook-observed.txt\n{response}fi\n"),
        )
        .unwrap();
        let settings = env.workspace_root().join(".nexus/sudocode/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            settings,
            json!({"hooks":{"PreToolUse":["sh approval-freshness-hook.sh"]}}).to_string(),
        )
        .unwrap();
    }
    let mut host = Host::with_options(
        &env,
        false,
        &[ENABLED[0], ("SCODE_LOG_PATH", log.to_str().unwrap())],
    );
    let pending = request_permission(&env, &mut host);
    let expected = match case {
        ApprovalFreshnessCase::RestrictivePolicy => {
            host.request(
                "session/setPermissionMode",
                &json!({"sessionId":host.session,"permissionMode":"read-only"}),
            );
            "current mode is read-only"
        }
        ApprovalFreshnessCase::DenyingHook => {
            std::fs::write(&hook_marker, "deny on the next invocation").unwrap();
            "FreshHookDeniedApproval"
        }
        ApprovalFreshnessCase::ChangedInput => {
            // A broader policy must not turn the old captured call into a
            // different operation. Without the exact-input fence, this hook's
            // replacement would now pass ordinary permission evaluation.
            host.request(
                "session/setPermissionMode",
                &json!({"sessionId":host.session,"permissionMode":"danger-full-access"}),
            );
            std::fs::write(&hook_marker, "replace the captured tool input").unwrap();
            "Tool input changed after approval"
        }
    };
    let reply = json!({
        "action":"reply","interaction_id":pending["id"],"revision":pending["revision"],
        "reply":{"kind":"permission","approved":true,"reason":null},
    });
    control(&mut host, reply.clone());
    wait_for_delivery(&mut host);
    assert!(!env.workspace_root().join("permission-count.txt").exists());
    assert!(!env
        .workspace_root()
        .join("mutated-permission-count.txt")
        .exists());
    let requests = request_evidence::accepted_messages(&log);
    let last = requests.last().unwrap()["messages"].to_string();
    assert!(
        last.contains("approved_operation_result"),
        "missing saved result: {last}"
    );
    assert!(
        last.contains(expected),
        "the current policy/hook result was not delivered: {last}"
    );
    if !matches!(case, ApprovalFreshnessCase::RestrictivePolicy) {
        assert_eq!(
            std::fs::read_to_string(env.workspace_root().join("approval-hook-observed.txt"))
                .unwrap(),
            "observed\n",
            "the deferred call must execute the fresh pre-tool hook once"
        );
    }
    let session = host.session.clone();
    let journal = interaction_journals(&env)
        .into_iter()
        .map(|path| serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap())
        .find(|journal| journal["session_id"] == session)
        .unwrap();
    let record = journal["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["id"] == pending["id"])
        .unwrap();
    assert_eq!(record["status"], "delivered");
    assert_eq!(record["execution"]["state"], "finished");

    // Restoring broad permission cannot revive the already-delivered approval.
    host.request(
        "session/setPermissionMode",
        &json!({"sessionId":host.session,"permissionMode":"danger-full-access"}),
    );
    control(&mut host, reply);
    host.idle();
    assert!(!env.workspace_root().join("permission-count.txt").exists());
    assert!(!env
        .workspace_root()
        .join("mutated-permission-count.txt")
        .exists());
    if !matches!(case, ApprovalFreshnessCase::RestrictivePolicy) {
        assert_eq!(
            std::fs::read_to_string(env.workspace_root().join("approval-hook-observed.txt"))
                .unwrap(),
            "observed\n",
            "an identical old reply must not invoke the pre-tool hook again"
        );
    }
    host.close();
}

#[test]
fn deferred_permission_respects_a_new_restrictive_policy() {
    deferred_approval_uses_current_policy_and_hooks(ApprovalFreshnessCase::RestrictivePolicy);
}

#[test]
fn deferred_permission_runs_the_current_denying_hook() {
    deferred_approval_uses_current_policy_and_hooks(ApprovalFreshnessCase::DenyingHook);
}

#[test]
fn deferred_permission_rejects_changed_hook_input_even_under_broader_policy() {
    deferred_approval_uses_current_policy_and_hooks(ApprovalFreshnessCase::ChangedInput);
}

#[allow(
    clippy::too_many_lines,
    reason = "Keep the connected request, execution and recovery journey together."
)]
fn failed_interaction_setup_preserves_plan_gate(wrong_workspace: bool) {
    let env = TestEnv::new_mock(if wrong_workspace {
        "interaction-binding-wrong-workspace"
    } else {
        "interaction-binding-malformed"
    });
    let plan = env.workspace_root().join("binding-recovery-plan.md");
    let flags = [ENABLED[0], ("SUDOCODE_PLAN_FILE", plan.to_str().unwrap())];
    let mut host = Host::with_options(&env, true, &flags);
    control(&mut host, json!({"action":"configure","enabled":true}));
    let pending = publish_plan_review(
        &env,
        &mut host,
        "binding-plan",
        "Create binding-effect.txt with BindingEffectMarker only after approval.",
    );
    let session = host.session.clone();
    host.close();
    drop(host);
    let journal_path = interaction_journals(&env)
        .into_iter()
        .find(|path| {
            let journal: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            journal["session_id"] == session
        })
        .expect("saved plan journal");
    let original = std::fs::read(&journal_path).unwrap();
    let invalid = if wrong_workspace {
        let mut journal: Value = serde_json::from_slice(&original).unwrap();
        journal["workspace_root"] = json!(env.workspace_root().join("another-workspace"));
        serde_json::to_vec(&journal).unwrap()
    } else {
        b"malformed journal fixture".to_vec()
    };
    std::fs::write(&journal_path, &invalid).unwrap();
    let mut host = Host::with_options(&env, true, &flags);
    host.close();
    host.request(
        "session/load",
        &json!({"sessionId":session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    host.session.clone_from(&session);
    let configure = json!({"sessionId":session,"action":"configure","enabled":true});
    let rejected = host.response(METHOD, &configure);
    assert!(rejected.get("error").is_some(), "{rejected}");
    if wrong_workspace {
        assert!(
            rejected.to_string().contains("workspace changed"),
            "{rejected}"
        );
    }

    let write_calls = json!([
        {"id":"binding-write","name":"write_file","input":{
            "path":"binding-effect.txt","content":"BindingEffectMarker"}}
    ]);
    let write_prompt = tool_batch(&env, &write_calls, "Attempt exactly one write_file call to create binding-effect.txt with BindingEffectMarker, then stop.");
    let before = env.captured_message_bodies().len();
    host.submit("work-with-unavailable-journal", &write_prompt, "steer");
    host.idle();
    assert!(
        !env.workspace_root().join("binding-effect.txt").exists(),
        "failed configuration must not silently remove the plan gate"
    );
    assert_eq!(
        env.captured_message_bodies().len(),
        before,
        "journal admission failure must precede model execution"
    );
    // Controls remain responsive. Disabling cannot erase an unreadable gate;
    // neither inspection nor a failed turn may replace the saved decisions.
    let disabled = host.response(
        METHOD,
        &json!({"sessionId":session,"action":"configure","enabled":false}),
    );
    assert!(disabled.get("error").is_some(), "{disabled}");
    assert!(host
        .response(METHOD, &json!({"sessionId":session,"action":"snapshot"}))
        .get("error")
        .is_some());
    assert_eq!(host.runtime_status()["foreground"], json!({"state":"idle"}));
    assert_eq!(std::fs::read(&journal_path).unwrap(), invalid);

    // Repair only this disposable fixture, then retry on the same engine.
    // It must recover the original identity and retain the pending write gate.
    std::fs::write(&journal_path, original).unwrap();
    let restored = host.request(METHOD, &configure);
    assert_eq!(restored["requests"][0], pending);
    let disabled = host.response(
        METHOD,
        &json!({"sessionId":session,"action":"configure","enabled":false}),
    );
    assert!(
        disabled
            .to_string()
            .contains("resolve outstanding interactions"),
        "{disabled}"
    );
    std::fs::write(
        env.workspace_root().join("binding-reference.txt"),
        "IndependentBindingRecoveryMarker",
    )
    .unwrap();
    let prompt = tool_batch(&env, &json!([
        {"id":"binding-recovered-write","name":"write_file","input":{
            "path":"binding-effect.txt","content":"BindingEffectMarker"}},
        {"id":"binding-recovered-read","name":"read_file","input":{"path":"binding-reference.txt"}}
    ]), "Attempt the file write once, then independently read binding-reference.txt. Report the actual results and stop.");
    host.submit("work-after-journal-recovery", &prompt, "steer");
    host.idle();
    finish_mock_question(&env);
    assert!(!env.workspace_root().join("binding-effect.txt").exists());
    let requests = env.captured_message_bodies();
    let last = requests.last().unwrap();
    assert!(last.contains("awaiting approval"), "{last}");
    assert!(last.contains("IndependentBindingRecoveryMarker"), "{last}");
    assert_eq!(snapshot(&mut host)["requests"][0], pending);

    control(&mut host, plan_reply(&pending, "2"));
    wait_for_delivery(&mut host);
    let write_prompt = tool_batch(&env, &write_calls, "Now create binding-effect.txt with BindingEffectMarker under the approved plan, then stop.");
    host.submit("work-after-binding-approval", &write_prompt, "steer");
    host.idle();
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("binding-effect.txt")).unwrap(),
        "BindingEffectMarker"
    );
    host.close();
}

#[test]
fn malformed_interaction_journal_blocks_work_and_recovers_without_losing_approval() {
    failed_interaction_setup_preserves_plan_gate(false);
}

#[test]
fn mismatched_interaction_workspace_blocks_work_and_recovers_without_losing_approval() {
    failed_interaction_setup_preserves_plan_gate(true);
}

#[test]
fn disabling_interactions_checks_requests_from_another_loaded_context() {
    let env = TestEnv::new_mock("interaction-disable-shared-context");
    let flags = [
        ENABLED[0],
        ("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1"),
    ];
    let mut first = Host::with_options(&env, true, &flags);
    control(&mut first, json!({"action":"configure", "enabled":true}));
    let mut stale = Host::with_options(&env, true, &flags);
    stale.close();
    stale.request(
        "session/load",
        &json!({"sessionId":first.session, "cwd":env.workspace_root(), "mcpServers":[]}),
    );
    stale.session.clone_from(&first.session);
    let initial = control(&mut stale, json!({"action":"configure", "enabled":true}));
    assert_eq!(initial["requests"], json!([]));

    let pending = ask(&env, &mut first);
    let rejected = stale.response(
        METHOD,
        &json!({"sessionId":stale.session, "action":"configure", "enabled":false}),
    );
    assert!(rejected.get("error").is_some(), "{rejected}");
    assert!(
        rejected
            .to_string()
            .contains("resolve outstanding interactions"),
        "{rejected}"
    );
    assert_eq!(snapshot(&mut stale)["requests"][0]["id"], pending["id"]);
    control(&mut first, answer(&pending));
    wait_for_delivery(&mut first);
    let before = env.captured_message_bodies().len();
    let disabled = control(&mut stale, json!({"action":"configure", "enabled":false}));
    assert_eq!(disabled["enabled"], false);
    assert_eq!(
        env.captured_message_bodies().len(),
        before,
        "configuration started inference"
    );
    stale.close();
    first.close();
}
