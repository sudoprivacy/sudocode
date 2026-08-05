//! PTY tests for MCP server management subcommands (add-json / remove).
//!
//! These test the `scode mcp add-json` and `scode mcp remove` CLI
//! subcommands, verifying end-to-end config file creation and mutation.
//!
//! **Why no `TestEnv`:** These tests exercise the CLI subcommand pathway
//! (`scode mcp add-json ...`), which is a pure config-file operation —
//! no model inference, no agent loop. `TestEnv` is designed for
//! agent-loop tests that need mock/live backend routing. MCP config
//! management is backend-agnostic, so `HarnessWorkspace` alone provides
//! the needed isolation.
//!
//! ```bash
//! cargo test --test pty_mcp_manage
//! ```
mod common;

use common::{HarnessWorkspace, DEFAULT_TIMEOUT};

/// Helper: spawn `scode mcp <args>` in a temp workspace.
fn spawn_mcp_in_workspace(workspace: &HarnessWorkspace, args: &[&str]) -> pty_expect::PtySession {
    let bin = common::scode_bin();
    let bin_str = bin.to_string_lossy().to_string();
    let workspace_root = workspace.root.display().to_string();
    let config_home = workspace.config_home.display().to_string();
    let home = workspace.home.display().to_string();

    let mut cmd = format!(
        "cd '{}' && exec /usr/bin/env SUDO_CODE_CONFIG_HOME='{}' HOME='{}' NO_COLOR=1 TERM=xterm '{}'",
        workspace_root, config_home, home, bin_str
    );
    for arg in args {
        cmd.push_str(&format!(" '{arg}'"));
    }
    let mut sess = pty_expect::PtySession::spawn("sh", &["-c", &cmd]).expect("spawn scode mcp");
    sess.set_default_timeout(DEFAULT_TIMEOUT);
    sess
}

/// `scode mcp add-json <name> <json>` creates a settings file and adds
/// the server, then `scode mcp list` shows it.
#[test]
fn mcp_add_json_then_list() {
    let workspace = HarnessWorkspace::new("mcp-add");

    // Add a server
    let mut sess = spawn_mcp_in_workspace(
        &workspace,
        &[
            "mcp",
            "add-json",
            "test-server",
            r#"{"command":"echo","args":["hello"]}"#,
        ],
    );
    sess.expect("Added").expect("should report server added");
    sess.expect("test-server").expect("should show server name");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);

    // Verify settings file was created
    let settings_path = workspace
        .root
        .join(".nexus")
        .join("sudocode")
        .join("settings.json");
    assert!(settings_path.exists(), "settings.json should be created");
    let content = std::fs::read_to_string(&settings_path).expect("read settings");
    assert!(content.contains("test-server"));
    assert!(content.contains("echo"));

    // List should show the server
    let mut sess = spawn_mcp_in_workspace(&workspace, &["mcp", "list"]);
    sess.expect("test-server")
        .expect("list should show the added server");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// `scode mcp remove <name>` removes a previously added server.
#[test]
fn mcp_remove_after_add() {
    let workspace = HarnessWorkspace::new("mcp-remove");

    // Add a server first
    let mut sess = spawn_mcp_in_workspace(
        &workspace,
        &["mcp", "add-json", "to-remove", r#"{"command":"cat"}"#],
    );
    sess.expect("Added").expect("should add server");
    let exit = sess.expect_eof().expect("exit");
    assert_eq!(exit, 0);

    // Remove it
    let mut sess = spawn_mcp_in_workspace(&workspace, &["mcp", "remove", "to-remove"]);
    sess.expect("removed").expect("should report removed");
    let exit = sess.expect_eof().expect("exit");
    assert_eq!(exit, 0);

    // Verify it's gone from the settings file
    let settings_path = workspace
        .root
        .join(".nexus")
        .join("sudocode")
        .join("settings.json");
    let content = std::fs::read_to_string(&settings_path).expect("read settings");
    assert!(
        !content.contains("to-remove"),
        "server should be removed from settings"
    );
}

/// `scode mcp add-json` with invalid JSON shows an error.
#[test]
fn mcp_add_json_invalid_json_errors() {
    let workspace = HarnessWorkspace::new("mcp-add-invalid");

    let mut sess =
        spawn_mcp_in_workspace(&workspace, &["mcp", "add-json", "bad-server", "not-json"]);
    sess.expect("invalid JSON|error|Error")
        .expect("should show error for invalid JSON");
    let exit = sess.expect_eof().expect("exit");
    // Non-zero exit is acceptable for errors
    assert!(exit == 0 || exit == 1);
}
