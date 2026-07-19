//! Shared PTY test helpers used by `tests/pty_*.rs` integration tests.
//!
//! ## Dual-mode test backend (mock / live)
//!
//! Every PTY test goes through [`TestEnv`]. The backend is selected by
//! the `SCODE_TEST_BACKEND` environment variable:
//!
//! | Value           | Behavior                                          |
//! |-----------------|---------------------------------------------------|
//! | unset / `mock`  | Starts `MockAnthropicService`, injects temp config|
//! | `live`          | Uses real `~/.nexus/sudocode` config (proxy auth)  |
//!
//! **If you add a new PTY test, you MUST use `TestEnv`.** This ensures
//! every test automatically works in both mock and live mode. Do NOT
//! create `MockAnthropicService` directly in test files.
//!
//! Mock mode runs in CI (no API keys). Live mode runs locally with
//! real credentials (`SCODE_TEST_BACKEND=live cargo test --test pty_*`).
//!
//! ## How to write a dual-mode test
//!
//! ```ignore
//! let env = TestEnv::new("my-test");
//! let prompt = env.prompt("Explain X briefly", "my_mock_scenario");
//! let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
//! // structural assertions — work in both modes:
//! sess.expect("some pattern").unwrap();
//! // mock-only precision assertions:
//! if env.is_mock() { sess.expect("exact mock text").unwrap(); }
//! ```

#![allow(dead_code)] // each test file uses a subset

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use pty_expect::{PtySession, Result};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Default PTY-test timeout for `expect` operations.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Live-mode timeout — real API calls can take a few seconds.
pub const LIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Locate the compiled `scode` binary for the current test run.
#[must_use]
pub fn scode_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scode"))
}

/// Spawn `scode <args...>` under a PTY with the default timeout.
pub fn spawn_scode(args: &[&str]) -> Result<PtySession> {
    spawn_scode_with_timeout(args, DEFAULT_TIMEOUT)
}

/// Spawn `scode <args...>` under a PTY with an explicit timeout.
pub fn spawn_scode_with_timeout(args: &[&str], timeout: Duration) -> Result<PtySession> {
    let bin = scode_bin();
    let bin_str = bin.to_string_lossy();
    let mut sess = PtySession::spawn(&bin_str, args)?;
    sess.set_default_timeout(timeout);
    Ok(sess)
}

// ──────────────────────────────────────────────────────────────────────
// Dual-mode test environment
// ──────────────────────────────────────────────────────────────────────

/// The backend a test runs against.
enum Backend {
    /// Deterministic mock — CI-safe, no API key required.
    Mock {
        _runtime: tokio::runtime::Runtime,
        server: MockAnthropicService,
        workspace: HarnessWorkspace,
    },
    /// Real API via the user's `~/.nexus/sudocode/sudocode.json`.
    Live {
        workspace: HarnessWorkspace,
        config_home: PathBuf,
    },
}

/// **The single entry point for all PTY tests that talk to a model.**
///
/// Reads `SCODE_TEST_BACKEND` to pick mock or live, then owns the
/// mock server (if any), the temp workspace, and exposes helpers that
/// adapt prompts and spawn commands to the active backend.
///
/// ## Why this exists
///
/// A future contributor (human or AI) adding a PTY test MUST go
/// through `TestEnv` to get a `PtySession`. Because the struct
/// controls prompt construction and spawn args, both modes stay in
/// sync automatically — you cannot accidentally write a mock-only
/// test.
/// Process-wide mutex that serialises live-mode tests. Multiple scode
/// processes hitting the same API key concurrently causes rate-limit
/// failures. In mock mode this lock is never acquired — tests run in
/// parallel as usual.
static LIVE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct TestEnv {
    backend: Backend,
    /// Holds the live serial lock for the duration of the test.
    _live_guard: Option<std::sync::MutexGuard<'static, ()>>,
}

impl TestEnv {
    /// Create a new test environment.  Backend is chosen by
    /// `SCODE_TEST_BACKEND` (default: `mock`).
    pub fn new(label: &str) -> Self {
        let mode = std::env::var("SCODE_TEST_BACKEND").unwrap_or_else(|_| "mock".to_string());
        match mode.as_str() {
            "live" => Self::new_live(label),
            _ => Self::new_mock(label),
        }
    }

    fn new_mock(label: &str) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let server = runtime
            .block_on(MockAnthropicService::spawn())
            .expect("mock server");
        let workspace = HarnessWorkspace::new(label);
        workspace.write_mock_config(&server.base_url());
        Self {
            backend: Backend::Mock {
                _runtime: runtime,
                server,
                workspace,
            },
            _live_guard: None,
        }
    }

    fn new_live(label: &str) -> Self {
        // Serialise live tests — rate-limit protection.
        let guard = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());

        let config_home = default_config_home();
        assert!(
            config_home.join("sudocode.json").exists(),
            "SCODE_TEST_BACKEND=live but {}/sudocode.json not found",
            config_home.display()
        );
        let workspace = HarnessWorkspace::new(label);
        Self {
            backend: Backend::Live {
                workspace,
                config_home,
            },
            _live_guard: Some(guard),
        }
    }

    /// `true` when running against the mock server (default / CI).
    #[must_use]
    pub fn is_mock(&self) -> bool {
        matches!(self.backend, Backend::Mock { .. })
    }

    /// `true` when running against a real API.
    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self.backend, Backend::Live { .. })
    }

    /// Build a prompt string.
    ///
    /// - **Mock mode**: returns `"{natural_prompt} PARITY_SCENARIO:{scenario}"`
    ///   so the mock server can route to the right canned response.
    /// - **Live mode**: returns `natural_prompt` as-is.
    ///
    /// This is the key DRY mechanism — every test writes ONE natural
    /// prompt and ONE scenario name. The env adapts.
    #[must_use]
    pub fn prompt(&self, natural_prompt: &str, mock_scenario: &str) -> String {
        match &self.backend {
            Backend::Mock { .. } => {
                format!("{natural_prompt} {SCENARIO_PREFIX}{mock_scenario}")
            }
            Backend::Live { .. } => natural_prompt.to_string(),
        }
    }

    /// Spawn `scode` under a PTY with the right auth, model, and
    /// config for the active backend.
    ///
    /// `extra_args` are appended after the backend-specific flags
    /// (e.g. `&["--permission-mode", "read-only", &prompt]`).
    pub fn spawn(&self, extra_args: &[&str]) -> PtySession {
        self.spawn_with_env(extra_args, &[])
    }

    /// Like [`spawn`] but with additional environment variables.
    pub fn spawn_with_env(&self, extra_args: &[&str], env_vars: &[(&str, &str)]) -> PtySession {
        match &self.backend {
            Backend::Mock { workspace, .. } => spawn_with_workspace(
                workspace,
                None,
                &["--auth", "api-key", "--model", "sonnet"],
                extra_args,
                DEFAULT_TIMEOUT,
                env_vars,
            ),
            Backend::Live {
                workspace,
                config_home,
            } => spawn_with_workspace(
                workspace,
                Some(config_home),
                &["--auth", "proxy", "--model", "auto"],
                extra_args,
                LIVE_TIMEOUT,
                env_vars,
            ),
        }
    }

    /// The workspace root (temp dir).  Useful for writing fixture
    /// files that tool calls will read.
    #[must_use]
    pub fn workspace_root(&self) -> &std::path::Path {
        match &self.backend {
            Backend::Mock { workspace, .. } | Backend::Live { workspace, .. } => &workspace.root,
        }
    }

    /// How many `/v1/messages` requests the mock server captured.
    /// Panics in live mode — request counting is mock-only.
    pub fn captured_message_count(&self) -> usize {
        match &self.backend {
            Backend::Mock {
                _runtime, server, ..
            } => _runtime
                .block_on(server.captured_requests())
                .iter()
                .filter(|r| r.path == "/v1/messages")
                .count(),
            Backend::Live { .. } => panic!("captured_message_count is mock-only"),
        }
    }

    /// Appropriate `expect` timeout for the backend.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        if self.is_live() {
            LIVE_TIMEOUT
        } else {
            DEFAULT_TIMEOUT
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// Workspace + spawn helpers
// ──────────────────────────────────────────────────────────────────────

/// Isolated temp directory for a single PTY test.
pub struct HarnessWorkspace {
    pub root: PathBuf,
    pub config_home: PathBuf,
    pub home: PathBuf,
}

impl HarnessWorkspace {
    pub fn new(label: &str) -> Self {
        let root = unique_temp_dir(label);
        let config_home = root.join("config-home");
        let home = root.join("home");
        fs::create_dir_all(&root).expect("workspace root");
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(&home).expect("home");
        Self {
            root,
            config_home,
            home,
        }
    }

    /// Write `sudocode.json` pointing at a mock server.
    pub fn write_mock_config(&self, mock_base_url: &str) {
        let sample = runtime::SAMPLE_SUDOCODE_JSON
            .replace("https://api.anthropic.com", mock_base_url)
            .replace("<YOUR_ANTHROPIC_API_KEY>", "test-pty-key");
        fs::write(self.config_home.join("sudocode.json"), sample)
            .expect("sudocode.json should be written");
    }
}

impl Drop for HarnessWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Spawn scode under a PTY via `env VAR=val ... scode <args>`.
///
/// `config_home_override`: if `Some`, sets `SUDO_CODE_CONFIG_HOME`
/// to this path (live mode — point at real config). If `None`, uses
/// the workspace's own config dir (mock mode — temp sudocode.json).
fn spawn_with_workspace(
    workspace: &HarnessWorkspace,
    config_home_override: Option<&std::path::Path>,
    base_args: &[&str],
    extra_args: &[&str],
    timeout: Duration,
    env_vars: &[(&str, &str)],
) -> PtySession {
    let bin = scode_bin();
    let bin_str = bin.to_string_lossy().to_string();
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    let home_str = workspace.home.display().to_string();

    // Determine the config home to use.
    let effective_config_home = config_home_override
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| workspace.config_home.display().to_string());

    // Build a shell command that cd's into the workspace root before
    // exec'ing scode.  This is necessary because `PtySession::spawn`
    // inherits the test runner's CWD, but tool calls (read_file, grep,
    // bash) need to operate in the workspace where fixture files live.
    let workspace_root = workspace.root.display().to_string();

    // Use /usr/bin/env (full POSIX path) rather than bare `env` so we
    // do NOT rely on sh's PATH lookup. On Windows sh (Git Bash),
    // sh receives PATH in Windows format (semi-colon separated with
    // C:\ paths) and can't find `env` — the resulting `exec: env: not
    // found` masquerades as a 127 exit. `/usr/bin/env` resolves the
    // same way on Linux, macOS, and Git Bash on Windows.
    let mut cmd = format!("cd {} && exec /usr/bin/env", shell_quote(&workspace_root));
    cmd.push_str(&format!(
        " SUDO_CODE_CONFIG_HOME={}",
        shell_quote(&effective_config_home)
    ));
    cmd.push_str(&format!(" HOME={}", shell_quote(&home_str)));
    cmd.push_str(" NO_COLOR=1");
    cmd.push_str(&format!(" PATH={}", shell_quote(&path)));
    cmd.push_str(" TERM=xterm");
    for (k, v) in env_vars {
        cmd.push_str(&format!(" {}={}", k, shell_quote(v)));
    }
    cmd.push_str(&format!(" {}", shell_quote(&bin_str)));
    for arg in base_args {
        cmd.push_str(&format!(" {}", shell_quote(arg)));
    }
    for arg in extra_args {
        cmd.push_str(&format!(" {}", shell_quote(arg)));
    }

    let sh = resolve_sh();
    let mut sess = PtySession::spawn(&sh, &["-c", &cmd]).expect("spawn scode");
    sess.set_default_timeout(timeout);
    sess
}

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
fn resolve_sh() -> String {
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

/// Shell-quote a string so it's safe to embed in `sh -c "..."`.
/// Wraps in single quotes and escapes any embedded single quotes.
/// Spawn `scode <args>` under a PTY with CWD set to the given
/// directory. Useful for session management tests where scode
/// requires CWD to match the session's workspace_root.
pub fn spawn_scode_in_dir(
    dir: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Result<PtySession> {
    let bin = scode_bin();
    let bin_str = bin.to_string_lossy().to_string();
    let dir_str = dir.display().to_string();

    // Run `sh -c "cd <dir> && exec /usr/bin/env <scode> <args>"` on ALL
    // platforms. On Windows this uses Git Bash's `sh.exe` (via `resolve_sh`) —
    // the same mechanism `spawn_with_workspace` already relies on.
    //
    // The previous `#[cfg(windows)]` branch spawned `cmd /c "cd /d \"dir\" &&
    // \"scode\" ..."`, which portable_pty passes to `CreateProcessW` with
    // MSVC-style quoting — embedded quotes become `\"`. `cmd.exe` does NOT
    // understand backslash-escaped quotes, so it saw `cd /d \"dir\"` as an
    // invalid path, printed "the filename, directory name, or volume label
    // syntax is incorrect", and exited 1 WITHOUT ever launching scode. Tests
    // then saw a non-zero exit (and a false "4" match on the `ESC[?1004h`
    // terminal escape). Routing through `sh -c` + `/usr/bin/env` quotes
    // reliably and resolves the scode path identically on Linux, macOS, and
    // Git Bash (see the note in `spawn_with_workspace`).
    let mut cmd = format!(
        "cd {} && exec /usr/bin/env {}",
        shell_quote(&dir_str),
        shell_quote(&bin_str)
    );
    for arg in args {
        cmd.push_str(&format!(" {}", shell_quote(arg)));
    }
    let sh = resolve_sh();
    let mut sess = PtySession::spawn(&sh, &["-c", &cmd])?;
    sess.set_default_timeout(timeout);
    Ok(sess)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn default_config_home() -> PathBuf {
    std::env::var_os("SUDO_CODE_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".nexus").join("sudocode"))
        })
        .unwrap_or_else(|| PathBuf::from(".nexus/sudocode"))
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-pty-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
