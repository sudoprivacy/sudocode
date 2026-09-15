//! PTY tests for the `TodoWrite` tool — the CC-parity todo checklist.
//!
//! Coverage target: roadmap row "TodoWrite + auto-verification streak nudge".
//! `TodoWrite` takes the whole list and replaces the previous one (no per-item
//! id, no partial update), so these tests drive it through the deterministic
//! mock scenarios and assert the terminal + persisted store.
//!
//! ```bash
//! cargo test --test pty_todo_write                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_todo_write  # real API
//! ```

mod common;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. A single TodoWrite round-trips: tool call → result → final text
// ──────────────────────────────────────────────────────────────────────

/// The model opens a two-item todo list via TodoWrite; the tool call and its
/// result surface in the terminal and the turn exits 0.
#[test]
fn todo_write_roundtrips_a_list() {
    let env = TestEnv::new("todo-write");

    let prompt = env.prompt(
        "Create a todo list with TodoWrite for: 'Write parser' (in progress) and \
         'Run tests' (pending). Just call the tool.",
        "todo_write_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TodoWrite",
        &prompt,
    ]);

    sess.expect("(?i)todowrite")
        .expect("model must invoke TodoWrite (agent trigger)");

    if env.is_mock() {
        // The mock's final message echoes the tool output, which includes the
        // saved list — the command should round-trip to completion.
        sess.expect("todo_write roundtrip complete")
            .expect("TodoWrite result should feed the final message");
    }

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "todo_write turn should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 2. TodoWrite persists the list to the store (verbatim), and an empty
//    list wipes it — whole-list replace semantics on disk.
// ──────────────────────────────────────────────────────────────────────

/// After a TodoWrite the store file contains the exact list; a subsequent
/// empty TodoWrite clears the store to `[]`. Both are verified by pointing
/// `SUDOCODE_TODO_STORE` at a temp file and reading it after the turn.
#[test]
fn todo_write_persists_then_empty_wipes_store() {
    let env = TestEnv::new("todo-write-persist");

    let store = env.workspace_root().join("todos.json");

    // First turn: open a list.
    let prompt = env.prompt(
        "Create a todo list with TodoWrite. Just call the tool.",
        "todo_write_roundtrip",
    );
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "TodoWrite",
            &prompt,
        ],
        &[("SUDOCODE_TODO_STORE", store.to_str().unwrap())],
    );
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);

    if env.is_mock() {
        let saved = std::fs::read_to_string(&store).expect("store file should exist after write");
        assert!(
            saved.contains("Write parser") && saved.contains("Run tests"),
            "store must hold the written list verbatim; got: {saved}"
        );

        // Second turn: an empty TodoWrite wipes the store to `[]`.
        let empty_prompt = env.prompt(
            "Clear the todo list with TodoWrite (send an empty list).",
            "todo_write_empty_roundtrip",
        );
        let mut sess = env.spawn_with_env(
            &[
                "--permission-mode",
                "workspace-write",
                "--allowedTools",
                "TodoWrite",
                &empty_prompt,
            ],
            &[("SUDOCODE_TODO_STORE", store.to_str().unwrap())],
        );
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);

        let wiped = std::fs::read_to_string(&store).expect("store file should still exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&wiped).expect("store must be valid JSON");
        assert_eq!(
            parsed.as_array().map(Vec::len),
            Some(0),
            "empty TodoWrite must wipe the store to []; got: {wiped}"
        );
    }
}
