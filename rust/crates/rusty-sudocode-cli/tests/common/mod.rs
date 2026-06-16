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

use std::path::PathBuf;
use std::time::Duration;

use pty_expect::{PtySession, Result};

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
