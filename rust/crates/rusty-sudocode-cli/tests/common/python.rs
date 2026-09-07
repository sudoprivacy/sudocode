//! Resolve a working Python 3 interpreter for tests that drive a Python helper
//! subprocess (the mock MCP servers `pty_mcp_tool` and `acp_integration` spawn).
//!
//! The reason this isn't simply `"python3"`: on Windows that name almost always
//! resolves to the WindowsApps *App Execution Alias* — a zero-byte reparse
//! point that opens the Microsoft Store rather than running anything. An MCP
//! server spawned through it never writes a byte, so the handshake just hangs
//! until the test times out. `python` may be the real interpreter or may be
//! another alias, and which one is real differs per machine, so name-matching
//! can't decide it. The only reliable test is to *run* each candidate, which is
//! what this does — once per process, then cached.
//!
//! Sibling of `common::resolve_sh`, and for the same underlying reason: the
//! POSIX name of an interpreter is not a portable way to reach it.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Command name (or absolute path) of a Python 3 interpreter that actually
/// runs. Panics with the candidates tried if none does — a loud failure beats
/// a subprocess that silently never speaks.
pub fn resolve_python() -> String {
    static RESOLVED: OnceLock<String> = OnceLock::new();
    RESOLVED.get_or_init(resolve_python_uncached).clone()
}

fn resolve_python_uncached() -> String {
    #[cfg(windows)]
    let candidates: &[&str] = &["python", "python3"];
    #[cfg(not(windows))]
    let candidates: &[&str] = &["python3", "python"];

    for candidate in candidates {
        if runs_python3(candidate) {
            return (*candidate).to_string();
        }
    }

    // Windows fallback: the `py` launcher knows where the real interpreters
    // are even when the PATH entries are Store aliases. Ask it for the
    // absolute path rather than shelling through `py -3` itself, so callers
    // keep dealing with a single executable and no extra argv prefix.
    #[cfg(windows)]
    if let Ok(output) = Command::new("py")
        .args(["-3", "-c", "import sys; print(sys.executable)"])
        .stderr(Stdio::null())
        .output()
    {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return path;
            }
        }
    }

    panic!("no working Python 3 interpreter found (tried {candidates:?} and the `py` launcher)");
}

/// `true` if running `<command> -c ...` succeeds — the check a Store alias
/// fails, because it exits non-zero (or pops the Store) instead of executing.
fn runs_python3(command: &str) -> bool {
    Command::new(command)
        .args(["-c", "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
