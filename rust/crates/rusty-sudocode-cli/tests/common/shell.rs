//! Resolve the POSIX shell the test harness drives children through.
//!
//! Lives in its own file (rather than inline in `common/mod.rs`) so suites that
//! are otherwise self-contained — `mock_parity_harness` has no `mod common` —
//! can `#[path = "common/shell.rs"] mod shell;` and still go through this one
//! SSOT instead of hand-rolling a second `sh` lookup.

/// Resolve the `sh` binary to the full path portable_pty needs.
///
/// On Unix this is trivially `"sh"` — the CreateProcess-equivalent
/// (posix_spawn) does PATH resolution. On Windows, portable_pty's
/// `CommandBuilder::new("sh")` hands the raw name to CreateProcessW,
/// which does NOT look up PATH; the child spawn then fails with
/// `os error 2` ("system cannot find the specified file"). Resolve
/// against Git for Windows' bundled `sh.exe` first, then fall back to
/// PATH scanning so contributors with a different sh installation
/// (WSL, MSYS2, chocolatey) are still covered.
///
/// Public so test files with their own bespoke spawn helpers (e.g.
/// `pty_mcp_manage`) resolve `sh` through this one SSOT rather than
/// handing a bare `"sh"` to `CreateProcessW` (which fails `os error 2`
/// on Windows).
pub fn resolve_sh() -> String {
    #[cfg(unix)]
    {
        String::from("sh")
    }
    #[cfg(windows)]
    {
        let candidates = [
            "C:\\Program Files\\Git\\usr\\bin\\sh.exe",
            "C:\\Program Files\\Git\\bin\\sh.exe",
            "C:\\Program Files (x86)\\Git\\usr\\bin\\sh.exe",
        ];
        for candidate in candidates {
            if std::path::Path::new(candidate).exists() {
                return candidate.to_string();
            }
        }
        if let Some(path_env) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_env) {
                let candidate = dir.join("sh.exe");
                if candidate.exists() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
        }
        // Last resort — will produce a clear "os error 2" spawn
        // failure rather than a silent hang, and matches historical
        // Linux CI behaviour.
        String::from("sh")
    }
}
