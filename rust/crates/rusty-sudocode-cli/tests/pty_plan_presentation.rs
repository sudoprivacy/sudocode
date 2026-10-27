//! Thinking, plan review, feedback and a revised document through the real REPL.
//! The existing provider batch fixture supplies deterministic content in CI.
mod common;

use common::{TestEnv, LIVE_TURN_BUDGET};
use pty_expect::PtySession;
use serde_json::{json, Value};

const FEEDBACK: &str = "Reuse the existing reader. Preserve ReaderReuseMarker.";

fn choose(sess: &mut PtySession, queue: bool, value: &str, budget: std::time::Duration) {
    if !queue {
        common::expect_screen(
            sess,
            |screen| screen.contains("Your choice:"),
            budget,
            "choice editor ready",
        );
    }
    if queue && value == FEEDBACK {
        sess.send("\x1b[B\x1b[B\x1b[B\x1b[B").unwrap();
    }
    sess.send(value).unwrap();
    if queue && value != FEEDBACK {
        return;
    }
    common::expect_screen(
        sess,
        |screen| {
            screen.contains(&if queue {
                format!("> [+] {value}")
            } else {
                format!("Your choice: {value}")
            })
        },
        budget,
        "review answer entered",
    );
    sess.send("\r").unwrap();
}

// PTY screen callbacks are higher-ranked borrows; method pointers cannot replace them.
#[allow(clippy::redundant_closure_for_method_calls)]
fn roundtrip(queue: bool) {
    let env = TestEnv::new("plan-presentation");
    let path = env.workspace_root().join("reviewed-plan.md");
    let path_text = path.to_str().unwrap();
    let mut sess = start(&env, queue, &path);
    let prompt = review_prompt(&env);
    // Classic Windows input is native console input; avoid multiline pastes.
    if queue {
        sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    } else {
        sess.send(&prompt).unwrap();
    }
    common::expect_screen(
        &sess,
        |s| !common::input_line_of(s).is_empty(),
        env.timeout(),
        "plan request entered",
    );
    sess.send("\r").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| {
            s.contains("Plan · awaiting review")
                && s.contains("│ PresentationStageA")
                && s.contains("Keep context & execute")
        },
        LIVE_TURN_BUDGET,
        "first framed plan",
    );
    let screen = sess.render(|s| s.contents());
    assert!(screen.contains("File:"));
    if env.is_mock() {
        assert!(
            screen.contains("✻ Thinking"),
            "reasoning remains visible: {screen}"
        );
        let lines: Vec<_> = screen.lines().collect();
        let start = lines
            .iter()
            .rposition(|line| line.contains("╭─ Plan · awaiting review"))
            .unwrap();
        assert!(
            start > 0 && lines[start - 1].trim().is_empty(),
            "separate thinking and plan: {screen}"
        );
    }
    let original = std::fs::read_to_string(&path).unwrap();
    assert!(original.contains("PresentationStageA"));
    choose(&mut sess, queue, FEEDBACK, env.timeout());
    common::expect_screen_settled(
        &sess,
        |s| {
            s.contains("WritePlan · revision requested")
                && s.contains("│ PresentationStageB")
                && s.contains("Keep context & execute")
        },
        LIVE_TURN_BUDGET,
        "feedback and revised plan",
    );
    let screen = sess.render(|s| s.contents());
    assert!(
        screen.contains("Feedback:"),
        "feedback is a review result: {screen}"
    );
    assert!(
        !screen.contains("write_plan(acceptance="),
        "no raw argument dump: {screen}"
    );
    let revised = std::fs::read_to_string(&path).unwrap();
    assert!(revised.contains("PresentationStageB") && revised.contains("ReaderReuseMarker"));
    let marker = common::turn_status_marker(&sess);
    choose(&mut sess, queue, "4", env.timeout());
    common::expect_turn_complete_after(
        &sess,
        &marker,
        LIVE_TURN_BUDGET,
        "review cancelled normally",
    );
    common::expect_screen(
        &sess,
        |s| s.contains("WritePlan · review cancelled"),
        env.timeout(),
        "cancelled review card",
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), revised);

    if env.is_mock() {
        assert_mock_decisions(&env, path_text);
    }
    common::expect_input_line_cleared(&sess, env.timeout(), "review returns input");
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", env.timeout(), "exit entered");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    verify_resume(&env, queue, &path);
}

fn review_prompt(env: &TestEnv) -> String {
    let calls = json!([
        {
            "id": "presentation-a", "name": "write_plan",
            "stream_thinking": [{"thinking": "Compare the existing readers before proposing the implementation.", "signature": "sig_plan_presentation"}],
            "input": {"context": "Review shared reader behavior.", "constraints": "Reuse existing code.", "acceptance": "The original reader still works.", "content": "# PresentationStageA\n\nInspect the reader before implementation."}
        },
        {
            "id": "presentation-b", "name": "write_plan",
            "input": {"context": "Review shared reader behavior.", "constraints": "Reuse existing code.", "acceptance": FEEDBACK, "content": "# PresentationStageB\n\nUse the shared reader and verify ReaderReuseMarker."}
        }
    ]);
    if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        "Use write_plan to propose a small shared-reader change. Title its first content heading PresentationStageA, and wait for review. After my feedback, call write_plan again with heading PresentationStageB and include ReaderReuseMarker exactly in its acceptance criteria. Wait for the second decision. If I exit review, leave the draft and stop. Do not execute code or call any other tools.".into()
    }
}

fn start(env: &TestEnv, queue: bool, path: &std::path::Path) -> PtySession {
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan",
        ],
        &[
            (
                "SUDOCODE_INTERRUPT_QUEUE_MODE",
                if queue { "queue" } else { "off" },
            ),
            ("SUDOCODE_MAX_TOOL_USE_CONCURRENCY", "1"),
            ("SUDOCODE_PLAN_FILE", path.to_str().unwrap()),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    sess.resize(100, 100).unwrap();
    common::expect_input_line_cleared(&sess, env.timeout(), "presentation input ready");
    sess
}

fn assert_mock_decisions(env: &TestEnv, path_text: &str) {
    let requests = env.captured_message_bodies();
    let request: Value = serde_json::from_str(requests.last().unwrap()).unwrap();
    let results: Vec<_> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter(|block| block["type"] == "tool_result")
        .collect();
    assert_eq!(results.len(), 2);
    for (block, status) in results.iter().zip(["revision_requested", "cancelled"]) {
        assert_ne!(
            block["is_error"], true,
            "a normal review is not a tool failure"
        );
        let content = block["content"].as_str().map_or_else(
            || {
                block["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            str::to_owned,
        );
        let result: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(result["status"], status);
        assert_eq!(result["planFile"], path_text);
    }
}

#[allow(clippy::redundant_closure_for_method_calls)]
fn verify_resume(env: &TestEnv, queue: bool, path: &std::path::Path) {
    let replacement = "# A newer unreviewed draft\n";
    std::fs::write(path, replacement).unwrap();
    let requests_before = env.is_mock().then(|| env.captured_message_bodies().len());
    let mut resumed = env.spawn_with_env(
        &["--resume", "latest", "--permission-mode", "workspace-write"],
        &[
            (
                "SUDOCODE_INTERRUPT_QUEUE_MODE",
                if queue { "queue" } else { "off" },
            ),
            ("NO_COLOR", ""),
        ],
    );
    resumed.resize(100, 100).unwrap();
    common::expect_screen_settled(
        &resumed,
        |s| {
            s.contains("│ PresentationStageA")
                && s.contains("│ PresentationStageB")
                && s.contains("WritePlan · review cancelled")
        },
        env.timeout(),
        "resume restores both recorded proposals",
    );
    let screen = resumed.render(|s| s.contents());
    for title in ["│ PresentationStageA", "│ PresentationStageB"] {
        assert_eq!(
            screen.lines().filter(|line| line.contains(title)).count(),
            1,
            "one restored document: {screen}"
        );
    }
    assert!(!screen.contains("A newer unreviewed draft"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), replacement);
    if let Some(requests_before) = requests_before {
        assert_eq!(
            env.captured_message_bodies().len(),
            requests_before,
            "resume requires no model request"
        );
    }
    common::expect_input_line_cleared(&resumed, env.timeout(), "resumed input ready");
    resumed.send("/exit").unwrap();
    common::expect_input_line(&resumed, "/exit", env.timeout(), "resumed exit entered");
    resumed.send("\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);
}

#[test]
fn async_review_separates_thinking_plan_and_revision() {
    roundtrip(true);
}

#[test]
fn classic_review_shares_plan_and_decision_formatting() {
    roundtrip(false);
}

#[test]
fn older_transcript_retains_its_recorded_proposal_and_feedback() {
    let env = TestEnv::new_mock("legacy-plan-presentation");
    let mut initial = env.spawn(&["--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&initial, env.timeout(), "initial input ready");
    initial.send("/exit\r").unwrap();
    assert_eq!(initial.expect_eof().unwrap(), 0);
    let transcript =
        common::find_session_transcript(&env.workspace_root().join(".scode/sessions")).unwrap();
    let session = runtime::Session::load_from_path(&transcript).unwrap();
    let mut meta: Value = serde_json::from_str(&session.to_json().unwrap().render()).unwrap();
    meta["type"] = json!("session_meta");
    let messages = [
        json!({"role":"user", "blocks":[{"type":"text", "text":"Review the legacy reader plan."}]}),
        json!({"role":"assistant", "blocks":[{"type":"tool_use", "id":"legacy-plan", "name":"write_plan", "input":{"content":"# LegacyRecordedPlan\n\nPreserve the old proposal.", "context":"Reader context.", "constraints":"Reuse existing code.", "acceptance":"The reader still works."}}]}),
        json!({"role":"tool", "blocks":[{"type":"tool_result", "tool_use_id":"legacy-plan", "tool_name":"write_plan", "output":"User feedback on the plan: LegacyFeedbackMarker\nRevise the plan accordingly and call write_plan again.", "is_error":true}]}),
    ];
    let mut lines = vec![meta.to_string()];
    lines.extend(
        messages
            .into_iter()
            .map(|message| json!({"type":"message", "message":message}).to_string()),
    );
    std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
    let mut resumed = env.spawn(&["--resume", "latest", "--permission-mode", "read-only"]);
    resumed.resize(70, 100).unwrap();
    common::expect_screen_settled(
        &resumed,
        |screen| {
            screen.contains("Plan · recorded proposal")
                && screen.contains("│ LegacyRecordedPlan")
                && screen.contains("LegacyFeedbackMarker")
        },
        env.timeout(),
        "legacy recorded plan and feedback",
    );
    common::expect_input_line_cleared(&resumed, env.timeout(), "legacy resume input ready");
    resumed.send("/exit").unwrap();
    common::expect_input_line(&resumed, "/exit", env.timeout(), "legacy exit entered");
    resumed.send("\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);
    assert_eq!(env.captured_message_count(), 0);
}
