//! Step budgets stop execution through the real CLI and shared provider builder.
mod common;

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
        assert!(final_request.get("tools").is_none_or(|v| v.is_null()));
        assert!(final_request.get("tool_choice").is_none_or(|v| v.is_null()));
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
    sess.set_default_timeout(Duration::from_secs(60));
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
    assert!(body.get("tools").is_none_or(|v| v.is_null()));
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
    let mut sess = env.spawn(&[
        "--permission-mode",
        "danger-full-access",
        "--allowedTools",
        "bash",
    ]);
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "initial prompt");
    for turn in 1..=2 {
        let prompt = format!("Turn {turn}: you must use bash to append one line to steps.txt in each separate tool round. Command: echo step >> steps.txt. Keep doing this indefinitely, with exactly one bash call per model response. When no tools remain, summarize the completed work and remaining work. Do not finish early.");
        let marker = common::turn_status_marker(&sess);
        sess.send(&prompt).unwrap();
        common::expect_screen_settled(
            &sess,
            |s| common::input_line_of(s).contains(&format!("Turn {turn}:")),
            common::DEFAULT_TIMEOUT,
            "turn pasted",
        );
        sess.send("\r").unwrap();
        common::expect_turn_complete_after(&sess, &marker, env.timeout(), "live bounded turn");
        assert_eq!(
            fs::read_to_string(env.workspace_root().join("steps.txt"))
                .unwrap()
                .lines()
                .count(),
            turn * 2
        );
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
