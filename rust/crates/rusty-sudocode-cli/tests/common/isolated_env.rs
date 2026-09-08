//! The env vars a spawned `scode` child still needs after `env_clear()`.
//!
//! Tests that spawn `scode` as a subprocess call `Command::env_clear()` so the
//! developer's real `~/.nexus/sudocode`, API keys and proxy settings can never
//! leak into a hermetic run. On Unix the only thing that must be handed back is
//! a `PATH` for the tools `bash` shells out to.
//!
//! On **Windows** `env_clear()` is far more destructive than it looks: it also
//! drops `SystemRoot` / `SystemDrive` / `windir`, and without those the child's
//! `ws2_32` (winsock) initialisation fails, so *any* socket — including the
//! loopback HTTP client that talks to `MockAnthropicService`, and the TCP
//! listener `scode acp serve` binds — never comes up. It also drops `TEMP`/`TMP`,
//! which the child needs to create temp files at all. The failure is silent and
//! looks like a hang, not an error: the child either wedges without connecting
//! (the historical ~400s `compact_output` timeout) or dies on startup so its
//! stderr closes immediately (the `acp_integration` "stderr closed before server
//! ready" panic, and the ~3h windows-latest CI wedge that first got these files
//! `#![cfg(unix)]`-gated). Every one of those was this, not a stdio/pipe-buffer
//! or ConPTY-handle problem.
//!
//! Handing the real host `PATH` back on Windows is deliberate too — the POSIX
//! `/usr/bin:/bin` is meaningless there, and `scode` needs to resolve git-bash
//! `sh.exe` for its bash tool the same way `common::resolve_sh` does.
//!
//! Returns pairs rather than taking a `&mut Command` so the one definition
//! serves both `std::process::Command` and `tokio::process::Command`; both have
//! an identical `.env(k, v)`.

/// The OS-provided part of a Windows environment.
///
/// What `env_clear()` is actually for here is keeping the developer's
/// *sudocode* state — config home, API keys, proxy settings — out of a
/// hermetic run. It is not meant to take the operating system with it, but on
/// Windows that is what it does, and the pieces below are load-bearing:
///
/// - `SystemRoot` / `SystemDrive` / `windir` — winsock, TLS and the loader.
/// - `TEMP` / `TMP` — any temp file the child creates.
/// - `PATH` / `PATHEXT` / `ComSpec` — resolving an external program *at all*.
///   `PATHEXT` is what makes a bare `printf` find `printf.exe`; without it
///   `scode`'s bash tool runs the command and captures empty output, which
///   surfaces as a mystifying "expected `alpha from bash`, got ``".
/// - `APPDATA` / `LOCALAPPDATA` / `ProgramData` / `ProgramFiles*` /
///   `PSModulePath` — PowerShell, which is the shell `scode`'s bash tool uses
///   on Windows.
/// - `USERPROFILE` / `HOMEDRIVE` / `HOMEPATH` / `USERNAME` / `COMPUTERNAME` /
///   `NUMBER_OF_PROCESSORS` / `OS` / `PROCESSOR_ARCHITECTURE` — routinely
///   assumed present by Windows tooling.
///
/// None of these can carry sudocode state, so passing them through costs the
/// isolation nothing.
#[cfg(windows)]
const WINDOWS_SYSTEM_VARS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "windir",
    "TEMP",
    "TMP",
    "PATH",
    "PATHEXT",
    "ComSpec",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "PSModulePath",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERNAME",
    "COMPUTERNAME",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PROCESSOR_ARCHITECTURE",
];

/// Env pairs to re-apply after `Command::env_clear()`, per platform.
///
/// Apply with `for (k, v) in inherited_env() { cmd.env(k, v); }`.
pub fn inherited_env() -> Vec<(&'static str, String)> {
    #[cfg(windows)]
    {
        WINDOWS_SYSTEM_VARS
            .iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (*key, value)))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![("PATH", "/usr/bin:/bin".to_string())]
    }
}
