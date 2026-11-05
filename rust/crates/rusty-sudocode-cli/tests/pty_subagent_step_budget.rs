//! A subagent turn winds down at its step budget instead of failing.
//!
//! The budget is the user's `maxSteps`. Reaching it withdraws the tools, so
//! the subagent reports what it found and the parent receives that report as
//! a completed result.

mod common;

use std::fs;
use std::time::Duration;

use common::TestEnv;
use serde_json::{json, Value};

#[test]
fn subagent_finishes_at_its_step_budget_with_a_summary() {
    let env = TestEnv::new_mock("subagent-step-budget");
    let budget = 2;
    let dir = env.workspace_root().join(".nexus/sudocode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("settings.json"),
        json!({"maxSteps": budget}).to_string(),
    )
    .unwrap();
    let prompt = env.prompt(
        "Delegate the stepping work to a subagent and report what it returns.",
        "subagent_step_budget_parent",
    );
    let mut session = env.spawn(&["-p", &prompt, "--permission-mode", "danger-full-access"]);
    session.set_default_timeout(Duration::from_secs(90));
    session.expect("subagent returned:").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);

    // The child ran exactly its budget of tool rounds.
    let steps = fs::read_to_string(env.workspace_root().join("steps.txt")).unwrap();
    assert_eq!(steps.lines().count(), budget);

    let requests: Vec<Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    // The child's requests open with its own prompt; the parent's do not.
    let child: Vec<&Value> = requests
        .iter()
        .filter(|request| {
            request["messages"][0]
                .to_string()
                .contains("PARITY_SCENARIO:step_budget ")
        })
        .collect();
    assert_eq!(
        child.len(),
        budget + 1,
        "budget rounds plus the final answer"
    );
    let last = child.last().unwrap();
    assert!(last.get("tools").is_none_or(Value::is_null), "{last}");
    assert!(last.get("tool_choice").is_none_or(Value::is_null), "{last}");

    // The parent received a completed child carrying its wind-down summary.
    let result = requests
        .iter()
        .flat_map(|request| request["messages"].as_array().unwrap())
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| {
            block["type"] == "tool_result" && block["tool_use_id"] == "toolu_step_budget_agent"
        })
        .expect("parent must receive the child's result");
    let text = result["content"].as_str().map_or_else(
        || {
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>()
        },
        str::to_owned,
    );
    let manifest: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(manifest["status"], "completed", "{manifest}");
    assert!(
        manifest["result"]
            .as_str()
            .is_some_and(|text| text.starts_with("Step budget reached. Existing results retained.")),
        "{manifest}"
    );
}
