//! Step budgets stop execution through the real CLI and shared provider builder.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

use common::TestEnv;
use serde_json::json;
use std::{fs, time::Duration};

fn set_budget(env: &TestEnv, limit: u32) {
    let dir = env.workspace_root().join(".nexus/sudocode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.json"),
        json!({"maxSteps":limit}).to_string(),
    )
    .unwrap();
}

#[test]
fn step_budget_finishes_with_existing_results() {
    for limit in [1, 5, 10] {
        let env = TestEnv::new_mock(&format!("step-budget-{limit}"));
        set_budget(&env, limit);
        let prompt = env.prompt("Repeatedly run echo step >> steps.txt. When the step budget is reached, summarize results and suggest next steps.", "step_budget");
        let mut sess = env.spawn(&[
            "-p",
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
            &prompt,
        ]);
        sess.set_default_timeout(Duration::from_secs(90));
        sess.expect("Step budget reached").unwrap();
        assert_eq!(sess.expect_eof().unwrap(), 0);
        assert_eq!(
            fs::read_to_string(env.workspace_root().join("steps.txt"))
                .unwrap()
                .lines()
                .count(),
            limit as usize
        );
        let requests: Vec<serde_json::Value> = env
            .captured_message_bodies()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect();
        assert_eq!(requests.len(), limit as usize + 1);
        let first = &requests[0];
        let final_request = requests.last().unwrap();
        assert!(final_request
            .get("tools")
            .is_none_or(serde_json::Value::is_null));
        assert!(final_request
            .get("tool_choice")
            .is_none_or(serde_json::Value::is_null));
        let system_text = |request: &serde_json::Value| {
            request["system"]
                .as_array()
                .unwrap()
                .iter()
                .map(|block| block["text"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        // Removing tools changes the provider's cache scope, not the prompt text.
        assert_eq!(system_text(first), system_text(final_request));
        for field in ["metadata", "thinking", "reasoning_effort"] {
            assert_eq!(
                first.get(field),
                final_request.get(field),
                "session field {field} drifted"
            );
        }
    }
}

#[test]
fn answer_before_budget_does_not_force_extra_request() {
    let env = TestEnv::new_mock("step-budget-early-answer");
    set_budget(&env, 5);
    let prompt = env.prompt("What is 2 + 2?", "single_turn_text");
    let mut sess = env.spawn(&["-p", &prompt]);
    sess.set_default_timeout(Duration::from_mins(1));
    sess.expect("The answer is 4").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    assert_eq!(env.captured_message_count(), 1);
}

#[test]
fn next_turn_gets_a_fresh_step_budget() {
    let env = TestEnv::new_mock("step-budget-two-turns");
    set_budget(&env, 1);
    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
    ]);
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "initial prompt");
    for turn in 1..=2 {
        let marker = common::turn_status_marker(&sess);
        let prompt = env.prompt(&format!("Turn {turn}: repeatedly append step to steps.txt using bash until the budget is reached."), "step_budget");
        sess.send(&prompt).unwrap();
        common::expect_screen_settled(
            &sess,
            |s| common::input_line_of(s).contains(&format!("Turn {turn}:")),
            common::DEFAULT_TIMEOUT,
            "turn pasted",
        );
        sess.send("\r").unwrap();
        common::expect_turn_complete_after(&sess, &marker, env.timeout(), "bounded turn");
        assert_eq!(
            fs::read_to_string(env.workspace_root().join("steps.txt"))
                .unwrap()
                .lines()
                .count(),
            turn
        );
        assert_eq!(env.captured_message_count(), turn * 2);
    }
    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

#[test]
fn multiple_tools_in_one_response_count_as_one_step() {
    let env = TestEnv::new_mock("step-budget-batch");
    set_budget(&env, 1);
    let calls: Vec<_> = (0..3).map(|i| json!({"id": format!("batch-{i}"), "name": "write_file", "input": {"path": format!("batch-{i}.txt"), "content": format!("result-{i}")}})).collect();
    let prompt = env.prompt(
        &format!(
            "Write all three files. TOOL_BATCH:{}",
            serde_json::to_string(&calls).unwrap()
        ),
        "tool_concurrency",
    );
    let mut sess = env.spawn(&[
        "-p",
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "write_file",
        &prompt,
    ]);
    sess.set_default_timeout(env.timeout());
    assert_eq!(sess.expect_eof().unwrap(), 0);
    for i in 0..3 {
        assert_eq!(
            fs::read_to_string(env.workspace_root().join(format!("batch-{i}.txt"))).unwrap(),
            format!("result-{i}")
        );
    }
    assert_eq!(env.captured_message_count(), 2);
    let body: serde_json::Value =
        serde_json::from_str(env.captured_message_bodies().last().unwrap()).unwrap();
    assert!(body.get("tools").is_none_or(serde_json::Value::is_null));
    assert!(body["messages"]
        .to_string()
        .contains("computation step limit (1 steps)"));
}

#[test]
fn provider_tool_violation_is_rejected_before_dispatch() {
    let env = TestEnv::new_mock("step-budget-violation");
    set_budget(&env, 1);
    let prompt = env.prompt(
        "Repeatedly append step to steps.txt. STEP_BUDGET_VIOLATION",
        "step_budget",
    );
    let mut sess = env.spawn(&[
        "-p",
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
        &prompt,
    ]);
    sess.set_default_timeout(env.timeout());
    assert_ne!(sess.expect_eof().unwrap(), 0);
    assert_eq!(
        fs::read_to_string(env.workspace_root().join("steps.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(env.captured_message_count(), 2);
}

#[test]
fn live_step_budget_preserves_results_across_turns() {
    let env = TestEnv::new("step-budget-live");
    if !env.is_live() {
        return;
    }
    set_budget(&env, 2);
    // Each next command needs a fresh ticket from the preceding real result.
    // The requested five steps are finite; the host must stop them after two.
    fs::write(env.workspace_root().join("step.cjs"), r"
const fs = require('node:fs');
const crypto = require('node:crypto');
const [turn, ticket] = process.argv.slice(2);
if (!['1', '2'].includes(turn)) throw new Error('invalid turn');
const path = `step-state-${turn}.json`;
const state = fs.existsSync(path) ? JSON.parse(fs.readFileSync(path, 'utf8')) : {step: 0, ticket: 'start'};
if (ticket !== state.ticket || state.step >= 5) throw new Error('stale ticket or completed task');
fs.appendFileSync('steps.txt', `turn-${turn}\n`);
state.step++;
state.ticket = crypto.randomBytes(12).toString('hex');
fs.writeFileSync(path, JSON.stringify(state));
console.log(`Completed ${state.step} of 5 steps for turn ${turn}.`);
console.log(state.step === 5 ? 'DONE' : `NEXT_COMMAND: node step.cjs ${turn} ${state.ticket}`);
").unwrap();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let captured = std::env::var_os("SCODE_LIVE_ARTIFACTS")
        .map_or_else(
            || env.workspace_root().join("requests"),
            std::path::PathBuf::from,
        )
        .join(format!("step-budget-{stamp}"));
    fs::create_dir_all(&captured).unwrap();
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
        ],
        &[("SUDOCODE_DUMP_REQUESTS", captured.to_str().unwrap())],
    );
    let mut previous_requests = 0;
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "initial prompt");
    for turn in 1..=2 {
        let prompt = format!("Turn {turn}: complete the five-step fixture in this same user turn. Start by running exactly `node step.cjs {turn} start` with Bash. Read its stdout and immediately execute the returned NEXT_COMMAND verbatim; repeat until DONE. Each command needs the fresh ticket from the previous result, so wait for the result before issuing the next command. Do not wait for another user message. Do not read or edit the script/state, combine commands, use loops, or run in the background. If the host removes tools before DONE, report the actual completed steps and remaining work honestly.");
        let marker = common::turn_status_marker(&sess);
        sess.send(&prompt).unwrap();
        common::expect_screen_settled(
            &sess,
            |s| common::input_line_of(s).contains(&format!("Turn {turn}:")),
            common::DEFAULT_TIMEOUT,
            "turn pasted",
        );
        sess.send("\r").unwrap();
        common::expect_turn_complete_after(
            &sess,
            &marker,
            common::LIVE_TURN_BUDGET,
            "live bounded turn",
        );
        let content =
            fs::read_to_string(env.workspace_root().join("steps.txt")).unwrap_or_else(|error| {
                panic!(
                    "turn {turn} produced no result: {error}\nPTY:\n{}",
                    sess.render(|screen| screen.raw().contents())
                )
            });
        let expected = if turn == 1 {
            "turn-1\nturn-1\n"
        } else {
            "turn-1\nturn-1\nturn-2\nturn-2\n"
        };
        assert_eq!(
            content,
            expected,
            "each turn must stop at two real tool rounds and retain earlier results\nPTY:\n{}",
            sess.render(|screen| screen.raw().contents())
        );
        let mut paths: Vec<_> = fs::read_dir(&captured)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| wire_requests::is_inference_request_dump(path))
            .collect();
        paths.sort();
        let requests: Vec<serde_json::Value> = paths
            .iter()
            .map(|path| serde_json::from_slice(&fs::read(path).unwrap()).unwrap())
            .collect();
        assert_step_budget_requests(&requests, previous_requests, turn);
        previous_requests = requests.len();
    }
    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

#[test]
fn invalid_step_budgets_fail_before_a_model_request() {
    for value in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!(4_294_967_296u64),
        json!("five"),
    ] {
        let env = TestEnv::new_mock("invalid-step-budget");
        let dir = env.workspace_root().join(".nexus/sudocode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            json!({"maxSteps": value}).to_string(),
        )
        .unwrap();
        let mut sess = env.spawn(&["-p", "hello"]);
        sess.set_default_timeout(Duration::from_secs(30));
        assert_ne!(
            sess.expect_eof().unwrap(),
            0,
            "invalid budget accepted: {value}"
        );
        assert_eq!(env.captured_message_count(), 0);
    }
}

fn assert_step_budget_requests(
    requests: &[serde_json::Value],
    previous_requests: usize,
    turn: usize,
) {
    let current = &requests[previous_requests..];
    assert!(
        current.len() >= 3,
        "two tool rounds and a final-answer request must be captured"
    );
    let tool_calls = |request: &serde_json::Value| -> std::collections::BTreeMap<_, _> {
        wire_requests::request_tool_blocks(request)
            .into_iter()
            .filter(|block| block["type"] == "tool_use")
            .map(|block| (block["id"].to_string(), block))
            .collect()
    };
    // Count executed tool rounds, not transport attempts. A legitimate retry
    // can repeat a snapshot without consuming another execution step.
    for request in current {
        let prior_calls = tool_calls(request);
        let has_tools = request["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty());
        if has_tools {
            assert!(prior_calls.len() < turn * 2, "tools offered after the cap");
        } else {
            assert_eq!(
                prior_calls.len(),
                turn * 2,
                "tools withdrawn before two completed rounds in this turn"
            );
            assert!(request
                .get("tool_choice")
                .is_none_or(serde_json::Value::is_null));
        }
    }
    assert!(
        current[0]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "each user turn gets tools again"
    );
    let final_request = current.last().unwrap();
    assert!(
        final_request
            .get("tools")
            .is_none_or(|tools| tools.is_null() || tools.as_array().is_some_and(Vec::is_empty)),
        "final answer must not offer tools"
    );
    let calls = tool_calls(final_request);
    assert_eq!(
        calls.len(),
        turn * 2,
        "each turn adds exactly two real tool calls"
    );
    for call in calls.values() {
        assert!(matches!(call["name"].as_str(), Some("bash" | "Bash")));
        let words = shell_words::split(call["input"]["command"].as_str().unwrap()).unwrap();
        assert_eq!(
            words.len(),
            4,
            "one fixture command, no compound shell or loop"
        );
        assert_eq!(&words[..2], ["node", "step.cjs"]);
        assert!(matches!(words[2].as_str(), "1" | "2"));
        assert!(
            words[3] == "start"
                || (words[3].len() == 24 && words[3].bytes().all(|b| b.is_ascii_hexdigit()))
        );
    }
}
