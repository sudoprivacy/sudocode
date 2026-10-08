//! Shared launcher for the pinned node-pty/xterm observer.
use super::TestEnv;
use serde_json::{json, Value};
use std::{path::PathBuf, process::Output};

pub fn run(env: &TestEnv, args: &[&str], scenario: &str, extra: Value) -> Output {
    let log_root = std::env::var_os("SCODE_TERMINAL_LOG_DIR")
        .map_or_else(|| env.workspace_root().join("terminal-logs"), PathBuf::from)
        .join(scenario);
    std::fs::create_dir_all(&log_root).unwrap();
    let mut config = json!({
        "binary": super::scode_bin(), "args": args, "scenario": scenario,
        "root": env.workspace_root(), "configHome": env.config_home(),
        "todos": env.workspace_root().join("todos.json"), "logRoot": log_root,
        "backend": std::env::var("SCODE_CONPTY_BACKEND").unwrap_or_else(|_| "bundled".into()),
    });
    let Value::Object(fields) = extra else {
        panic!("terminal config must be an object");
    };
    config.as_object_mut().unwrap().extend(fields);
    let manifest = env.workspace_root().join("terminal.json");
    std::fs::write(&manifest, serde_json::to_vec(&config).unwrap()).unwrap();
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../e2e/terminal-resize/run.cjs");
    std::process::Command::new(
        std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into()),
    )
    .arg(script)
    .arg(manifest)
    .env("ELECTRON_RUN_AS_NODE", "1")
    .output()
    .expect("start real terminal host; see e2e/terminal-resize/README.md")
}

pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "real terminal failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("{}", String::from_utf8_lossy(&output.stdout));
}

pub fn prose_prompt(env: &TestEnv, document: &str, extra: Value) -> String {
    let root = env.workspace_root();
    let path = root.join("prose-control.json");
    let mut control = json!({
        "document": document, "ready": root.join("prose-ready"),
        "release": root.join("prose-release"), "chunk_chars": 12, "delay_ms": 20,
    });
    let Value::Object(fields) = extra else {
        panic!("prose control must be an object");
    };
    control.as_object_mut().unwrap().extend(fields);
    std::fs::write(&path, serde_json::to_vec(&control).unwrap()).unwrap();
    env.prompt(
        &format!("PROSE_CONTROL:{}", json!({"path": path})),
        "prose_preview",
    )
}
