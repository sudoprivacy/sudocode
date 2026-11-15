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

#![allow(dead_code, unused_imports)] // Each original test uses a subset.

use pty_expect::{PtySession, Result};
pub use scode_test_support::*;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::time::Duration;

/// Keep the established test API and caller-local Cargo binary metadata.
pub struct TestEnv(scode_test_support::TestEnv);

impl TestEnv {
    #[must_use]
    #[inline]
    pub fn new(label: &str) -> Self {
        Self(scode_test_support::TestEnv::new(label, scode_bin))
    }

    #[must_use]
    #[inline]
    pub fn new_mock(label: &str) -> Self {
        Self(scode_test_support::TestEnv::new_mock(label, scode_bin))
    }

    #[must_use]
    #[inline]
    pub fn peer(&self, label: &str) -> Self {
        Self(self.0.peer(label))
    }
}

impl Deref for TestEnv {
    type Target = scode_test_support::TestEnv;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TestEnv {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Locate the compiled CLI, or a release artifact selected for live acceptance.
#[must_use]
#[inline]
pub fn scode_bin() -> PathBuf {
    std::env::var_os("SCODE_TEST_BIN")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_BIN_EXE_scode").map(PathBuf::from))
        .expect(
            "standalone benchmark drivers require SCODE_TEST_BIN pointing to the measured release",
        )
}

/// Spawn `scode <args...>` under a PTY with the default timeout.
#[inline]
pub fn spawn_scode(args: &[&str]) -> Result<IsolatedSession> {
    spawn_scode_with_timeout(args, DEFAULT_TIMEOUT)
}

/// Spawn `scode <args...>` under a PTY with an explicit timeout, in a scratch
/// workspace with its own home and config home. Without them it reads the
/// developer's own settings and the checkout's project config, so a key the
/// developer's settings still carry fails these tests on that machine only.
#[inline]
pub fn spawn_scode_with_timeout(args: &[&str], timeout: Duration) -> Result<IsolatedSession> {
    let workspace = HarnessWorkspace::new("scode");
    let session = spawn_binary_in_dir_with_env(
        &scode_bin(),
        &workspace.root,
        args,
        timeout,
        &[
            ("HOME", &workspace.home),
            ("SUDO_CODE_CONFIG_HOME", &workspace.config_home),
        ],
    )?;
    Ok(IsolatedSession {
        session,
        _workspace: workspace,
    })
}

/// A session that owns its scratch workspace. The session is declared first
/// so it drops first: the process ends before its directory is removed.
pub struct IsolatedSession {
    session: PtySession,
    _workspace: HarnessWorkspace,
}

impl Deref for IsolatedSession {
    type Target = PtySession;
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl DerefMut for IsolatedSession {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

/// Spawn the caller's selected CLI in an explicit directory.
#[inline]
pub fn spawn_scode_in_dir(
    dir: &std::path::Path,
    args: &[&str],
    timeout: Duration,
) -> Result<PtySession> {
    spawn_scode_in_dir_with_env(dir, args, timeout, &[])
}

/// Preserve the existing directory, argument and environment contract.
#[inline]
pub fn spawn_scode_in_dir_with_env(
    dir: &std::path::Path,
    args: &[&str],
    timeout: Duration,
    env: &[(&str, &std::path::Path)],
) -> Result<PtySession> {
    scode_test_support::spawn_binary_in_dir_with_env(&scode_bin(), dir, args, timeout, env)
}
