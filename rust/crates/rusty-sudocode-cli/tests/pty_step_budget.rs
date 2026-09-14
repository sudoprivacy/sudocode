//! Step budgets stop execution and request a tool-free final answer over the real CLI.
mod common;

use common::TestEnv;
use std::{fs, time::Duration};

#[test]
fn step_budget_finishes_with_existing_results() {
    for limit in [5, 10] {
        let env = TestEnv::new(&format!("step-budget-{limit}"));
        let dir = env.workspace_root().join(".nexus/sudocode");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("settings.json"),
            format!(r#"{{"maxSteps":{limit}}}"#),
        )
        .unwrap();
        let prompt = env.prompt("Repeatedly run echo step >> steps.txt. When the step budget is reached, summarize results and suggest next steps.", "step_budget");
        let mut sess = env.spawn(&[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
            &prompt,
        ]);
        sess.resize(40, 200).unwrap();
        sess.set_default_timeout(Duration::from_secs(90));
        if env.is_mock() {
            sess.expect("Step budget reached").unwrap();
            sess.expect("discuss continuing with the user").unwrap();
            assert_eq!(
                fs::read_to_string(env.workspace_root().join("steps.txt"))
                    .unwrap()
                    .lines()
                    .count(),
                limit
            );
            assert_eq!(env.captured_message_count(), limit + 1);
        } else {
            sess.expect("[Nn]ext").unwrap();
        }
    }
}

#[test]
fn answer_before_budget_does_not_force_extra_request() {
    let env = TestEnv::new("step-budget-early-answer");
    let dir = env.workspace_root().join(".nexus/sudocode");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("settings.json"), r#"{"maxSteps":5}"#).unwrap();
    let prompt = env.prompt("What is 2 + 2?", "single_turn_text");
    let mut sess = env.spawn(&[&prompt]);
    sess.set_default_timeout(Duration::from_secs(60));
    if env.is_mock() {
        sess.expect("The answer is 4").unwrap();
        assert_eq!(env.captured_message_count(), 1);
        assert!(!env.workspace_root().join("steps.txt").exists());
    } else {
        sess.expect("4").unwrap();
    }
}
