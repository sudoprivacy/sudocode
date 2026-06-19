//! Shared PTY test helpers used by `tests/pty_*.rs` integration tests.
//!
//! This module is loaded by sibling test files via `mod common;`. Rust
//! treats it as a submodule rather than a separate test binary because
//! its file is `common/mod.rs`, not `common.rs` at the top level — no
//! test binary is built from this file by itself.
//!
//! The helpers wrap `pty-expect` with sudocode-specific conveniences:
//! locating the compiled `scode` binary, applying a sensible default
//! timeout, and (in future commits) common workspace setup like a
//! tempdir and pointing scode at the mock Anthropic service.
//!
//! ## Why these wrappers exist
//!
//! Every PTY test starts the same way — find the binary, allocate a
//! PTY, set a timeout, spawn. Bare `pty_expect::PtySession::spawn`
//! plus `env!("CARGO_BIN_EXE_scode")` would scatter that prelude
//! across every test file. Centralising it here:
//!
//! - keeps individual `tests/pty_*.rs` files focused on the *scenario*
//!   (the multi-step real-user workflow), not the boilerplate;
//! - lets us change the spawn convention once (e.g. inject a default
//!   `--config-dir`) without touching every test;
//! - matches the cli/ existing-tests pattern where helpers like
//!   `unique_temp_dir(label)` were duplicated file-by-file — this is
//!   the consolidated version for the PTY layer.

#![allow(dead_code)] // each test file uses a subset

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pty_expect::{PtySession, Result};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Default PTY-test timeout for `expect` operations.
///
/// Most CLI prompts complete in well under a second; this generous
/// budget covers cold cargo-test startup, syntect highlight init, and
/// the first model-list HTTP round trip (when mocked). Bump it
/// locally inside a test if you need to wait on a slow tool call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Locate the compiled `scode` binary for the current test run.
///
/// Cargo sets `CARGO_BIN_EXE_<bin name>` at integration-test compile
/// time for every binary in the same crate. The crate's
/// `[[bin]] name = "scode"` makes this resolve to the freshly-built
/// `scode` for whatever profile (`debug` / `release`) the test is
/// running under.
#[must_use]
pub fn scode_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_scode"))
}

/// Spawn `scode <args...>` under a PTY with the default timeout
/// applied to subsequent `expect` calls.
///
/// Returns the live session; the caller drives it with
/// `expect`/`send_line`/`send_ctrl`/`expect_eof` against the PTY's
/// raw byte stream. Drop semantics are owned by `PtySession` itself
/// (the child gets reaped on drop).
///
/// # Errors
///
/// Returns any `pty_expect::Error` from the spawn or timeout
/// configuration; tests usually `.expect("spawn scode")` because a
/// failure here is a framework-level problem, not a test failure.
pub fn spawn_scode(args: &[&str]) -> Result<PtySession> {
    spawn_scode_with_timeout(args, DEFAULT_TIMEOUT)
}

/// Variant of `spawn_scode` taking an explicit timeout — use when the
/// scenario waits on something legitimately slow (live API, long
/// bash command, large file scan) and the default would falsely
/// fire.
///
/// # Errors
///
/// Same as [`spawn_scode`].
pub fn spawn_scode_with_timeout(args: &[&str], timeout: Duration) -> Result<PtySession> {
    let bin = scode_bin();
    let bin_str = bin.to_string_lossy();
    let mut sess = PtySession::spawn(&bin_str, args)?;
    sess.set_default_timeout(timeout);
    Ok(sess)
}

/// Isolated workspace for PTY tests that talk to a mock Anthropic
/// service. Mirrors the workspace layout from `mock_parity_harness.rs`
/// but owns its own temp directory.
pub struct HarnessWorkspace {
    pub root: PathBuf,
    pub config_home: PathBuf,
    pub home: PathBuf,
}

impl HarnessWorkspace {
    /// Create a new workspace under a unique temp directory.
    pub fn new(label: &str) -> Self {
        let root = unique_temp_dir(label);
        let config_home = root.join("config-home");
        let home = root.join("home");
        fs::create_dir_all(&root).expect("workspace root should be created");
        fs::create_dir_all(&config_home).expect("config home should be created");
        fs::create_dir_all(&home).expect("home should be created");
        Self {
            root,
            config_home,
            home,
        }
    }

    /// Write `sudocode.json` pointing at the given mock server URL.
    pub fn write_config(&self, mock_base_url: &str) {
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

/// Spawn `scode` under a PTY with the given environment variable
/// overrides.
///
/// Launches via `env VAR=val ... scode <args>` so that the specified
/// variables override the inherited environment. This is necessary
/// because `pty-expect`'s `PtySession::spawn` does not expose
/// `CommandBuilder::env`.
pub fn spawn_scode_with_env(
    args: &[&str],
    env_vars: &[(&str, &str)],
    timeout: Duration,
) -> Result<PtySession> {
    let bin = scode_bin();
    let bin_str = bin.to_string_lossy().to_string();

    let mut env_args: Vec<String> = Vec::new();
    for (key, value) in env_vars {
        env_args.push(format!("{key}={value}"));
    }
    env_args.push(bin_str);
    for arg in args {
        env_args.push((*arg).to_string());
    }

    let arg_refs: Vec<&str> = env_args.iter().map(|s| s.as_str()).collect();
    let mut sess = PtySession::spawn("env", &arg_refs)?;
    sess.set_default_timeout(timeout);
    Ok(sess)
}

/// Spawn `scode` under a PTY pre-configured to talk to a mock
/// Anthropic service via the given workspace.
pub fn spawn_scode_mock(workspace: &HarnessWorkspace, extra_args: &[&str]) -> Result<PtySession> {
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    let config_home = workspace.config_home.display().to_string();
    let home = workspace.home.display().to_string();

    let env_vars: Vec<(&str, &str)> = vec![
        ("SUDO_CODE_CONFIG_HOME", &config_home),
        ("HOME", &home),
        ("NO_COLOR", "1"),
        ("PATH", &path),
        ("TERM", "xterm"),
    ];

    let mut args = vec!["--auth", "api-key", "--model", "sonnet"];
    args.extend_from_slice(extra_args);

    spawn_scode_with_env(&args, &env_vars, DEFAULT_TIMEOUT)
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
