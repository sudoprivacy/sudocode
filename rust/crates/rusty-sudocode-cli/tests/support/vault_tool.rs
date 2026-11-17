//! Runs the builtin Vault plugin's tool entry, `scode vault tool <list|use>`,
//! the way the plugin host does: the tool input on stdin, exactly one JSON
//! object on stdout and nothing on stderr.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// One tool run: its single JSON result and the raw stdout it came from.
pub struct ToolRun {
    pub result: Value,
    pub stdout: String,
}

/// Run `scode vault tool <tool>` from `project`, with `home` as the home
/// directory and `config` as the global (user-layer) config directory.
pub fn run(project: &Path, home: &Path, config: &Path, tool: &str, input: &str) -> ToolRun {
    let mut child = Command::new(scode())
        .args(["vault", "tool", tool])
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("SCODE_GLOBAL_CONFIG_DIR", config)
        .env_remove("SUDO_CODE_CONFIG_HOME")
        .env_remove("SCODE_PROJECT_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch scode");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write the tool input");
    let output = child.wait_with_output().expect("scode exits");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vault tool {tool} exited {}",
        output.status
    );
    assert!(
        stderr.is_empty(),
        "vault tool {tool} wrote stderr: {stderr}"
    );
    let result: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("expected one JSON object ({error}): {stdout}"));
    assert!(result.is_object(), "{stdout}");
    ToolRun { result, stdout }
}

/// The scode cargo built beside this test, or the relocated copy
/// `SUDOCODE_VAULT_SCODE` names: e2e/vault-live/run-native.sh runs a prebuilt
/// bundle on a host that has no compiler, where cargo's path does not exist.
fn scode() -> PathBuf {
    std::env::var_os("SUDOCODE_VAULT_SCODE")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_scode")), PathBuf::from)
}
