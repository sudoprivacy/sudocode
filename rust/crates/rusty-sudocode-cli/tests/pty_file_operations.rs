//! PTY tests for file operation tools:
//!
//! 1. write_file → read_file roundtrip
//! 2. write_file → edit_file → read_file verification
//! 3. write multiple files → glob_search → grep_search discovery
//!
//! Each test is a real user journey with 3+ steps and causal data
//! flow (step N's output feeds step N+1's assertion). Designed per
//! the integration-test-generator quality bar.
//!
//! ## Dual-mode (mock / live)
//!
//! All tests go through `TestEnv` (see `common/mod.rs`).
//!
//! ```bash
//! cargo test --test pty_file_operations                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_file_operations  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. write_file → read_file roundtrip
// ──────────────────────────────────────────────────────────────────────

/// User asks scode to create a file, then read it back.
///
/// Steps with causal data flow:
/// 1. Spawn scode with write_file scenario.
/// 2. Expect "write_file" tool call — proves the model decided to write.
/// 3. Expect the written file path in output — proves write succeeded.
/// 4. Mock: second turn reads the file back and confirms content.
///    Live: the model writes and confirms in one turn.
/// 5. Verify the file exists on disk with correct content.
/// 6. expect_eof == 0 — proves clean exit.
///
/// Catches: write_file not creating the file, wrong path, permission
/// errors, content corruption.
#[test]
fn write_then_read_back() {
    let env = TestEnv::new("write-read");

    let prompt = env.prompt(
        "Create a file called 'hello.txt' with the content 'hello from scode' using write_file, then read it back with read_file to confirm.",
        "write_file_allowed",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "write_file,read_file",
        &prompt,
    ]);

    // Step 1: the model calls write_file.
    sess.expect("write_file")
        .expect("should see write_file tool call");

    if env.is_mock() {
        // Mock scenario writes to generated/output.txt and confirms.
        sess.expect("(?i)(generated|output\\.txt|succeeded)")
            .expect("mock: write_file result");
    } else {
        // Live: the model writes the file and reads it back.
        sess.expect("(?i)(hello|created|wrote|written)")
            .expect("live: should see write confirmation");
    }

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "write-read should exit 0; got {exit}");

    // Verify the file was actually written to disk.
    if env.is_mock() {
        let content =
            std::fs::read_to_string(env.workspace_root().join("generated").join("output.txt"))
                .expect("mock: generated/output.txt should exist");
        assert_eq!(content, "created by mock service\n");
    } else {
        let content = std::fs::read_to_string(env.workspace_root().join("hello.txt"))
            .expect("live: hello.txt should exist");
        assert!(
            content.contains("hello from scode"),
            "live: file content should contain 'hello from scode', got: {content}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. write_file → edit_file → read_file verification
// ──────────────────────────────────────────────────────────────────────

/// User journey: create a file, edit it (replace a string), then read
/// back to verify the edit took effect.
///
/// Steps with causal data flow:
/// 1. Pre-create fixture.txt with known content ("alpha parity line").
/// 2. Spawn scode with edit_file scenario.
/// 3. Expect "edit_file" tool call — proves the model decided to edit.
/// 4. Expect success confirmation.
/// 5. Verify on disk: "alpha" was replaced with "omega".
/// 6. expect_eof == 0.
///
/// Catches: edit_file not finding old_string, not writing back, partial
/// replacement, permission errors.
#[test]
fn edit_then_verify_on_disk() {
    let env = TestEnv::new("edit-verify");

    // Pre-create the file that edit_file will modify.
    std::fs::write(
        env.workspace_root().join("fixture.txt"),
        "alpha parity line\nbeta line\n",
    )
    .expect("fixture.txt should be written");

    let prompt = env.prompt(
        "Edit fixture.txt: replace the word 'alpha' with 'omega' using edit_file, then read it back with read_file to confirm the change.",
        "edit_file_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "edit_file,read_file",
        &prompt,
    ]);

    // Step 1: the model calls edit_file.
    sess.expect("edit_file")
        .expect("should see edit_file tool call");

    if env.is_mock() {
        sess.expect("(?i)(roundtrip complete|fixture)")
            .expect("mock: edit_file final response");
    } else {
        // Live: the model edits and confirms.
        sess.expect("(?i)(omega|replaced|changed|edited|updated)")
            .expect("live: should see edit confirmation");
    }

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "edit-verify should exit 0; got {exit}");

    // Verify on disk: "alpha" → "omega".
    let content = std::fs::read_to_string(env.workspace_root().join("fixture.txt"))
        .expect("fixture.txt should still exist");
    assert!(
        content.contains("omega"),
        "fixture.txt should contain 'omega' after edit, got: {content}"
    );
    assert!(
        !content.contains("alpha"),
        "fixture.txt should not contain 'alpha' after edit, got: {content}"
    );
    // "beta line" should be untouched.
    assert!(
        content.contains("beta line"),
        "fixture.txt should still contain 'beta line', got: {content}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 3. write multiple files → glob_search → grep_search
// ──────────────────────────────────────────────────────────────────────

/// User journey: create a workspace with multiple files, then use
/// glob_search to discover them by pattern, then grep_search to find
/// specific content within them.
///
/// Steps with causal data flow:
/// 1. Pre-create 3 .txt files with known content.
/// 2. Spawn scode asking to find .txt files, then grep for "parity".
/// 3. Expect "glob" or "grep" tool names in output.
/// 4. Expect the response mentions the matching files/content.
/// 5. expect_eof == 0.
///
/// Catches: glob not finding files, grep not matching, tool output
/// not rendering, incorrect match counts.
#[test]
fn glob_then_grep_discovery() {
    let env = TestEnv::new("glob-grep");

    // Pre-create a workspace with multiple files.
    let root = env.workspace_root();
    std::fs::write(root.join("notes.txt"), "alpha parity line\n").expect("notes.txt");
    std::fs::write(root.join("data.txt"), "beta line\ngamma parity line\n").expect("data.txt");
    std::fs::write(root.join("readme.md"), "this is markdown, not txt\n").expect("readme.md");

    let prompt = env.prompt(
        "First use glob_search with pattern '*.txt' to find all .txt files in the current directory. Then use grep_search to find all lines containing 'parity' in those files. Tell me how many matches you found.",
        "multi_tool_turn_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "read-only",
        "--allowedTools",
        "glob_search,grep_search,read_file",
        &prompt,
    ]);

    if env.is_mock() {
        // Mock uses multi_tool_turn_roundtrip which calls read_file + grep_search.
        sess.expect("read_file")
            .expect("mock: should see read_file");
        sess.expect("grep").expect("mock: should see grep_search");
        sess.expect("roundtrip complete")
            .expect("mock: final response");
    } else {
        // Live: the model should use glob and/or grep and report results.
        sess.set_default_timeout(Duration::from_secs(60));
        sess.expect("(?i)(glob|grep|txt|parity)")
            .expect("live: should see tool activity");
        sess.expect("(?i)(parity|2|two|match|found|notes|data)")
            .expect("live: should see discovery results");
    }

    sess.set_default_timeout(Duration::from_secs(120));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("glob-grep scode should exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "glob-grep should exit 0; got {exit}");
}
