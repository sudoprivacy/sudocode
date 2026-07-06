//! PTY test for the MCP stdio tool-call path.
//!
//! ## Why this test exists
//!
//! Two production bugs converge on this single path and neither was
//! caught by any pre-existing test:
//!
//! 1. **NDJSON stdio framing.** scode's MCP stdio transport must speak
//!    newline-delimited JSON (the MCP spec framing), not LSP-style
//!    `Content-Length` headers. Getting this wrong means scode cannot
//!    talk to the vast majority of third-party MCP servers.
//! 2. **Nested-runtime `block_on`.** The tool loop reaches
//!    `RuntimeMcpState::call_tool` from inside the outer
//!    `runtime.block_on(run_turn)` context; a bare inner
//!    `runtime.block_on` there panics with "Cannot start a runtime from
//!    within a runtime".
//!
//! This test drives a real `scode` turn under a PTY that discovers a
//! real (subprocess) MCP server over NDJSON and calls its `echo` tool.
//! A regression in either the transport framing or the runtime bridge
//! makes the echoed payload fail to round-trip, so the assertion below
//! fails.
//!
//! `#![cfg(unix)]` for the same reason as the sibling PTY/ACP suites:
//! the mock MCP server is a POSIX subprocess (a `python3` script) and
//! the harness spawns scode under a POSIX PTY.
//!
//! ```bash
//! cargo test --test pty_mcp_tool                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_mcp_tool  # real API
//! ```
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;

use common::TestEnv;

/// Minimal NDJSON MCP server exposing a single `echo` tool. Speaks the
/// MCP spec's newline-delimited JSON framing: one JSON-RPC message per
/// line, `\n`-terminated, no `Content-Length` headers. scode's client
/// performs a request/response handshake (`initialize` → `tools/list` →
/// `tools/call`) and sends no notifications, so those three methods are
/// all that need handling.
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

/// Write the NDJSON MCP server script and a project-level
/// `.nexus/sudocode/settings.json` that registers it as the `parity`
/// stdio server. scode runs with its CWD set to the workspace root, so
/// this project config is discovered and its `mcpServers` merged. The
/// `echo` tool is therefore exposed to the model as `mcp__parity__echo`.
fn configure_mcp_server(workspace_root: &Path) {
    let script_path = workspace_root.join("parity-mcp-server.py");
    fs::write(&script_path, MCP_SERVER_SCRIPT).expect("mcp server script should write");

    let settings_dir = workspace_root.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings_dir).expect("project config dir should be created");
    let settings = serde_json::json!({
        "mcpServers": {
            "parity": {
                "command": "python3",
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

/// A real turn discovers the `parity` MCP server over NDJSON stdio,
/// calls `mcp__parity__echo`, and the echoed payload round-trips back
/// into the response — proving both the NDJSON framing and the
/// nested-runtime `call_tool` bridge work end-to-end.
#[test]
fn mcp_echo_tool_round_trips_through_ndjson_stdio() {
    let env = TestEnv::new("mcp-tool");
    configure_mcp_server(env.workspace_root());

    let prompt = env.prompt(
        "Use the parity echo MCP tool to echo the text 'hello from mcp parity', \
         then tell me exactly what it returned.",
        "mcp_tool_roundtrip",
    );

    // `danger-full-access` so the MCP tool call is not gated behind an
    // interactive permission prompt (this test exercises transport +
    // runtime, not the permission surface).
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);

    // Assert on the `echo:`-prefixed form, not the bare input text: the
    // `echo:` prefix is added only by the MCP server's response, so it
    // can only appear if the call actually round-tripped through the
    // NDJSON transport and the nested-runtime bridge. Matching the bare
    // input would false-positive on the turn-1 tool-call arguments.
    sess.expect("echo:hello from mcp parity")
        .expect("response should surface the echoed MCP tool output");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "MCP tool turn should exit 0; got {exit}");
}
