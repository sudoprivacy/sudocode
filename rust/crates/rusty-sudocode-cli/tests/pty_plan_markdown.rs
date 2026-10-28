//! Review the persisted Markdown document, then execute it without another user
//! prompt. Both REPLs use the same engine transition and presentation rules.
mod common;

use std::cell::RefCell;
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
            "write_plan,write_file,read_file,AskUserQuestion",
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
    // Set the document viewport before waiting for the editor. Resizing a
    // ready Windows console and immediately pasting races its prompt repaint.
    sess.resize(100, 100).unwrap();
    common::expect_input_line_cleared(
        &sess,
        env.timeout().max(Duration::from_secs(30)),
        "ready to review a plan",
    );
    sess
}

fn submit_plan(env: &TestEnv, sess: &mut PtySession, queue: bool) {
    let prompt = if env.is_live() {
        env.prompt(&format!(
            "Earlier I considered editing the sample application. I have instead chosen the plan below. Call write_plan now. Set content to exactly the Markdown between BEGIN_PLAN_MARKDOWN and END_PLAN_MARKDOWN, excluding the delimiter lines. Only that Markdown belongs in content; do not include the instructions or the separate fields below.\nBEGIN_PLAN_MARKDOWN\n{PLAN_REVIEW_MARKDOWN}\nEND_PLAN_MARKDOWN\nSet the independent context field to 'Review context marker', constraints field to 'Only write the approved artifact.', and acceptance field to 'The artifact contains the approved text.'. Wait for the tool's approval decision before using write_file to execute the plan."
        ), "plan_execution_roundtrip")
    } else {
        env.prompt(
            "Earlier I considered editing the sample application.",
            "plan_execution_roundtrip",
        )
    };
    let input = if !queue && env.is_live() {
        // Rustyline on Windows reads native console events. ANSI bracketed
        // paste does not preserve its multiline payload there. As in the
        // transcript-spacing live test, read the original instructions from
        // the fixture instead of changing the shared OS clipboard.
        std::fs::write(env.workspace_root().join("plan-input.md"), &prompt).unwrap();
        "Earlier I considered editing the sample application. Read plan-input.md and follow it."
            .to_string()
    } else {
        prompt.clone()
    };
    if queue {
        sess.send(&format!("\x1b[200~{input}\x1b[201~")).unwrap();
    } else {
        sess.send(&input).unwrap();
    }
    let expected_input = if env.is_live() && queue {
        format!("[Pasted text #1 +{} lines]", prompt.matches('\n').count())
    } else {
        input.clone()
    };
    common::expect_screen(
        sess,
        |screen| {
            if env.is_live() && !queue {
                common::input_line_of(screen) == expected_input
            } else {
                common::input_line_of(screen) == expected_input
            }
        },
        env.timeout(),
        "complete plan prompt pasted into current input",
    );
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct GlyphStyle {
    row: u16,
    col: u16,
    bold: bool,
    foreground: String,
}

struct StyledSnapshot {
    contents: String,
    samples: Vec<Option<GlyphStyle>>,
}

fn assert_styles(sess: &PtySession, expected: &[(&str, bool, bool)]) -> String {
    let previous = RefCell::new(None);
    let captured = RefCell::new(None);
    // ConPTY can deliver a repaint in several chunks. Observe all sampled
    // glyphs in one frame, with stable positions and styles. The spinner may
    // keep animating outside this document, so the whole screen need not stop.
    common::expect_screen(
        sess,
        |text| {
            let snapshot = sess.render(|screen| {
                let raw = screen.raw();
                let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
                let samples = expected
                    .iter()
                    .map(|(needle, _, _)| {
                        // User input and the running tool's summary may quote
                        // the same Markdown after the document entered history.
                        let row = rows.iter().rposition(|row| {
                            row.contains(needle)
                                && !matches!(row.trim_start().chars().next(), Some('❯' | '╭'))
                        })?;
                        let byte = rows[row].find(needle)?;
                        let col = rows[row][..byte].width();
                        let row = row.try_into().ok()?;
                        let col = col.try_into().ok()?;
                        let cell = raw.cell(row, col)?;
                        Some(GlyphStyle {
                            row,
                            col,
                            bold: cell.bold(),
                            foreground: format!("{:?}", cell.fgcolor()),
                        })
                    })
                    .collect::<Vec<_>>();
                StyledSnapshot {
                    contents: raw.contents(),
                    samples,
                }
            });
            let is_complete =
                snapshot.contents == text && snapshot.samples.iter().all(Option::is_some);
            let are_styles_stable = previous.borrow().as_ref() == Some(&snapshot.samples);
            previous.replace(Some(snapshot.samples.clone()));
            captured.replace(Some(snapshot));
            is_complete && are_styles_stable
        },
        common::DEFAULT_TIMEOUT,
        "complete and settled plan text and styles",
    );
    let snapshot = captured.into_inner().unwrap();
    // Assert after releasing the PTY reader's mutex. An assertion failure
    // must not poison that mutex and obscure the original diagnostic.
    for ((needle, bold, colored), sample) in expected.iter().zip(&snapshot.samples) {
        let actual = sample.as_ref().unwrap();
        assert_eq!(actual.bold, *bold, "{needle}: {}", snapshot.contents);
        assert_eq!(
            actual.foreground != "Default",
            *colored,
            "{needle}: {}",
            snapshot.contents
        );
    }
    snapshot.contents
}

fn assert_document(sess: &PtySession, no_color: bool) {
    let text = assert_styles(
        sess,
        &[
            ("Review plan", !no_color, false),
            ("Verify bold", !no_color, false),
            ("src/main.rs", false, !no_color),
            ("Context", !no_color, false),
        ],
    );
    let start = text
        .rfind("╭─ Plan · awaiting review")
        .expect("the complete saved plan has its own review frame");
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
    let review: runtime::plan_store::PlanReviewResult = serde_json::from_str(&text).unwrap();
    assert_eq!(
        review.status,
        runtime::plan_store::PlanReviewStatus::Approved
    );
    assert!(
        !text.contains("you will receive"),
        "no future prompt promise"
    );
    let call = blocks
        .iter()
        .find(|block| block["type"] == "tool_use" && block["id"] == result["tool_use_id"])
        .expect("preserve tool pairing across reset");
    let reviewed = call["input"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let lines: Vec<_> = plan
        .lines()
        .map(str::trim)
        .filter(|line| line.len() > 30)
        .collect();
    assert!(
        !lines.is_empty(),
        "the reviewed plan has lines worth checking"
    );
    for line in lines {
        assert!(
            reviewed.contains(line),
            "execution must see the reviewed plan through its write_plan call: {line}"
        );
        assert!(
            !review.message.contains(line),
            "approval must not repeat the plan the model already sent: {line}"
        );
    }
}

fn approve(queue: bool, clear: bool, no_color: bool) {
    let env = TestEnv::new("plan-markdown");
    let mut sess = start(&env, queue, no_color);
    submit_plan(&env, &mut sess, queue);
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
    // The pinned native terminal in pty_review_scrollback verifies styles
    // through resize and history movement in color and NoColor. This PTY's
    // screen-only parser has no scrollback to inspect after that movement.
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
    let text = format!("{:?}", persisted.messages());
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
    submit_plan(&env, &mut sess, true);
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
    submit_plan(&env, &mut sess, true);
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
    assert_styles(
        &sess,
        &[("Verify bold", true, false), ("src/main.rs", false, true)],
    );
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
    submit_plan(&env, &mut sess, true);
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
    assert!(!format!("{:?}", persisted.messages()).contains("The user APPROVED the plan"));
    assert!(persisted.compaction.is_none());
}
