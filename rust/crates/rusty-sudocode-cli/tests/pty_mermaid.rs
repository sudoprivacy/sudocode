//! Actual streamed rendering, input ownership and replay in both terminal UIs.
mod common;

use common::TestEnv;
use std::time::Duration;

const DOCUMENT: &str =
    "DiagramBefore.\n\n```mermaid\nflowchart TD\nA[请求] --> B[Reply]\n```\n\nDiagramDone.";

fn assert_diagram(screen: &str) {
    assert!(
        screen.contains("请求") && screen.contains("Reply") && screen.contains('─'),
        "{screen}"
    );
    assert!(
        !screen.contains("flowchart TD") && !screen.contains("source shown"),
        "{screen}"
    );
}

fn assert_model_wire_and_original_source(env: &TestEnv, prompt: &str) {
    let requests = env.captured_message_bodies();
    let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
    let instructions = request["system"].to_string();
    let received_full_prompt = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter_map(|block| block["text"].as_str())
        .any(|text| text.contains(prompt));
    assert!(
        received_full_prompt,
        "the exact complete prompt must reach the model"
    );
    assert!(
        instructions.contains("# Diagrams") && instructions.contains("fenced `mermaid` block"),
        "diagram guidance must reach the actual model request: {instructions}"
    );
    let transcript = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        .expect("persisted transcript");
    let saved = runtime::Session::load_from_path(&transcript).unwrap();
    let unchanged = saved.messages().iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, runtime::ContentBlock::Text { text } if text == DOCUMENT))
    });
    assert!(
        unchanged,
        "original Mermaid Markdown must survive display unchanged"
    );
}

#[test]
fn mermaid_stream_and_replay_preserve_input_and_original_markdown() {
    for mode in ["off", "queue"] {
        eprintln!("Mermaid native fixture mode={mode}");
        let env = TestEnv::new_mock("mermaid-stream-replay");
        let prompt = common::terminal_host::prose_prompt(&env, DOCUMENT, serde_json::json!({}));
        let mut session = env.spawn_with_env(
            &["--permission-mode", "read-only"],
            &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode), ("NO_COLOR", "1")],
        );
        session.resize(60, 100).unwrap();
        common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
        let initial_status = common::turn_status_marker(&session);
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
            "complete prompt across wrapped input rows",
        );
        session.send("\r").unwrap();
        // The gated turn intentionally keeps the status spinner alive.
        let screen = common::expect_screen(
            &session,
            |s| {
                s.contains("请求")
                    && s.contains("Reply")
                    && env.workspace_root().join("prose-ready").exists()
            },
            common::DEFAULT_TIMEOUT,
            "streamed Mermaid before EOF",
        );
        assert_diagram(&screen);
        if mode == "queue" {
            session.send("draftkeep").unwrap();
            common::expect_input_line(
                &session,
                "draftkeep",
                common::DEFAULT_TIMEOUT,
                "draft during stream",
            );
        }
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        if mode == "off" {
            common::expect_turn_complete_after(
                &session,
                &initial_status,
                common::DEFAULT_TIMEOUT,
                "classic turn completed",
            );
            session.send("draftkeep").unwrap();
        }
        common::expect_screen_settled(
            &session,
            |s| {
                let status = common::turn_status_identity(s);
                !status.is_empty()
                    && status != initial_status
                    && s.contains("DiagramDone.")
                    && common::input_line_of(s) == "draftkeep"
            },
            common::DEFAULT_TIMEOUT,
            "draft after handoff",
        );
        session.send("\x15/exit").unwrap();
        common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "exit input");
        session.send("\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
        assert_model_wire_and_original_source(&env, &prompt);

        let mut replay = env.spawn_with_env(
            &["--resume", "latest", "--permission-mode", "read-only"],
            &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode), ("NO_COLOR", "1")],
        );
        replay.resize(60, 100).unwrap();
        let screen = common::expect_screen_settled(
            &replay,
            |s| s.contains("DiagramDone."),
            common::DEFAULT_TIMEOUT,
            "replayed Mermaid",
        );
        assert_diagram(&screen);
        common::expect_input_line_cleared(&replay, common::DEFAULT_TIMEOUT, "replay ready");
        replay.send("/exit").unwrap();
        common::expect_input_line(&replay, "/exit", common::DEFAULT_TIMEOUT, "replay exit");
        replay.send("\r").unwrap();
        assert_eq!(replay.expect_eof().unwrap(), 0);
    }
}

/// Every advertised family, the fallbacks and the fence edge cases, streamed
/// one character at a time so each split point reaches the real renderer.
const EDGE_DOCUMENT: &str = concat!(
    "EdgeBefore.\n\n",
    "```mermaid title=example\nsequenceDiagram\nA->>B: request\n```\n\n",
    "```mermaid\nstateDiagram-v2\n[*] --> Ready\n```\n\n",
    "```mermaid\nclassDiagram\nclass Order\nOrder --> Item\n```\n\n",
    "```mermaid\nerDiagram\nORDER ||--o{ ITEM : contains\n```\n\n",
    "> ~~~~mermaid\n> flowchart TD\n> Q1[引用] --> Q2[Quoted]\n> ~~~~~\n\n",
    "- Listed:\n\n  ```mermaid\n  flowchart TD\n\n  L1[列表] --> L2[Listed2]\n  ```\n\n",
    "```mermaid\npie\n\"Cats\": 2\n```\n\n",
    "```mermaid\ngraph TD\nU1[\u{1b}[2J] --> U2[Safe]\n```\n\n",
    "EdgeAfter.\n\n",
    "```mermaid\nflowchart LR\nZ1 --> Z2\n```junk",
);

#[test]
fn mermaid_families_fallbacks_and_fence_edges_render_while_streaming() {
    for mode in ["off", "queue"] {
        eprintln!("Mermaid edge fixture mode={mode}");
        let env = TestEnv::new_mock("mermaid-edges");
        let prompt = common::terminal_host::prose_prompt(
            &env,
            EDGE_DOCUMENT,
            serde_json::json!({"chunk_chars": 1, "delay_ms": 2}),
        );
        let mut session = env.spawn_with_env(
            &["--permission-mode", "read-only"],
            &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode), ("NO_COLOR", "1")],
        );
        session.resize(150, 100).unwrap();
        common::expect_input_line_cleared(&session, Duration::from_secs(30), "initial prompt");
        let initial_status = common::turn_status_marker(&session);
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
            "complete prompt across wrapped input rows",
        );
        session.send("\r").unwrap();
        common::expect_screen(
            &session,
            |_| env.workspace_root().join("prose-ready").exists(),
            common::DEFAULT_TIMEOUT,
            "whole document streamed",
        );
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        let screen = common::expect_screen_settled(
            &session,
            |s| {
                let status = common::turn_status_identity(s);
                !status.is_empty() && status != initial_status && s.contains("EdgeAfter.")
            },
            common::DEFAULT_TIMEOUT,
            "turn completed",
        );

        for label in [
            "request", "Ready", "Order", "Item", "ORDER", "ITEM", "引用", "Quoted", "列表",
            "Listed2",
        ] {
            assert!(screen.contains(label), "missing {label}:\n{screen}");
        }
        for source in [
            "sequenceDiagram",
            "stateDiagram-v2",
            "classDiagram",
            "erDiagram",
            "flowchart TD",
        ] {
            assert!(
                !screen.contains(source),
                "completed fence left source {source}:\n{screen}"
            );
        }
        assert!(
            screen
                .lines()
                .any(|line| line.contains("引用") && line.trim_start().starts_with('>')),
            "a quoted diagram keeps its quote prefix:\n{screen}"
        );
        // Unsupported and unsafe diagrams display escaped source with a notice;
        // a raw escape would have cleared the screen above it.
        assert_eq!(
            screen.matches("Mermaid source shown").count(),
            2,
            "{screen}"
        );
        assert!(screen.contains("\"Cats\": 2"), "{screen}");
        assert!(
            screen.contains("graph TD") && screen.contains("\\u{1b}[2J"),
            "{screen}"
        );
        assert!(screen.contains("EdgeBefore."), "{screen}");
        // "```junk" is not a closing fence, so the last fence never completes.
        assert!(
            screen.contains("flowchart LR") && screen.contains("```junk"),
            "{screen}"
        );

        session.send("/exit").unwrap();
        common::expect_input_line(&session, "/exit", common::DEFAULT_TIMEOUT, "exit input");
        session.send("\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
    }
}
