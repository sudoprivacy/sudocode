//! Open fences use incremental syntax state and finalize once into native history.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::json;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

const DOCUMENT: &str = "```rust\nfn main() {\n    let message = \"first line\nStreamString中文\nlast line\";\n    /* first comment\nStreamComment\n    */\n}\nPartialCodeTail";

fn color(session: &PtySession, needle: &str) -> (String, String) {
    session.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        let row = rows.iter().rposition(|row| row.contains(needle)).unwrap();
        let col = rows[row][..rows[row].find(needle).unwrap()].width();
        let cell = raw
            .cell(row.try_into().unwrap(), col.try_into().unwrap())
            .unwrap();
        (
            format!("{:?}", cell.fgcolor()),
            format!("{:?}", cell.bgcolor()),
        )
    })
}

#[test]
fn multiline_code_stream_matches_final_colors_and_keeps_draft() {
    for no_color in [false, true] {
        let env = TestEnv::new_mock("incremental-code");
        let mut session = env.spawn_with_env(
            &[],
            &[
                ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                ("COLORFGBG", "15;0"),
                ("TERM", "xterm-256color"),
                ("COLORTERM", "truecolor"),
                ("NO_COLOR", if no_color { "1" } else { "" }),
            ],
        );
        session.resize(70, 100).unwrap();
        common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "code prompt");
        let marker = common::turn_status_marker(&session);
        let prompt = common::terminal_host::prose_prompt(
            &env,
            DOCUMENT,
            json!({"chunk_chars":7,"delay_ms":10}),
        );
        session.send(&format!("{prompt}\r")).unwrap();
        let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
        while !env.workspace_root().join("prose-ready").exists() {
            assert!(Instant::now() < deadline, "provider completed code deltas");
            std::thread::sleep(Duration::from_millis(10));
        }
        common::expect_screen(
            &session,
            |screen| {
                screen.contains("StreamString中文")
                    && screen.contains("StreamComment")
                    && screen.contains("PartialCodeTail")
            },
            common::DEFAULT_TIMEOUT,
            "code is visible before fence/message close",
        );
        let string = color(&session, "StreamString");
        let comment = color(&session, "StreamComment");
        assert_eq!(string.1, "Default");
        assert_eq!(comment.1, "Default");
        if no_color {
            assert_eq!(string.0, "Default");
            assert_eq!(comment.0, "Default");
        } else {
            assert_ne!(
                string.0, "Default",
                "open multiline string must already be highlighted"
            );
            assert_ne!(
                comment.0, "Default",
                "parser state continues inside the open comment"
            );
            assert_ne!(string.0, comment.0);
        }
        session.send("code-draft界").unwrap();
        let draft = String::from("code-draft界");
        common::expect_input_line(
            &session,
            &draft,
            common::DEFAULT_TIMEOUT,
            "typing during code preview",
        );
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        common::expect_turn_complete_with_input(
            &session,
            &marker,
            &draft,
            common::DEFAULT_TIMEOUT,
            "open fence canonical flush",
        );
        common::expect_input_line(
            &session,
            &draft,
            common::DEFAULT_TIMEOUT,
            "code handoff preserves draft",
        );
        assert_eq!(color(&session, "StreamString"), string);
        assert_eq!(color(&session, "StreamComment"), comment);
        session.render(|screen| {
            let text = screen.contents();
            assert_eq!(
                text.matches("StreamString").count(),
                1,
                "one finalized code block: {text}"
            );
            assert_eq!(text.matches("PartialCodeTail").count(), 1);
            assert!(!text.contains("```rust"));
        });
        session.send("\x15/exit").unwrap();
        common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "code exit");
        session.send("\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
    }
}

#[test]
fn specialized_fences_wait_for_the_canonical_renderer() {
    let document = "SpecialFenceBefore.\n\n```mermaid title=preview\nflowchart TD\nA[PartialDiagramNode] --> B[回复]\n";
    for no_color in [false, true] {
        let env = TestEnv::new_mock("specialized-fence-preview");
        let mut session = env.spawn_with_env(
            &[],
            &[
                ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                ("TERM", "xterm-256color"),
                ("COLORTERM", "truecolor"),
                ("NO_COLOR", if no_color { "1" } else { "" }),
            ],
        );
        session.resize(70, 100).unwrap();
        common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "diagram prompt");
        let marker = common::turn_status_marker(&session);
        let prompt = common::terminal_host::prose_prompt(
            &env,
            document,
            json!({"chunk_chars":1,"delay_ms":2}),
        );
        session.send(&prompt).unwrap();
        let complete_prompt = prompt.split_whitespace().collect::<String>();
        common::expect_screen(
            &session,
            |screen| {
                screen
                    .split_whitespace()
                    .collect::<String>()
                    .contains(&complete_prompt)
            },
            common::DEFAULT_TIMEOUT,
            "complete diagram prompt",
        );
        session.send("\r").unwrap();
        // The footer acknowledges consumption in the real UI; the provider's
        // ready file alone would race the pending deltas and painting thread.
        let counter = format!("↓ {} tokens", document.len() / 4);
        let screen = common::expect_screen(
            &session,
            |screen| screen.contains(&counter) && screen.contains("SpecialFenceBefore."),
            common::DEFAULT_TIMEOUT,
            "open specialized fence consumed",
        );
        assert!(
            !screen.contains("flowchart TD") && !screen.contains("PartialDiagramNode"),
            "specialized fences must retain canonical presentation while open: {screen}"
        );
        session.send("diagram-draft").unwrap();
        common::expect_input_line(
            &session,
            "diagram-draft",
            common::DEFAULT_TIMEOUT,
            "draft while diagram waits",
        );
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        common::expect_turn_complete_with_input(
            &session,
            &marker,
            "diagram-draft",
            common::DEFAULT_TIMEOUT,
            "unfinished diagram canonical fallback",
        );
        session.render(|screen| {
            let text = screen.contents();
            assert_eq!(text.matches("flowchart TD").count(), 1, "{text}");
            assert_eq!(text.matches("PartialDiagramNode").count(), 1, "{text}");
        });
        session.send("\x15/exit").unwrap();
        common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "diagram exit");
        session.send("\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
    }
}

#[test]
#[ignore = "requires the pinned real terminal host; run explicitly for rendering acceptance"]
fn real_terminal_open_code_resize_and_handoff_do_not_duplicate_history() {
    for scenario in ["code_resize", "code_queued_resize"] {
        let env = TestEnv::new_mock(scenario);
        let prompt = common::terminal_host::prose_prompt(
            &env,
            DOCUMENT,
            json!({"chunk_chars":7,"delay_ms":10}),
        );
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/terminal/prose.cjs");
        let output = common::terminal_host::run_script(
            &env,
            &[
                "--auth",
                "api-key",
                "--model",
                "sonnet",
                "--permission-mode",
                "read-only",
            ],
            scenario,
            json!({"cols":120,"rows":40,"prompt":prompt,"draft":"code-draft界👩🏽‍💻"}),
            &script,
        );
        common::terminal_host::assert_success(&output);
    }
}
