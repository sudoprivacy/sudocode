//! Resizing inline chrome must not commit old frames into terminal history.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session, TokenUsage};

#[test]
#[ignore = "requires the pinned real terminal host; CI runs this test explicitly"]
fn resize_keeps_one_status_and_todo_without_erasing_history_or_input() {
    let env = TestEnv::new("chrome-resize");
    let store = env.workspace_root().join("todos.json");
    std::fs::write(
        &store,
        r#"[{"content":"ResizeCompletedTask","activeForm":"Checking resize","status":"completed"}]"#,
    )
    .unwrap();
    let mut saved = Session::new().with_workspace_root(env.workspace_root());
    saved.push_user_text("Saved resize conversation").unwrap();
    let mut body = (0..70)
        .map(|i| format!("Earlier history line {i}\n"))
        .collect::<String>();
    body.push_str("ResizeHistorySentinel");
    let mut message = ConversationMessage::assistant(vec![ContentBlock::Text { text: body }]);
    message.usage = Some(TokenUsage {
        cache_read_input_tokens: 405_405,
        cache_creation_input_tokens: 4_095,
        output_tokens: 900,
        ..TokenUsage::default()
    });
    message.duration_ms = Some(61_000);
    saved.push_message(message).unwrap();
    let path = env.workspace_root().join("resize-session.jsonl");
    saved.save_to_path(&path).unwrap();
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
    let log_root = std::env::var_os("SCODE_TERMINAL_LOG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| env.workspace_root().join("terminal-logs"));
    std::fs::create_dir_all(&log_root).unwrap();
    let manifest = env.workspace_root().join("terminal.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({
            "binary": common::scode_bin(),
            "args": ["--auth", auth.as_str(), "--model", model.as_str(), "--resume",
                path.to_str().unwrap(), "--permission-mode", "read-only"],
            "root": env.workspace_root(),
            "configHome": env.config_home(),
            "todos": store,
            "logRoot": log_root,
            "backend": std::env::var("SCODE_CONPTY_BACKEND").unwrap_or_else(|_| "bundled".into()),
        }))
        .unwrap(),
    )
    .unwrap();
    let host = std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into());
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../e2e/terminal-resize/run.cjs");
    let output = std::process::Command::new(host)
        .arg(script)
        .arg(manifest)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .output()
        .expect("start real terminal host; see e2e/terminal-resize/README.md");
    assert!(
        output.status.success(),
        "real terminal resize failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            0,
            "resize must not run a turn"
        );
    }
}
