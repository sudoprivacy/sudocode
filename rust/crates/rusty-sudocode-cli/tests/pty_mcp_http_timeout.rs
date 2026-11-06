//! Real PTY, authenticated HTTP MCP, and a delayed tool result.

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::TestEnv;
use serde_json::{json, Value};

struct HttpFixture(Child);

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn configure_remote(env: &TestEnv) -> HttpFixture {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http_mcp.py");
    let mut child = HttpFixture(
        Command::new(common::resolve_python())
            .arg("-u")
            .arg(script)
            .arg(env.workspace_root().join("remote-receipts.jsonl"))
            .args(["pty-http-budget", "0.75"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start owned HTTP MCP fixture"),
    );
    let stdout = child.0.stdout.take().expect("MCP readiness pipe");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut url = String::new();
        let result = BufReader::new(stdout).read_line(&mut url).map(|_| url);
        let _ = sender.send(result);
    });
    let url = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("HTTP MCP readiness deadline")
        .expect("HTTP MCP readiness");
    assert!(url.starts_with("http://127.0.0.1:"));
    let settings = env.workspace_root().join(".nexus/sudocode/settings.json");
    fs::create_dir_all(settings.parent().expect("settings parent")).expect("settings directory");
    fs::write(
        settings,
        json!({"mcpServers":{"parity":{
            "type":"http", "url":url.trim(), "toolCallTimeoutMs":3_000,
            "headers":{"Authorization":"Bearer pty-http-budget"}
        }}})
        .to_string(),
    )
    .expect("write scoped MCP settings");
    child
}

#[test]
fn remote_mcp_deadline_preserves_real_tool_result_in_pty() {
    let env = TestEnv::new("http-mcp-budget");
    let _remote = configure_remote(&env);
    let prompt = env.prompt(
        "Use the parity echo MCP tool with the exact text 'hello from mcp parity'. Report its actual returned text.",
        "mcp_tool_roundtrip",
    );
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", &prompt],
        &[("SUDOCODE_ENABLE_MCP", "1")],
    );
    if env.is_live() {
        session.set_default_timeout(Duration::from_secs(180));
    }
    session
        .expect("echo:hello from mcp parity")
        .expect("actual delayed HTTP tool result");
    assert_eq!(session.expect_eof().expect("normal CLI exit"), 0);
    let calls: Vec<Value> = fs::read_to_string(env.workspace_root().join("remote-receipts.jsonl"))
        .expect("remote receipts")
        .lines()
        .map(|line| serde_json::from_str(line).expect("receipt JSON"))
        .collect();
    let invoked: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "tools/call")
        .collect();
    assert!(
        !invoked.is_empty(),
        "the model must actually invoke the tool"
    );
    assert!(invoked
        .iter()
        .all(|call| call["params"]["arguments"]["text"] == "hello from mcp parity"));
}
