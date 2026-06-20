//! PTY tests for error paths — verifies scode handles failures
//! gracefully rather than crashing or hanging.
//!
//! Same DRY principle as all PTY tests: one set of assertions that
//! passes in both mock and live modes. Mock forces the tool call
//! via scenario routing; live model may avoid the call or hit the
//! error naturally. The assertions are structural enough to cover
//! both outcomes.
//!
//! ```bash
//! cargo test --test pty_error_paths                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_error_paths  # real API
//! ```

mod common;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. write_file in read-only mode
// ──────────────────────────────────────────────────────────────────────

/// Permission enforcement: write_file should fail in read-only mode.
///
/// Mock: tool call forced → scode returns permission error → model
///       says "denied" or "permission".
/// Live: model sees read-only → says "can't write" without calling,
///       OR calls and gets the error.
/// Both: no file created, exit 0.
#[test]
fn write_file_denied_in_read_only() {
    let env = TestEnv::new("write-denied");

    let prompt = env.prompt(
        "Create a file called 'denied.txt' with content 'should not exist' using write_file.",
        "write_file_denied",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--allowedTools",
        "write_file",
        &prompt,
    ]);

    // Structural assertion: the response must mention the permission
    // issue — whether the tool returned an error or the model
    // preemptively explained it can't write.
    sess.expect("(?i)(permission|denied|read.only|cannot|can.t write|not allowed|requires)")
        .expect("response should mention permission restriction");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "permission error should not crash; got exit {exit}"
    );

    // No file created — structural, works both modes.
    assert!(
        !env.workspace_root().join("denied.txt").exists(),
        "denied.txt should NOT exist after read-only write"
    );
    assert!(
        !env.workspace_root()
            .join("generated")
            .join("denied.txt")
            .exists(),
        "generated/denied.txt should NOT exist after read-only write"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. edit_file on non-existent file
// ──────────────────────────────────────────────────────────────────────

/// Missing file: edit_file on a file that doesn't exist.
///
/// Mock: tool call forced → scode returns "not found" → model relays.
/// Live: model may try and get error, or check first and report.
/// Both: response mentions file absence, exit 0.
#[test]
fn edit_file_not_found() {
    let env = TestEnv::new("edit-notfound");

    // Intentionally do NOT create any file.
    let prompt = env.prompt(
        "Edit the file 'missing.txt': replace 'foo' with 'bar' using edit_file.",
        "edit_file_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "edit_file,read_file",
        &prompt,
    ]);

    // Structural: response mentions file is missing/absent.
    sess.expect("(?i)(not found|no such file|does not exist|missing|doesn.t exist|cannot find)")
        .expect("response should mention file not found");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "file-not-found should not crash; got exit {exit}");
}
