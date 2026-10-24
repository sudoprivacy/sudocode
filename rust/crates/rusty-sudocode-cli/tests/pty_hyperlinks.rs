//! The real CLI preserves semantic links through history, layout and questions.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::json;
use std::time::Duration;

fn document() -> String {
    let long = format!("https://example.com/{}", "long-path-".repeat(180));
    let mut text = String::from(
        "## LinkHeading\n\n[PR #891](https://github.com/sudoprivacy/sudocode/pull/891) PlainAfter\n\n\
         [**DragStart** 中文界 e\u{301} 👩🏽‍💻 wrapping words wrapping words wrapping words wrapping words wrapping words DragEnd](https://example.com/drag)\n\n\
         | Name | Destination |\n| --- | --- |\n| TableLink | [中文👩🏽‍💻](https://example.com/table) |\n\n\
         - [ListStart 中文界 👩🏽‍💻 wrapping words wrapping words wrapping words wrapping words wrapping words ListEnd](https://example.com/list)\n\n\
         [UnsafeLink](javascript:alert) [FileLink](file:///tmp/not-opened)\n\n",
    );
    text.push_str("[QueryLink](https://example.com/search?q=a;b#fragment)\n\n");
    text.push_str(&format!(
        "[SemicolonLink](https://example.com/{}tail)\n\n",
        "part;".repeat(32)
    ));
    text.push_str(&format!("[LongDestination]({long})\n\n"));
    text.push_str(&format!(
        "[OversizedDestination](https://example.com/{})\n\n",
        "x".repeat(8192)
    ));
    for i in 0..64 {
        text.push_str(&format!(
            "[DenseLink{i:02}](https://example.com/dense/{i}) "
        ));
    }
    text.push_str("\n\nLinkBodyEND.");
    text
}

fn seed(env: &TestEnv, text: String) -> std::path::PathBuf {
    let mut session = Session::new().with_workspace_root(env.workspace_root().to_path_buf());
    session.push_user_text("Review these links.").unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text,
        }]))
        .unwrap();
    let path = env.workspace_root().join("links.jsonl");
    // The release comparison also opens this fixture with main's older CLI.
    // Export the common snapshot format through the public session serializer.
    std::fs::write(&path, format!("{}\n", session.to_json().unwrap().render())).unwrap();
    path
}

#[test]
fn terminal_links_survive_wrapping_without_color() {
    let env = TestEnv::new_mock("terminal-links");
    let path = seed(&env, "[PR #891](https://github.com/sudoprivacy/sudocode/pull/891) PlainAfter\n\n[Wrapping中文👩🏽‍💻 wrapping words wrapping words wrapping words END](https://example.com/a;b)\n\nLinkBodyEND.".into());
    let mut session = env.spawn_with_env(
        &["--resume", path.to_str().unwrap()],
        &[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "vscode"),
            ("NO_COLOR", "1"),
            ("TMUX", ""),
            ("STY", ""),
        ],
    );
    let wire = session.expect("PlainAfter").unwrap();
    // ConPTY may assign an OSC 8 id while translating console cells. The
    // destination must survive; transport-owned parameters need not be empty.
    assert!(
        wire.split("\x1b]8;").skip(1).any(|sequence| {
            sequence.split_once(';').is_some_and(|(_, target)| {
                target.split(['\x1b', '\x07']).next()
                    == Some("https://github.com/sudoprivacy/sudocode/pull/891")
            })
        }),
        "missing native link in PTY output: {wire:?}"
    );
    common::expect_screen(
        &session,
        |s| s.contains("LinkBodyEND.") && s.contains('❯'),
        Duration::from_secs(30),
        "link history rendered",
    );
    session.render(|screen| {
        let text = screen.raw().contents();
        assert!(text.contains("PR #891 PlainAfter"), "{text}");
        assert!(
            !text.contains("https://"),
            "hidden target counted as text: {text}"
        );
    });
    session.send("/exit").unwrap();
    common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn unsupported_terminal_keeps_destinations_visible() {
    let env = TestEnv::new_mock("link-fallback");
    let path = seed(&env, "[PR #891](https://github.com/sudoprivacy/sudocode/pull/891) PlainAfter\n\n[Wrapping中文👩🏽‍💻 wrapping words wrapping words wrapping words END](https://example.com/a;b)\n\nLinkBodyEND.".into());
    let mut session = env.spawn_with_env(
        &["--resume", path.to_str().unwrap()],
        &[
            ("TERM", "xterm-256color"),
            ("TERM_PROGRAM", "unknown"),
            ("WT_SESSION", ""),
            ("VTE_VERSION", ""),
            ("KONSOLE_VERSION", ""),
            ("NO_COLOR", "1"),
        ],
    );
    session
        .expect("https://github.com/sudoprivacy/sudocode/pull/891")
        .unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("LinkBodyEND.") && s.contains('❯'),
        Duration::from_secs(30),
        "fallback rendered",
    );
    session.render(|screen| {
        let text = screen.raw().contents();
        assert!(
            text.contains("https://github.com/sudoprivacy/sudocode/pull/891"),
            "fallback destination: {text}"
        );
    });
    session.send("/exit").unwrap();
    common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
#[ignore = "browser terminal acceptance and release comparison; requires npm ci and Chromium"]
fn browser_terminal_links_and_interaction() {
    let env = TestEnv::new_mock("browser-links");
    let path = seed(&env, document());
    let prose = common::terminal_host::prose_prompt(
        &env,
        &document(),
        json!({
            "start_ready": env.workspace_root().join("stream-ready"),
            "start_release": env.workspace_root().join("stream-release"),
            "chunk_chars": 60, "delay_ms": 30,
        }),
    );
    let question = env.prompt(&format!("TOOL_BATCH:{}", json!([{
        "id":"links-question", "name":"AskUserQuestion", "input": {
            "title":"Links", "question":"Continue the review?",
            "description":"Review [QuestionLink](https://example.com/question) then choose.",
            "options":["Continue", "Stop"],
        },
    }])), "tool_concurrency");
    let output = common::terminal_host::run(
        &env,
        &[
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--resume",
            path.to_str().unwrap(),
            "--permission-mode",
            "danger-full-access",
        ],
        "browser-links",
        json!({
            "prompt":prose, "question":question, "cols":80, "rows":38,
            "report":std::env::var("SCODE_GUI_REPORT").ok(),
            "expectLinks":std::env::var("SCODE_GUI_EXPECT_LINKS").as_deref() != Ok("0"),
        }),
    );
    common::terminal_host::assert_success(&output);
}
