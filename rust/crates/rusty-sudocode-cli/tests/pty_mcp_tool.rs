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
//! Runs on Windows too: the PTY harness is cross-platform (`common::resolve_sh`)
//! and the mock MCP server reaches a real interpreter through
//! `common::resolve_python` rather than the bare `python3` name, which on
//! Windows is a Microsoft Store alias that never runs the script.
//!
//! ```bash
//! cargo test --test pty_mcp_tool                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_mcp_tool  # real API
//! ```

mod common;

use std::fs;
use std::path::Path;

use common::TestEnv;

/// Minimal NDJSON MCP server exposing a single `echo` tool. Speaks the
/// MCP spec's newline-delimited JSON framing: one JSON-RPC message per
/// line, `\n`-terminated, no `Content-Length` headers. scode's client
/// performs initialization and discovery (`initialize` → `tools/list`)
/// before `tools/call`; ID-less notifications are ignored.
const MCP_SERVER_SCRIPT: &str = r#"import base64, json, struct, sys, zlib

protocol_log = open(sys.argv[1], 'a', encoding='utf-8')
print('process start', file=protocol_log, flush=True)

def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))

png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 64, 64, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress((b'\x00' + b'\xff\x00\x00' * 64) * 64)) + chunk(b'IEND', b'')
image = base64.b64encode(png).decode('ascii')

def read_message():
    line = sys.stdin.buffer.readline()
    if not line:
        return None
    return json.loads(line.decode())

def send_message(message):
    payload = json.dumps(message).encode()
    sys.stdout.buffer.write(payload + b'\n')
    sys.stdout.buffer.flush()
    print(f'completed response: {method} id={message["id"]} success={"result" in message}', file=protocol_log, flush=True)

while True:
    request = read_message()
    if request is None:
        break
    method = request.get('method')
    print(f'incoming method: {method} id={request.get("id")}', file=protocol_log, flush=True)
    if 'id' not in request:
        continue
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
                'content': [{'type': 'text', 'text': f'echo:{text}'}, {'type': 'image', 'data': image, 'mimeType': 'image/png'}],
                'isError': False,
            },
        })
    else:
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

    let output = std::process::Command::new(common::resolve_python())
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("MCP fixture Python should resolve");
    assert!(output.status.success(), "{output:?}");
    let python = String::from_utf8(output.stdout).unwrap().trim().to_string();
    assert!(Path::new(&python).is_absolute());
    let status = std::process::Command::new(&python)
        .env_remove("PATH")
        .args([
            "-c",
            "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)",
        ])
        .status()
        .expect("MCP fixture Python should start without PATH");
    assert!(status.success());

    let settings_dir = workspace_root.join(".nexus").join("sudocode");
    fs::create_dir_all(&settings_dir).expect("project config dir should be created");
    let settings = serde_json::json!({
        "mcpServers": {
            "parity": {
                "command": python,
                "args": [
                    script_path.display().to_string(),
                    workspace_root.join("parity-mcp-protocol.log").display().to_string(),
                ],
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
         then tell me exactly what it returned in English, including the dominant color of its image.",
        "mcp_tool_roundtrip",
    );

    // `danger-full-access` so the MCP tool call is not gated behind an
    // interactive permission prompt (this test exercises transport +
    // runtime, not the permission surface).
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access", &prompt],
        &[("SUDOCODE_ENABLE_MCP", "1")],
    );
    if env.is_live() {
        // A real gateway may spend longer than 30s in queue/tool discovery.
        // Keep a bounded live deadline without slowing deterministic CI runs.
        sess.set_default_timeout(std::time::Duration::from_mins(3));
    }

    // Assert on the `echo:`-prefixed form, not the bare input text: the
    // `echo:` prefix is added only by the MCP server's response, so it
    // can only appear if the call actually round-tripped through the
    // NDJSON transport and the nested-runtime bridge. Matching the bare
    // input would false-positive on the turn-1 tool-call arguments.
    sess.expect("echo:hello from mcp parity")
        .unwrap_or_else(|error| {
            let protocol_log =
                fs::read_to_string(env.workspace_root().join("parity-mcp-protocol.log"))
                    .unwrap_or_else(|error| format!("protocol log unavailable: {error}"));
            panic!(
                "MCP result was not displayed: {error:?}\n\
                 A successful tools/list response with no incoming tools/call means model non-use \
                 or a dispatch issue, not an MCP handshake failure.\n\
                 MCP protocol log:\n{protocol_log}\nScreen tail:\n{}",
                common::screen_tail(&sess, 5000)
            );
        });

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "MCP tool turn should exit 0; got {exit}");
    if env.is_live() {
        let screen = common::screen_tail(&sess, 5000);
        assert!(
            screen.to_lowercase().contains("red"),
            "the real model must identify the MCP image color: {screen}"
        );
    }
    if !env.is_live() {
        let requests = env.captured_message_bodies();
        assert!(
            requests
                .iter()
                .any(|body| body.contains("\"type\":\"image\"") && body.contains("image/png")),
            "the model must receive a native MCP image attachment"
        );
        for body in requests {
            let request: serde_json::Value =
                serde_json::from_str(&body).expect("model request JSON");
            for message in request["messages"].as_array().expect("messages") {
                for block in message["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_result" {
                        assert!(
                            !block.to_string().contains("\\\"data\\\""),
                            "base64 must not appear inside tool text"
                        );
                    }
                }
            }
        }
    }
}
