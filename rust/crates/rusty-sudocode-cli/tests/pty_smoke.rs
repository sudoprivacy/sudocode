//! Smoke test proving the PTY test framework wires through to the
//! real `scode` binary.
//!
//! **Not a meaningful coverage test** — its only job is to fail loudly
//! if the framework wiring breaks (binary path resolution, PTY
//! allocation, ANSI stream reading, child reaping). Every other PTY
//! scenario in this directory assumes this smoke passes.
//!
//! The integration-test-generator quality bar — real multi-step
//! workflows, data flow step-to-step — applies to *coverage* tests,
//! not to framework-proof smoke. One-step `--help → Usage → EOF` is
//! the right shape for a smoke test.

mod common;

use common::spawn_scode;

/// Spawn `scode --help`, see clap's `Usage:` line, see clean exit.
///
/// This is the entry-point gauntlet for the whole PTY layer:
///
/// 1. `env!("CARGO_BIN_EXE_scode")` resolves to a real binary path.
/// 2. `pty-expect`/`portable-pty` allocates a PTY and spawns the
///    child under it.
/// 3. The child writes clap's help text through the PTY back to us.
/// 4. `expect(r"Usage:")` matches against the raw byte stream.
/// 5. The child exits, `expect_eof` reaps and reports the code.
///
/// Anything broken in the framework breaks this test loudly. Every
/// other PTY test inherits the same wiring.
///
/// Cross-platform. pty-expect v0.1.1 (this PR's dep) responds to
/// ConPTY's DSR(6) handshake automatically, so Windows local +
/// Windows CI both run this test the same way Unix does.
#[test]
fn scode_help_prints_usage_and_exits_cleanly() {
    let mut sess = spawn_scode(&["--help"]).expect("spawn scode --help");
    sess.expect(r"Usage:")
        .expect("scode --help should print a Usage: line");
    let exit = sess
        .expect_eof()
        .expect("scode --help should exit on its own");
    assert_eq!(exit, 0, "scode --help should exit 0; got {exit}");
}

#[cfg(target_os = "macos")]
#[test]
fn cold_macos_binary_is_signed_and_starts_under_a_pty() {
    // A new inode exercises first activation instead of the kernel's warm cache.
    let directory = tempfile::tempdir().expect("cold executable directory");
    let binary = directory.path().join("scode");
    std::fs::copy(common::scode_bin(), &binary).expect("copy the actual CLI");

    let mut session = pty_expect::PtySession::spawn(
        binary.to_str().expect("UTF-8 fixture executable"),
        &["--help"],
    )
    .expect("spawn the cold CLI under a real PTY");
    session.set_default_timeout(common::DEFAULT_TIMEOUT);
    session.expect("Usage:").expect("cold CLI help");
    assert_eq!(session.expect_eof().expect("cold CLI exit"), 0);

    // Verify after execution so the check cannot prewarm first activation.
    let verification = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict"])
        .arg(&binary)
        .output()
        .expect("verify the cold executable's code signature");
    assert!(
        verification.status.success(),
        "macOS CLI artifacts must carry a valid code signature: {}",
        String::from_utf8_lossy(&verification.stderr)
    );
}
