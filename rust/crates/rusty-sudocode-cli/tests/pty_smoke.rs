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
/// `#[cfg(unix)]` because pty-expect's Windows ConPTY runtime is
/// deferred to v0.2 (the Windows code path in pty-expect itself is
/// gated the same way). On the Windows compile-check builds this
/// test still type-checks; it just doesn't run.
#[cfg(unix)]
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
