//! Review the persisted Markdown document, then execute it without another user
//! prompt. Both REPLs use the same engine transition and presentation rules.
mod common;

use std::time::Duration;

use common::{TestEnv, LIVE_TURN_BUDGET};
use mock_anthropic_service::PLAN_REVIEW_MARKDOWN;
use pty_expect::PtySession;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

fn start(env: &TestEnv, queue: bool, no_color: bool) -> PtySession {
    let plan = env.workspace_root().join("reviewed-plan.md");
    let todos = env.workspace_root().join("todos.json");
    std::fs::write(&todos, r#"[{"content":"carryforwardtodo","status":"in_progress","activeForm":"doing carryforwardtodo"}]"#).unwrap();
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan,write_file,AskUserQuestion",
        ],
        &[
            (
                "SUDOCODE_INTERRUPT_QUEUE_MODE",
                if queue { "queue" } else { "off" },
            ),
            ("SUDOCODE_PLAN_FILE", plan.to_str().unwrap()),
            ("SUDOCODE_TODO_STORE", todos.to_str().unwrap()),
            ("NO_COLOR", if no_color { "1" } else { "" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    common::expect_input_line_cleared(
        &sess,
        env.timeout().max(Duration::from_secs(30)),
        "ready to review a plan",
    );
    sess.resize(100, 100).unwrap();
    sess
}

fn submit_plan(env: &TestEnv, sess: &mut PtySession) {
    let prompt = env.prompt(&format!(
        "Earlier I considered editing the sample application. I have instead chosen the plan below. Call write_plan now with exactly this Markdown content:\n{PLAN_REVIEW_MARKDOWN}\nSet context to 'Review context marker', constraints to 'Only write the approved artifact.', and acceptance to 'The artifact contains the approved text.'. Wait for the tool's approval decision before using write_file to execute the plan."
    ), "plan_execution_roundtrip");
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    sess.send("\r").unwrap();
    common::expect_screen(
        sess,
        |screen| {
            screen.contains("Choose an action")
                && screen.contains("Keep context & execute")
                && screen.contains("Verify bold")
                && screen.contains("let verified = true;")
        },
        LIVE_TURN_BUDGET,
        "full Markdown approval document",
    );
}

fn assert_style(sess: &PtySession, needle: &str, bold: bool, colored: bool) {
    sess.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        let row = rows
            .iter()
            .rposition(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?}: {}", raw.contents()));
        let byte = rows[row].find(needle).unwrap();
        let col = rows[row][..byte].width();
        let cell = raw
            .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
            .unwrap();
        assert_eq!(cell.bold(), bold, "{needle}: {}", raw.contents());
        assert_eq!(
            format!("{:?}", cell.fgcolor()) != "Default",
            colored,
            "{needle}: {}",
            raw.contents()
        );
    });
}

fn assert_document(sess: &PtySession, no_color: bool) {
    assert_style(sess, "Review plan", !no_color, false);
    assert_style(sess, "Verify bold", !no_color, false);
    assert_style(sess, "src/main.rs", false, !no_color);
    assert_style(sess, "Context", !no_color, false);
    sess.render(|screen| {
        let text = screen.raw().contents();
        let start = text
            .rfind("[Choose an action]")
            .or_else(|| text.rfind("Choose an action\n"));
        if let Some(start) = start {
            let document = &text[start..];
            assert!(
                !document.contains("**Verify bold**"),
                "emphasis must be rendered: {document}"
            );
            assert!(
                !document.contains("```rust"),
                "fences must be rendered: {document}"
            );
        }
    });
}

fn choose(sess: &mut PtySession, queue: bool, choice: &str) {
    if queue {
        sess.send(choice).unwrap();
    } else {
        common::expect_screen(
            sess,
            |screen| screen.contains("Your choice:"),
            LIVE_TURN_BUDGET,
            "choice editor ready",
        );
        sess.send(choice).unwrap();
        common::expect_screen(
            sess,
            |screen| screen.contains(&format!("Your choice: {choice}")),
            LIVE_TURN_BUDGET,
            "choice entered",
        );
        sess.send("\r").unwrap();
    }
}

fn finish(sess: &mut PtySession) {
    common::expect_input_line_cleared(sess, LIVE_TURN_BUDGET, "REPL rearmed");
    sess.send("/exit").unwrap();
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit entered");
    sess.send("\r").unwrap();
    sess.set_default_timeout(Duration::from_secs(30));
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

fn assert_requests(env: &TestEnv, plan: &str, clear: bool) {
    if env.is_live() {
        return;
    }
    let requests: Vec<Value> = env
        .captured_message_bodies()
        .iter()
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    let resumed = requests
        .iter()
        .find(|request| {
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["content"].as_array().unwrap().iter().any(|block| {
                        block["type"] == "tool_result"
                            && block["content"]
                                .to_string()
                                .contains("The user APPROVED the plan")
                    })
                })
        })
        .expect("approval must reach a provider request without another user prompt");
    for field in [
        "system",
        "tools",
        "model",
        "thinking",
        "tool_choice",
        "metadata",
        "output_config",
    ] {
        assert_eq!(
            requests[0][field], resumed[field],
            "stable provider prefix: {field}"
        );
    }
    assert_eq!(
        resumed["messages"]
            .to_string()
            .contains("Earlier I considered editing the sample application."),
        !clear
    );
    if clear {
        assert!(
            resumed["messages"].to_string().contains("carryforwardtodo"),
            "todos survive clearing exploration history"
        );
    }
    let blocks: Vec<_> = resumed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .collect();
    let result = blocks
        .iter()
        .find(|block| {
            block["type"] == "tool_result"
                && block["content"]
                    .to_string()
                    .contains("The user APPROVED the plan")
        })
        .unwrap();
    let text = result["content"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });
    assert!(
        text.contains(plan),
        "execution must receive the reviewed file, including framing"
    );
    assert!(
        !text.contains("you will receive"),
        "no future prompt promise"
    );
    assert!(
        blocks
            .iter()
            .any(|block| block["type"] == "tool_use" && block["id"] == result["tool_use_id"]),
        "preserve tool pairing across reset"
    );
}

fn approve(queue: bool, clear: bool, no_color: bool) {
    let env = TestEnv::new("plan-markdown");
    let mut sess = start(&env, queue, no_color);
    submit_plan(&env, &mut sess);
    let saved = std::fs::read_to_string(env.workspace_root().join("reviewed-plan.md"))
        .expect("the complete source must already be persisted while approval is pending");
    for section in ["## Context", "## Constraints", "## Acceptance Criteria"] {
        assert!(
            saved.contains(section),
            "saved plan missing {section}: {saved}"
        );
    }
    // A real model may compose the fixture's accented character. The raw
    // saved source remains authoritative; mock HTTP assertions below compare
    // that source byte-for-byte with the approval result sent for execution.
    let comparable = if env.is_live() {
        saved.replace('é', "e\u{301}")
    } else {
        saved.clone()
    };
    assert!(
        comparable.contains(PLAN_REVIEW_MARKDOWN),
        "saved document: {saved}"
    );
    assert_document(&sess, no_color);
    if queue {
        sess.resize(100, 72).unwrap();
        common::expect_screen(
            &sess,
            |s| s.contains("let verified = true;") && s.contains("Keep context & execute"),
            env.timeout(),
            "reflowed plan",
        );
        assert_document(&sess, no_color);
    }
    let marker = common::turn_status_marker(&sess);
    choose(&mut sess, queue, if clear { "1" } else { "2" });
    common::expect_turn_complete_after(
        &sess,
        &marker,
        LIVE_TURN_BUDGET,
        "approved plan executes in the same turn",
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("plan-executed.txt"))
            .unwrap()
            .trim(),
        "completed from approved plan"
    );
    assert_requests(&env, &saved, clear);
    finish(&mut sess);
    let transcript = find_transcript(&env.workspace_root().join(".scode"));
    let persisted = runtime::Session::load_from_path(&transcript).unwrap();
    let text = format!("{:?}", persisted.messages);
    assert_eq!(
        text.contains("Earlier I considered editing the sample application."),
        !clear
    );
    assert!(text.contains("The user APPROVED the plan"));
    assert_eq!(persisted.compaction.is_some(), clear);
    if clear {
        assert!(text.contains("carryforwardtodo"));
        assert!(
            std::fs::read_dir(transcript.parent().unwrap())
                .unwrap()
                .any(|entry| {
                    let path = entry.unwrap().path();
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains("before-compact-")
                        && std::fs::read_to_string(path)
                            .unwrap()
                            .contains("Earlier I considered editing the sample application.")
                }),
            "previous history is archived before replacement"
        );
    }
}

fn find_transcript(dir: &std::path::Path) -> std::path::PathBuf {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name == "transcript.jsonl")
            {
                return path;
            }
        }
    }
    panic!("missing persisted session");
}

#[test]
fn async_plan_markdown_clear_and_execute_without_another_prompt() {
    approve(true, true, false);
}
#[test]
fn sync_plan_uses_the_same_rendering_and_continuation() {
    approve(false, true, false);
}
#[test]
fn no_color_keeps_markdown_structure_and_keep_context_executes() {
    approve(true, false, true);
}

#[test]
fn rejection_keeps_the_saved_draft_without_executing() {
    let env = TestEnv::new("plan-reject");
    let mut sess = start(&env, true, false);
    submit_plan(&env, &mut sess);
    let marker = common::turn_status_marker(&sess);
    choose(&mut sess, true, "4");
    common::expect_turn_complete_after(&sess, &marker, LIVE_TURN_BUDGET, "rejection completes");
    assert!(!env.workspace_root().join("plan-executed.txt").exists());
    assert!(env.workspace_root().join("reviewed-plan.md").exists());
    finish(&mut sess);
}

#[test]
fn a_changed_file_cannot_execute_under_a_stale_approval() {
    let env = TestEnv::new("plan-stale");
    let mut sess = start(&env, true, false);
    submit_plan(&env, &mut sess);
    std::fs::write(
        env.workspace_root().join("reviewed-plan.md"),
        "# Replacement draft\n",
    )
    .unwrap();
    let marker = common::turn_status_marker(&sess);
    choose(&mut sess, true, "1");
    common::expect_turn_complete_after(&sess, &marker, LIVE_TURN_BUDGET, "stale approval rejected");
    assert!(!env.workspace_root().join("plan-executed.txt").exists());
    finish(&mut sess);
}

#[test]
fn fuzzy_questions_share_markdown_and_remain_responsive() {
    let env = TestEnv::new("question-markdown");
    let mut sess = start(&env, true, false);
    let prompt = env.prompt(&format!("Call AskUserQuestion with title 'Markdown question', description exactly {PLAN_REVIEW_MARKDOWN:?}, and twelve options labelled 'Option 1' through 'Option 12'."), "question_markdown_many");
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    sess.send("\r").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("12 items") && s.contains("Verify bold"),
        LIVE_TURN_BUDGET,
        "Markdown fuzzy question",
    );
    assert_style(&sess, "Verify bold", true, false);
    assert_style(&sess, "src/main.rs", false, true);
    sess.send("Option 12").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("1 match") && s.contains("Option 12"),
        env.timeout(),
        "filter remains responsive",
    );
    let marker = common::turn_status_marker(&sess);
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(&sess, &marker, LIVE_TURN_BUDGET, "question answered");
    finish(&mut sess);
}

#[test]
fn cancelling_review_does_not_schedule_execution_on_the_next_turn() {
    let env = TestEnv::new("plan-cancel");
    let mut sess = start(&env, true, false);
    submit_plan(&env, &mut sess);
    sess.send("\x1b").unwrap();
    common::expect_input_line_cleared(&sess, LIVE_TURN_BUDGET, "cancelled review returns input");
    assert!(!env.workspace_root().join("plan-executed.txt").exists());
    let marker = common::turn_status_marker(&sess);
    let prompt = env.prompt(
        "Say hello only. Leave the saved plan as a draft.",
        "single_turn_text",
    );
    sess.send(&prompt).unwrap();
    common::expect_input_line(
        &sess,
        "Say hello only",
        env.timeout(),
        "next prompt entered",
    );
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(
        &sess,
        &marker,
        LIVE_TURN_BUDGET,
        "next turn completes without stale approval",
    );
    assert!(!env.workspace_root().join("plan-executed.txt").exists());
    finish(&mut sess);
    let persisted =
        runtime::Session::load_from_path(find_transcript(&env.workspace_root().join(".scode")))
            .unwrap();
    assert!(
        persisted.compaction.is_none(),
        "cancelled review must not clear history"
    );
}

#[test]
fn one_shot_saves_a_draft_without_inventing_user_approval() {
    let env = TestEnv::new("plan-noninteractive");
    let plan = env.workspace_root().join("reviewed-plan.md");
    let prompt = env.prompt("Call write_plan to propose creating plan-executed.txt. Save the draft for later review, without executing it.", "plan_execution_roundtrip");
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan,write_file",
            &prompt,
        ],
        &[("SUDOCODE_PLAN_FILE", plan.to_str().unwrap())],
    );
    sess.set_default_timeout(LIVE_TURN_BUDGET);
    assert_eq!(sess.expect_eof().unwrap(), 0);
    assert!(plan.exists());
    assert!(!env.workspace_root().join("plan-executed.txt").exists());
    let persisted =
        runtime::Session::load_from_path(find_transcript(&env.workspace_root().join(".scode")))
            .unwrap();
    assert!(!format!("{:?}", persisted.messages).contains("The user APPROVED the plan"));
    assert!(persisted.compaction.is_none());
}
