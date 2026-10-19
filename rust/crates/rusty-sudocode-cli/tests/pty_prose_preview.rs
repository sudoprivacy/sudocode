//! Inline prose, ordered history handoff, bounded source and input preservation.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use std::fmt::Write as _;
use std::time::Duration;

fn start(env: &TestEnv, cols: u16) -> PtySession {
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", "15;0"),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    session.resize(40, cols).unwrap();
    common::expect_screen(
        &session,
        |s| s.contains('❯'),
        Duration::from_mins(1),
        "prompt",
    );
    session
}

fn gated_prompt(env: &TestEnv, document: &str) -> String {
    common::terminal_host::prose_prompt(env, document, serde_json::json!({}))
}

fn finish(session: &mut PtySession) {
    session.send("\x15/exit").unwrap();
    common::expect_input_line(session, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

fn body_rows(session: &PtySession) -> Vec<(usize, String)> {
    session.render(|screen| {
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let start = rows
            .iter()
            .position(|row| row.contains("ProseStart"))
            .unwrap();
        let end = (start..rows.len())
            .find(|&end| {
                rows[start..=end]
                    .concat()
                    .split_whitespace()
                    .collect::<String>()
                    .contains("ProseDone.")
            })
            .unwrap();
        rows.into_iter()
            .enumerate()
            .skip(start)
            .take(end - start + 1)
            .collect()
    })
}

fn expect_finalized_with_draft(session: &PtySession, marker: &str, draft: &str) {
    common::expect_screen_settled(
        session,
        |screen| {
            let status = common::turn_status_line(screen);
            !status.is_empty() && status != marker && common::input_line_of(screen) == draft
        },
        common::DEFAULT_TIMEOUT,
        "finalized turn preserves its draft",
    );
}

#[test]
fn handoff_preserves_body_rows_and_draft_at_wide_and_narrow_widths() {
    for cols in [120, 48] {
        let env = TestEnv::new_mock("prose-handoff");
        let mut session = start(&env, cols);
        let marker = common::turn_status_marker(&session);
        session
            .send(&format!(
                "{}\r",
                gated_prompt(&env, mock_anthropic_service::PROSE_PREVIEW_DOC)
            ))
            .unwrap();
        common::expect_screen(
            &session,
            |s| {
                s.split_whitespace()
                    .collect::<String>()
                    .contains("ProseDone.")
                    && env.workspace_root().join("prose-ready").exists()
            },
            common::DEFAULT_TIMEOUT,
            "complete source visible before EOF",
        );
        session.send("draft界👩🏽‍💻").unwrap();
        common::expect_input_line(
            &session,
            "draft界👩🏽‍💻",
            common::DEFAULT_TIMEOUT,
            "draft during stream",
        );
        let before = body_rows(&session);
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        expect_finalized_with_draft(&session, &marker, "draft界👩🏽‍💻");
        let after = body_rows(&session);
        assert_eq!(
            before, after,
            "history handoff must retain physical body rows at {cols} columns"
        );
        session.render(|screen| assert_eq!(screen.contents().matches("ProseStart").count(), 1));
        common::expect_input_line(
            &session,
            "draft界👩🏽‍💻",
            common::DEFAULT_TIMEOUT,
            "draft survives handoff",
        );
        finish(&mut session);
    }
}

#[test]
fn preview_continues_after_a_stable_paragraph() {
    let env = TestEnv::new_mock("prose-after-block");
    let mut session = start(&env, 120);
    let marker = common::turn_status_marker(&session);
    let document = format!(
        "Stable paragraph.\n\n{}",
        mock_anthropic_service::PROSE_PREVIEW_DOC
    );
    session
        .send(&format!("{}\r", gated_prompt(&env, &document)))
        .unwrap();
    common::expect_screen(
        &session,
        |s| {
            s.contains("Stable paragraph.")
                && s.contains("ProseDone.")
                && env.workspace_root().join("prose-ready").exists()
        },
        common::DEFAULT_TIMEOUT,
        "pending prose after a stable block remains live",
    );
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    common::expect_turn_complete_after(
        &session,
        &marker,
        common::DEFAULT_TIMEOUT,
        "second paragraph flush",
    );
    session.render(|screen| assert_eq!(screen.contents().matches("ProseStart").count(), 1));
    finish(&mut session);
}

#[test]
fn ambiguous_emoji_width_uses_complete_block_fallback() {
    let env = TestEnv::new_mock("prose-emoji-fallback");
    let mut session = start(&env, 120);
    let marker = common::turn_status_marker(&session);
    let document = "WidthStart 👩🏽‍💻 ordinary words WidthDone";
    session
        .send(&format!("{}\r", gated_prompt(&env, document)))
        .unwrap();
    let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
    while !env.workspace_root().join("prose-ready").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(160));
    session.render(|screen| assert!(!screen.contents().contains("WidthDone")));
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    common::expect_turn_complete_after(
        &session,
        &marker,
        common::DEFAULT_TIMEOUT,
        "emoji canonical flush",
    );
    session.render(|screen| {
        assert_eq!(screen.contents().matches("WidthStart").count(), 1);
        assert!(screen.contents().contains("👩🏽‍💻 ordinary words WidthDone"));
    });
    finish(&mut session);
}

#[test]
fn cancel_commits_visible_partial_prose_once() {
    let env = TestEnv::new_mock("prose-cancel");
    let mut session = start(&env, 120);
    session
        .send(&format!(
            "{}\r",
            gated_prompt(&env, mock_anthropic_service::PROSE_PREVIEW_DOC)
        ))
        .unwrap();
    common::expect_screen(
        &session,
        |s| {
            s.split_whitespace()
                .collect::<String>()
                .contains("ProseDone.")
        },
        common::DEFAULT_TIMEOUT,
        "visible partial response",
    );
    session.send("keep-draft").unwrap();
    common::expect_input_line(&session, "keep-draft", common::DEFAULT_TIMEOUT, "draft");
    session.send("\x1b").unwrap();
    common::expect_screen_settled(
        &session,
        |s| {
            s.split_whitespace()
                .collect::<String>()
                .contains("ProseDone.")
                && s.to_lowercase().contains("cancelled")
        },
        common::DEFAULT_TIMEOUT,
        "cancel commits visible prose",
    );
    session.render(|screen| assert_eq!(screen.contents().matches("ProseStart").count(), 1));
    // Escape intentionally clears the draft in the existing input policy.
    common::expect_input_line_cleared(
        &session,
        common::DEFAULT_TIMEOUT,
        "input ready after cancel",
    );
    session.send("after-cancel").unwrap();
    common::expect_input_line(
        &session,
        "after-cancel",
        common::DEFAULT_TIMEOUT,
        "input usable after cancel",
    );
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    finish(&mut session);
}

#[test]
fn tiny_resize_keeps_input_and_complete_final_text() {
    let env = TestEnv::new_mock("prose-resize");
    let mut session = start(&env, 120);
    let marker = common::turn_status_marker(&session);
    session
        .send(&format!(
            "{}\r",
            gated_prompt(&env, mock_anthropic_service::PROSE_PREVIEW_DOC)
        ))
        .unwrap();
    common::expect_screen(
        &session,
        |s| {
            s.split_whitespace()
                .collect::<String>()
                .contains("ProseDone.")
        },
        common::DEFAULT_TIMEOUT,
        "preview",
    );
    session.send("resize-draft").unwrap();
    common::expect_input_line(
        &session,
        "resize-draft",
        common::DEFAULT_TIMEOUT,
        "draft before resize",
    );
    for (rows, cols) in [(40, 60), (12, 40), (40, 120)] {
        session.resize(rows, cols).unwrap();
        common::expect_input_line(
            &session,
            "resize-draft",
            common::DEFAULT_TIMEOUT,
            "input after resize",
        );
    }
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    expect_finalized_with_draft(&session, &marker, "resize-draft");
    common::expect_screen(
        &session,
        |s| {
            s.split_whitespace()
                .collect::<String>()
                .contains("ProseDone.")
        },
        common::DEFAULT_TIMEOUT,
        "complete final text",
    );
    finish(&mut session);
}

#[test]
fn source_above_preview_limit_is_committed_completely() {
    let env = TestEnv::new_mock("prose-cap");
    let mut session = start(&env, 120);
    let marker = common::turn_status_marker(&session);
    let document = format!("CapStart {} CapDone", "ordinary prose words ".repeat(600));
    let prompt = gated_prompt(&env, &document);
    let path = env.workspace_root().join("prose-control.json");
    let mut control: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    control["chunk_chars"] = serde_json::json!(256);
    control["delay_ms"] = serde_json::json!(5);
    std::fs::write(path, serde_json::to_vec(&control).unwrap()).unwrap();
    session.send(&format!("{prompt}\r")).unwrap();
    let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
    while !env.workspace_root().join("prose-ready").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    session.send("bounded-draft").unwrap();
    common::expect_input_line(
        &session,
        "bounded-draft",
        common::DEFAULT_TIMEOUT,
        "input with capped preview",
    );
    session.render(|screen| assert!(!screen.contents().contains("CapDone")));
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    expect_finalized_with_draft(&session, &marker, "bounded-draft");
    common::expect_screen(
        &session,
        |s| s.contains("CapDone"),
        common::DEFAULT_TIMEOUT,
        "final tail above cap",
    );
    finish(&mut session);
    // Inspect the actual persisted assistant message, not the echoed prompt.
    let sessions = env.workspace_root().join(".scode/sessions");
    let mut is_complete = false;
    for entry in walk_session_files(&sessions) {
        if let Ok(saved) = runtime::Session::load_from_path(&entry) {
            is_complete |= saved.messages.iter().any(|message| {
                message.blocks.iter().any(|block|
                matches!(block, runtime::ContentBlock::Text { text } if text == &document))
            });
        }
    }
    assert!(
        is_complete,
        "persisted session must retain every byte above the preview cap"
    );
}

fn walk_session_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk_session_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn live_prose_is_visible_and_input_remains_usable() {
    let env = TestEnv::new("prose-live");
    let mut session = start(&env, 120);
    let marker = common::turn_status_marker(&session);
    let prompt = env.prompt("Reply with one ordinary paragraph of about 150 words beginning with LiveProseStart and ending with LiveProseDone. Do not use tools, lists, headings or code fences.", "prose_preview");
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(
        &session,
        &marker,
        common::LIVE_TURN_BUDGET,
        "real or mock prose turn",
    );
    let last = if env.is_live() {
        "LiveProseDone"
    } else {
        "ProseDone"
    };
    common::expect_screen(
        &session,
        |s| {
            s.lines().any(|line| {
                line.starts_with("  ") && line.contains(last)
                    || line.starts_with("• ") && line.contains(last)
            })
        },
        common::LIVE_TURN_BUDGET,
        "complete body",
    );
    session.send("still-usable").unwrap();
    common::expect_input_line(
        &session,
        "still-usable",
        common::DEFAULT_TIMEOUT,
        "input after real prose turn",
    );
    finish(&mut session);
}

#[test]
#[ignore = "requires the pinned real terminal host; run explicitly for rendering acceptance"]
fn real_terminal_handoff_and_resize_preserve_native_history() {
    for (scenario, cols) in [
        ("prose_wide", 120),
        ("prose_narrow", 48),
        ("prose_resize", 120),
    ] {
        let env = TestEnv::new_mock(scenario);
        let prompt = gated_prompt(&env, mock_anthropic_service::PROSE_PREVIEW_DOC);
        let mut args = vec![
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--permission-mode",
            "read-only",
        ];
        let saved_path = env.workspace_root().join("saved.jsonl");
        if scenario == "prose_resize" {
            let mut saved = runtime::Session::new().with_workspace_root(env.workspace_root());
            saved.push_user_text("Saved conversation").unwrap();
            let mut body = String::new();
            for i in 0..70 {
                writeln!(body, "Earlier history line {i}").unwrap();
            }
            let mut message =
                runtime::ConversationMessage::assistant(vec![runtime::ContentBlock::Text {
                    text: body,
                }]);
            message.usage = Some(runtime::TokenUsage {
                output_tokens: 900,
                ..Default::default()
            });
            message.duration_ms = Some(1_000);
            saved.push_message(message).unwrap();
            saved.save_to_path(&saved_path).unwrap();
            args.extend(["--resume", saved_path.to_str().unwrap()]);
        }
        let output = common::terminal_host::run(
            &env,
            &args,
            scenario,
            serde_json::json!({
                "cols": cols, "rows": 40, "prompt": prompt,
            }),
        );
        common::terminal_host::assert_success(&output);
    }
}
