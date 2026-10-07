//! Exercise the interactive parent → preset → file tools → parent result chain.
mod common;

use std::{fs, time::Duration};

use common::TestEnv;
use serde_json::Value;

#[test]
fn background_verification_reports_without_another_user_prompt() {
    run_background_verification(Workflow::Background);
}

#[test]
fn synchronous_verification_auto_backgrounds_and_reports() {
    run_background_verification(Workflow::AutoBackground);
}

#[test]
fn background_result_queues_while_parent_is_busy() {
    run_background_verification(Workflow::BusyParent);
}

/// Live behavior evaluation: the task asks for a monitoring workflow without
/// naming any tool or preset. Keep model choice out of deterministic CI.
#[test]
#[ignore = "live delegation evaluation"]
fn claude_proactively_delegates_ci_monitoring() {
    run_background_verification(Workflow::Natural);
}

#[derive(Clone, Copy)]
enum Workflow {
    Background,
    AutoBackground,
    BusyParent,
    Natural,
}

#[test]
fn coordinator_does_not_duplicate_completion() {
    let env = TestEnv::new("coordinator-completion");
    let evidence = "COORDINATOR_CHILD_EVIDENCE_7531";
    fs::write(env.workspace_root().join("evidence.txt"), evidence).unwrap();
    let mut parent = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_COORDINATOR_MODE", "1"),
        ],
    );
    common::expect_input_line_cleared(&parent, Duration::from_secs(15), "coordinator ready");
    let prompt = env.prompt("Use agent_spawn with agent Explore and run_in_background true to read evidence.txt and return its exact contents. Finish your turn after spawning, without polling. When the completion arrives, report it verbatim.", "subagent_workflow");
    parent.send(&format!("{prompt}\r")).unwrap();
    parent.set_default_timeout(Duration::from_secs(180));
    parent
        .expect(evidence)
        .expect("coordinator must receive actual child evidence");
    common::expect_input_line_cleared(&parent, Duration::from_secs(15), "coordinator result");
    parent.send("/exit\r").unwrap();
    assert_eq!(parent.expect_eof().unwrap(), 0);
    assert_parent_prefix_stable(&env);
}

fn run_background_verification(workflow: Workflow) {
    let auto_background = matches!(workflow, Workflow::AutoBackground);
    let natural = matches!(workflow, Workflow::Natural);
    let busy_parent = matches!(workflow, Workflow::BusyParent);
    let env = TestEnv::new("background-verification-evidence");
    let evidence = format!(
        "CI-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    fs::write(env.workspace_root().join("ci-result.txt"), &evidence).unwrap();
    let mut parent = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_AGENT_AUTO_BG_SECS", "1"),
            ("SUDOCODE_COORDINATOR_MODE", "0"),
        ],
    );
    common::expect_input_line_cleared(&parent, Duration::from_secs(15), "ready");
    let before = common::turn_status_marker(&parent);
    let prompt = if natural {
        "CI is still running. Please monitor it while we continue working here; this chat needs to stay available. The local CI fixture becomes ready when ci-ready appears; ci-result.txt then contains the build result. Inspect and report the result as soon as CI is ready, without me having to ask again. For now, just let me know monitoring is underway.".to_string()
    } else {
        format!("Use agent_spawn with agent Verification and run_in_background {}. Its task: run `while [ ! -f ci-ready ]; do sleep 0.2; done; cat ci-result.txt` in bash and report the file content exactly. This simulates waiting for CI. After the spawn returns, tell me it is running and finish your turn immediately. Do not use pid_output or wait in the parent. When the result arrives, report it verbatim.", !auto_background)
    };
    parent
        .send(&format!("{}\r", env.prompt(&prompt, "subagent_workflow")))
        .unwrap();
    common::expect_turn_complete_after(
        &parent,
        &before,
        Duration::from_secs(180),
        "background launch",
    );
    assert_eq!(manifests(&env).len(), 1, "child must actually launch");
    if !natural {
        assert_eq!(manifests(&env)[0]["subagentType"], "Verification");
    } else {
        eprintln!(
            "Proactive delegation selected {}",
            manifests(&env)[0]["subagentType"]
        );
    }
    let before = common::turn_status_marker(&parent);
    if busy_parent {
        parent.send("HOLD_PARENT: while CI runs, use bash yourself to run `touch parent-ready; while [ ! -f parent-release ]; do sleep 0.2; done; echo PARENT_WORK_FINISHED`. Then report that command's output.\r").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(180);
        while !env.workspace_root().join("parent-ready").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "parent did not start independent work"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        // Inspect the child during a foreground tool, then dismiss the view.
        // Escape belongs to the browser and must not cancel that tool/turn.
        parent.send("\x1b[B").unwrap();
        common::expect_screen(
            &parent,
            |s| s.contains("Enter to view"),
            Duration::from_secs(30),
            "background footer selected",
        );
        parent.send("\r").unwrap();
        common::expect_screen(
            &parent,
            |s| s.contains("Enter details"),
            Duration::from_secs(30),
            "task browser during parent tool",
        );
        parent.send("\x1b").unwrap();
        common::expect_input_line_cleared(
            &parent,
            Duration::from_secs(30),
            "back to running parent input",
        );
        assert!(!parent.render(|s| s.contents()).contains("[Interrupted"));
    } else {
        parent
            .send("While CI runs, what is 17 + 25? Answer the number only.\r")
            .unwrap();
        common::expect_turn_complete_after(
            &parent,
            &before,
            Duration::from_secs(180),
            "independent parent work",
        );
        assert!(parent.render(|s| s.contents()).contains("42"));
    }
    let before = common::turn_status_marker(&parent);
    fs::write(env.workspace_root().join("ci-ready"), "ready").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        let children = manifests(&env);
        if children.iter().any(|child| child["status"] == "completed") {
            assert!(
                children[0]["result"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(&evidence),
                "{children:?}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child did not finish: {children:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    if busy_parent {
        assert_eq!(
            common::turn_status_marker(&parent),
            before,
            "background completion must not interrupt the parent"
        );
        fs::write(env.workspace_root().join("parent-release"), "ready").unwrap();
        common::expect_screen(
            &parent,
            |s| {
                s.lines()
                    .any(|line| line.trim() == "│ PARENT_WORK_FINISHED")
            },
            Duration::from_secs(180),
            "foreground tool finishes normally after task view dismissal",
        );
        parent.set_default_timeout(Duration::from_secs(180));
        parent
            .expect(&evidence)
            .expect("parent must report the queued evidence");
        common::expect_input_line_cleared(
            &parent,
            Duration::from_secs(15),
            "queued result complete",
        );
    }
    common::expect_turn_complete_after(
        &parent,
        &before,
        if env.is_live() {
            Duration::from_secs(180)
        } else {
            Duration::from_secs(15)
        },
        "automatic result delivery",
    );
    assert!(
        common::screen_contains(&parent.render(|s| s.contents()), &evidence),
        "{}",
        common::screen_tail(&parent, 4000)
    );
    parent.send("/exit\r").unwrap();
    assert_eq!(parent.expect_eof().unwrap(), 0);
    assert_parent_prefix_stable(&env);
}

fn assert_parent_prefix_stable(env: &TestEnv) {
    if env.is_live() {
        return;
    }
    let requests: Vec<Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str::<Value>(body).unwrap())
        .filter(|body| {
            body["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "agent_spawn"))
        })
        .collect();
    assert!(
        requests.len() >= 3,
        "parent launch, follow-up, and notification must reach the provider"
    );
    for request in &requests[1..] {
        assert_eq!(
            request["tools"], requests[0]["tools"],
            "tools prefix changed"
        );
        assert_eq!(
            request["system"], requests[0]["system"],
            "system prefix changed"
        );
    }
    let notifications = requests
        .iter()
        .filter(|request| {
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["role"] == "user"
                        && message["content"].as_array().is_some_and(|content| {
                            content.iter().any(|block| {
                                block["text"]
                                    .as_str()
                                    .is_some_and(|text| text.contains("<task-notification>"))
                            })
                        })
                })
        })
        .count();
    let occurrences: usize = requests
        .iter()
        .flat_map(|request| request["messages"].as_array().unwrap())
        .filter(|message| message["role"] == "user")
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter_map(|block| block["text"].as_str())
        .map(|text| text.matches("<task-notification>").count())
        .sum();
    assert_eq!(
        occurrences, 1,
        "coordinator and event bridge must not duplicate the notification"
    );
    assert_eq!(
        notifications, 1,
        "deliver the result once, after the parent is ready"
    );
}

fn manifests(env: &TestEnv) -> Vec<Value> {
    fs::read_dir(env.workspace_root().join(".sudocode-agents"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| serde_json::from_slice(&fs::read(entry.path()).ok()?).ok())
        .collect()
}

#[test]
fn explicitly_collected_result_does_not_trigger_another_turn() {
    let env = TestEnv::new("collected-background-result");
    let evidence = "COLLECTED_CHILD_EVIDENCE_9274";
    fs::write(env.workspace_root().join("ci-result.txt"), evidence).unwrap();
    let mut parent = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line_cleared(&parent, Duration::from_secs(15), "ready");
    let before = common::turn_status_marker(&parent);
    let prompt = env.prompt("COLLECT_RESULT: Use agent_spawn with agent Verification and run_in_background true to run `while [ ! -f ci-ready ]; do sleep 0.2; done; cat ci-result.txt` in bash. Then explicitly use pid_output with block true to retrieve its result, and report it verbatim.", "subagent_workflow");
    parent.send(&format!("{prompt}\r")).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while manifests(&env).is_empty() {
        assert!(std::time::Instant::now() < deadline, "child did not start");
        std::thread::sleep(Duration::from_millis(100));
    }
    fs::write(env.workspace_root().join("ci-ready"), "ready").unwrap();
    common::expect_turn_complete_after(
        &parent,
        &before,
        Duration::from_secs(180),
        "explicit result collection",
    );
    assert!(common::screen_contains(
        &parent.render(|s| s.contents()),
        evidence
    ));
    let before = common::turn_status_marker(&parent);
    parent
        .send("While CI runs, what is 17 + 25? Answer the number only.\r")
        .unwrap();
    common::expect_turn_complete_after(
        &parent,
        &before,
        Duration::from_secs(180),
        "next user turn",
    );
    parent.send("/exit\r").unwrap();
    assert_eq!(parent.expect_eof().unwrap(), 0);
    if env.is_mock() {
        let requests: Vec<Value> = env
            .captured_message_bodies()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect();
        assert!(
            requests.iter().any(|request| request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"].to_string().contains("workflow_collect"))),
            "parent must actually collect the result"
        );
        assert!(
            !requests.iter().any(|request| request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "user")
                .any(|message| message["content"]
                    .to_string()
                    .contains("<task-notification>"))),
            "do not re-deliver a result already collected by the parent"
        );
    }
}

#[test]
fn resumed_explore_returns_file_evidence() {
    let env = TestEnv::new("resumed-explore-evidence");
    let evidence = format!(
        "EVIDENCE-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    fs::write(env.workspace_root().join("evidence.txt"), &evidence).unwrap();

    let mut first = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line_cleared(&first, Duration::from_secs(15), "fresh session");
    first.send("/exit\r").unwrap();
    assert_eq!(first.expect_eof().unwrap(), 0);

    let mut parent = env.spawn_with_env(
        &["--resume", "--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line_cleared(&parent, Duration::from_secs(15), "resumed session");
    let before = common::turn_status_marker(&parent);
    parent.send(&format!("{}\r", env.prompt("Use agent_spawn with agent Explore and run_in_background false to read evidence.txt and report its exact contents. Only the child should read the file. Then report the child's result verbatim.", "subagent_workflow"))).unwrap();
    common::expect_turn_complete_after(
        &parent,
        &before,
        Duration::from_secs(240),
        "resumed Explore result",
    );
    let children = manifests(&env);
    assert_eq!(children.len(), 1, "expected one child: {children:?}");
    let child = &children[0];
    assert_eq!(child["subagentType"], "Explore", "{child}");
    assert_eq!(child["status"], "completed", "{child}");
    assert!(child["toolUses"].as_u64().unwrap_or(0) > 0, "{child}");
    assert!(
        child["result"]
            .as_str()
            .unwrap_or_default()
            .contains(&evidence),
        "{child}"
    );
    assert!(
        common::screen_contains(&parent.render(|s| s.contents()), &evidence),
        "{}",
        common::screen_tail(&parent, 4000)
    );
    parent.send("/exit\r").unwrap();
    assert_eq!(parent.expect_eof().unwrap(), 0);
}
