//! PTY tests for the deferred tools mechanism (CC parity).
//!
//! Verifies the `ExecuteExtraTool` → deferred tool dispatch roundtrip:
//! the mock LLM emits an `ExecuteExtraTool` call with `tool_name: "CronList"`,
//! scode dispatches it, and the model sees the CronList result.
//!
//! The second test verifies that `ExecuteExtraTool` can dispatch to an MCP
//! tool: the mock LLM targets `mcp__parity__echo` through `ExecuteExtraTool`,
//! and the MCP echo response round-trips back.
//!
//! ```bash
//! cargo test --test pty_deferred_tools                          # mock (CI)
//! ```
mod common;

use std::fs;
use std::path::Path;

use common::TestEnv;

#[test]
fn execute_extra_tool_roundtrip() {
    let env = TestEnv::new("execute-extra-tool");
    if env.is_live() {
        eprintln!("SKIP: deferred-tool dispatch is validated against the mock backend");
        return;
    }
    let prompt = env.prompt(
        "List all scheduled cron tasks using ExecuteExtraTool.",
        "execute_extra_tool_roundtrip",
    );

    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);

    sess.expect("roundtrip complete")
        .expect("should see roundtrip completion message");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "execute_extra_tool roundtrip should exit 0; got {exit}"
    );
}

/// Minimal NDJSON MCP server — same as `pty_mcp_tool` but reused here to
/// verify the unified dispatch path through `ExecuteExtraTool`.
const MCP_SERVER_SCRIPT: &str = r#"import json, sys

def read_message():
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    return json.loads(line.decode())

def send_message(message):
    payload = json.dumps(message).encode()
    sys.stdout.buffer.write(payload + b'\n')
    sys.stdout.buffer.flush()

while True:
    request = read_message()
    if request is None:
        break
    method = request.get('method')
    if method == 'initialize':
        send_message({
            'jsonrpc': '2.0',
            'id': request['id'],
            'result': {
                'protocolVersion': request['params']['protocolVersion'],
                'capabilities': {'tools': {}},
                'serverInfo': {'name': 'parity-mcp', 'version': '0.1.0'},
            },
        })
    elif method == 'tools/list':
        send_message({
            'jsonrpc': '2.0',
            'id': request['id'],
            'result': {
                'tools': [
                    {
                        'name': 'echo',
                        'description': 'Echoes the provided text back as echo:<text>',
                        'inputSchema': {
                            'type': 'object',
                            'properties': {'text': {'type': 'string'}},
                            'required': ['text'],
                        },
                    }
                ]
            },
        })
    elif method == 'tools/call':
        args = request['params'].get('arguments') or {}
        text = args.get('text', '')
        send_message({
            'jsonrpc': '2.0',
            'id': request['id'],
            'result': {
                'content': [{'type': 'text', 'text': f'echo:{text}'}],
                'isError': False,
            },
        })
    elif 'id' in request:
        send_message({
            'jsonrpc': '2.0',
            'id': request['id'],
            'error': {'code': -32601, 'message': f'unknown method: {method}'},
        })
"#;

fn configure_mcp_server(workspace_root: &Path) {
    let script_path = workspace_root.join("parity-mcp-server.py");
    fs::write(&script_path, MCP_SERVER_SCRIPT).expect("mcp server script should write");

    let settings_dir = workspace_root.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings_dir).expect("project config dir should be created");
    let settings = serde_json::json!({
        "mcpServers": {
            "parity": {
                "command": common::resolve_python(),
                "args": [script_path.display().to_string()],
            }
        }
    });
    fs::write(
        settings_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("settings json"),
    )
    .expect("project settings.json should write");
}

/// ExecuteExtraTool dispatches to an MCP tool through the unified path:
/// model emits `ExecuteExtraTool { tool_name: "mcp__parity__echo", ... }`,
/// the `CliToolExecutor` intercept routes to `execute_runtime_tool`, and
/// the MCP echo response round-trips back.
#[test]
fn execute_extra_tool_dispatches_mcp_tool() {
    let env = TestEnv::new("execute-extra-tool-mcp");
    configure_mcp_server(env.workspace_root());

    let prompt = env.prompt(
        "Use ExecuteExtraTool to call the parity echo MCP tool with text 'hello from deferred mcp'.",
        "execute_extra_tool_mcp_roundtrip",
    );

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", &prompt],
        &[("SUDOCODE_ENABLE_MCP", "1")],
    );

    sess.expect("echo:hello from deferred mcp")
        .expect("MCP echo response should round-trip through ExecuteExtraTool");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "ExecuteExtraTool → MCP roundtrip should exit 0; got {exit}"
    );
}
