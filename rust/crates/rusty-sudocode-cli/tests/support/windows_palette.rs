//! Palette-query capability is observed at the real ConPTY/xterm boundary.
use crate::common::{self, TestEnv};
use serde_json::json;

#[test]
#[ignore = "requires the pinned real terminal host; CI runs this target explicitly"]
fn windows_startup_does_not_solicit_color_replies_into_the_input_draft() {
    for backend in ["bundled", "system"] {
        for background in ["15;0", "0;15", ""] {
            for reply_timing in ["startup", "late"] {
                let env = TestEnv::new("windows-palette");
                let log_root = std::env::var_os("SCODE_TERMINAL_LOG_DIR")
                    .map_or_else(|| env.workspace_root().join("terminal-logs"), Into::into)
                    .join(format!(
                        "palette-{backend}-{reply_timing}-{}",
                        background.replace(';', "-")
                    ));
                let manifest = env.workspace_root().join("palette-terminal.json");
                std::fs::write(
                    &manifest,
                    serde_json::to_vec(&json!({
                        "binary": common::scode_bin(),
                        "args": ["--permission-mode", "read-only"],
                        "root": env.workspace_root(), "configHome": env.config_home(),
                        "todos": env.workspace_root().join("todos.json"), "logRoot": log_root,
                        "backend": backend, "env": {"COLORFGBG": background},
                        "replyTiming": reply_timing,
                        "expectedMuted": if background == "0;15" {241} else {247},
                    }))
                    .unwrap(),
                )
                .unwrap();
                let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/windows_palette.cjs");
                let output = std::process::Command::new(
                    std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into()),
                )
                .arg(script)
                .arg(manifest)
                .env("ELECTRON_RUN_AS_NODE", "1")
                .output()
                .expect("start real palette terminal host");
                common::terminal_host::assert_success(&output);
                if env.is_mock() {
                    assert_eq!(
                        env.captured_message_count(),
                        0,
                        "startup must not run a turn"
                    );
                }
            }
        }
    }
}
