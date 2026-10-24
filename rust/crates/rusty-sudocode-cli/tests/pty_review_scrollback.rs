//! Full documents use native history independently of the live question controls.
mod common;

use common::TestEnv;
use serde_json::json;
use std::process::Command;

#[test]
#[ignore = "requires the pinned real terminal host; explicitly run in rendering acceptance"]
fn real_terminal_long_reviews_preserve_scrollback_and_reading_position() {
    for kind in ["plan", "question"] {
        let env = TestEnv::new("review-scrollback");
        let saved_path = env.workspace_root().join("saved-review-session.jsonl");
        let mut saved = runtime::Session::new().with_workspace_root(env.workspace_root());
        saved.push_user_text("Earlier review work").unwrap();
        saved
            .push_message(runtime::ConversationMessage::assistant(vec![
                runtime::ContentBlock::Text {
                    text: (0..70)
                        .map(|i| format!("EarlierReviewHistory{i:03}\n"))
                        .collect(),
                },
            ]))
            .unwrap();
        saved.save_to_path(&saved_path).unwrap();
        let document = format!(
            "# Full review document\n\n{}",
            (0..80)
                .map(|i| format!(
                    "- **ReviewLine{i:03}**: preserve 中文 and exact e\u{301} source."
                ))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let mut call = if kind == "plan" {
            json!({"id":"review_plan", "name":"write_plan", "input":{
                "content":document, "context":"Read every line.",
                "constraints":"Keep the complete document.", "acceptance":"Every line is available."
            }})
        } else {
            json!({"id":"review_question", "name":"AskUserQuestion", "input":{
                "title":"Review questions", "description":document,
                "questions": [
                    {"id":"first", "prompt":"First review decision", "kind":"single_select",
                     "options":[{"label":"First choice", "value":"first"}]},
                    {"id":"second", "prompt":"Second review decision", "kind":"single_select",
                     "options":(1..=12).map(|i| json!({"label":format!("Choice {i}"), "value":i.to_string()})).collect::<Vec<_>>()}
                ]
            }})
        };
        let stream_gate = env.workspace_root().join("review-stream-release");
        call["stream_wait_for"] = json!(stream_gate);
        // The existing fixture gates after its first text delta. Release the
        // second delta only once the reader has moved into native history.
        call["stream_text_chunks"] = json!([
            "ReviewPrelude: review is ready.\n\n",
            "ReviewStreamPulse: output continues during review.\n\n"
        ]);
        let prompt = if env.is_mock() {
            format!(
                "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
                json!([call])
            )
        } else {
            format!("Exercise review with supplied tool input. Call {} with exactly this JSON input: {}. Wait for the user's decision. If a plan is rejected, leave it saved without calling write_plan again. After review completes respond with exactly Review finished.",
                call["name"].as_str().unwrap(), call["input"])
        };
        let log_root = std::env::var_os("SCODE_TERMINAL_LOG_DIR")
            .map_or_else(
                || env.workspace_root().join("terminal-logs"),
                std::path::PathBuf::from,
            )
            .join(format!("review-{kind}"));
        let auth = if env.is_live() {
            std::env::var("SCODE_LIVE_AUTH_MODE").unwrap_or_else(|_| "proxy".into())
        } else {
            "api-key".into()
        };
        let model = if env.is_live() {
            common::live_model()
        } else {
            "sonnet".into()
        };
        let config = json!({
            "binary":common::scode_bin(),
            "args":["--auth",auth,"--model",model,"--permission-mode","workspace-write","--allowedTools","write_plan,AskUserQuestion","--resume",saved_path],
            "root":env.workspace_root(), "configHome":env.config_home(),
            "todos":env.workspace_root().join("todos.json"), "logRoot":log_root,
            "backend":std::env::var("SCODE_CONPTY_BACKEND").unwrap_or_else(|_| "bundled".into()),
            "cols":120, "rows":40, "prompt":prompt, "kind":kind,
            "expected":80, "isLive":env.is_live(),
            "env":{"SUDOCODE_PLAN_FILE":env.workspace_root().join("review-plan.md")}
        });
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/terminal/review.cjs");
        let output =
            Command::new(std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into()))
                .arg(script)
                .arg(config.to_string())
                .env("ELECTRON_RUN_AS_NODE", "1")
                .output()
                .expect("start the pinned real terminal host");
        common::terminal_host::assert_success(&output);
        if kind == "plan" {
            let saved =
                std::fs::read_to_string(env.workspace_root().join("review-plan.md")).unwrap();
            let mut transcripts = vec![saved_path];
            if let Some(path) =
                common::find_session_transcript(&env.workspace_root().join(".scode"))
            {
                transcripts.push(path);
            }
            let content = transcripts
                .iter()
                .filter_map(|path| runtime::Session::load_from_path(path).ok())
                .flat_map(runtime::Session::into_messages)
                .flat_map(|message| message.blocks)
                .find_map(|block| match block {
                    runtime::ContentBlock::ToolUse { name, input, .. } if name == "write_plan" => {
                        let input: serde_json::Value = serde_json::from_str(&input).unwrap();
                        input["content"].as_str().map(str::to_owned)
                    }
                    _ => None,
                })
                .expect("the actual provider's plan input is persisted");
            assert!(
                saved.contains(content.trim()),
                "review preserves the provider's complete original Markdown bytes"
            );
            if env.is_mock() {
                assert!(saved.contains(&document));
            }
        }
    }
}
