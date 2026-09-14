//! Live regression for model inheritance through the real CLI tool loop.
mod common;

use std::fs;
use std::path::Path;

use common::{TestEnv, LIVE_TIMEOUT};
use serde_json::Value;

fn collect_spawns(dir: &Path, spawns: &mut Vec<(String, Value)>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_spawns(&path, spawns);
        } else if path
            .file_name()
            .is_some_and(|name| name == "transcript.jsonl")
        {
            for line in fs::read_to_string(path).unwrap().lines() {
                let row: Value = serde_json::from_str(line).unwrap();
                let message = &row["message"];
                if let Some(blocks) = message["blocks"].as_array() {
                    for block in blocks {
                        if matches!(block["name"].as_str(), Some("agent_spawn" | "Agent")) {
                            let input =
                                serde_json::from_str(block["input"].as_str().unwrap()).unwrap();
                            spawns.push((message["model"].as_str().unwrap().to_string(), input));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn subagent_and_result_summary_inherit_current_parent_model() {
    let env = TestEnv::new("agent-model-inheritance");
    if !env.is_live() || std::env::var_os("SCODE_LIVE_AGENT_TESTS").is_none() {
        eprintln!(
            "SKIP: requires SCODE_TEST_BACKEND=live and SCODE_LIVE_AGENT_TESTS=1; \
             live agent orchestration is model- and service-dependent"
        );
        return;
    }
    let fixture = "MODEL_INHERITANCE_QW: retain the complete deployment checklist, verify the production configuration, run the regression checks, record the results, and report every unresolved blocker before the release.";
    fs::write(env.workspace_root().join("model-check.txt"), fixture).unwrap();
    let prompt = "Call agent_spawn exactly once with agent Explore, fresh true, run_in_background false, description 'check model inheritance', and prompt 'Read model-check.txt and return its entire contents verbatim'. Omit model and auth_mode entirely: this test must exercise inheritance. Wait for completion using pid_output if necessary. Report the child result. Do not read the fixture yourself or create files.";
    let mut cli = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", "--print", prompt],
        &[("SUDOCODE_AGENT_SUMMARY_THRESHOLD_CHARS", "80")],
    );
    cli.set_default_timeout(LIVE_TIMEOUT.saturating_mul(8));
    assert_eq!(cli.expect_eof().unwrap(), 0);

    let mut spawns = Vec::new();
    collect_spawns(&env.workspace_root().join(".scode"), &mut spawns);
    assert_eq!(
        spawns.len(),
        1,
        "must actually delegate exactly once: {}",
        cli.render(|screen| screen.contents())
    );
    let (parent_model, input) = &spawns[0];
    assert!(
        input.get("model").is_none(),
        "must test implicit inheritance"
    );
    assert!(input.get("auth_mode").is_none());
    let manifests: Vec<Value> = fs::read_dir(env.workspace_root().join(".sudocode-agents"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap())
        .collect();
    assert_eq!(manifests.len(), 1);
    let child = &manifests[0];
    assert_eq!(child["model"], parent_model.as_str());
    assert_eq!(child["status"], "completed", "{child}");
    assert!(child["result"]
        .as_str()
        .unwrap()
        .contains("MODEL_INHERITANCE_QW"));
    let full = child["resultFullPath"]
        .as_str()
        .expect("must exercise result summarization");
    assert!(fs::read_to_string(full).unwrap().contains(fixture));
}
