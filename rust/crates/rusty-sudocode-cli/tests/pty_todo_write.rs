//! PTY tests for `TodoWrite`.
//!
//! Coverage target: roadmap §Feature-inventory row "TodoWrite"
//! (must-have). Before this file: 0 PTY tests → row marked "Gap". After:
//! the two branches that matter in production are gated.
//!
//! ## What TodoWrite actually does (source: tools/src/lib.rs)
//!
//! `TodoWrite` persists the incoming todo list to
//! `<cwd>/.sudocode-todos.json` (or `$SUDOCODE_TODO_STORE` if set). Two
//! subtle branches:
//!
//! 1. **Some pending / in_progress** — write the full list verbatim.
//!    Real user flow: agent breaks task into 3 items, marks one
//!    in_progress. The JSON on disk must match.
//! 2. **All completed** — the tool WIPES the file to an empty array
//!    `[]` instead of persisting the completed list. This is the
//!    "clear-when-done" semantic that CC's TodoWrite ships too, and is
//!    invisible from the tool's output (it looks like a normal write).
//!    Regression pattern: a well-intentioned refactor swaps this to
//!    "persist the completed list" and users end up with growing
//!    todo files.
//!
//! ```bash
//! cargo test --test pty_todo_write                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_todo_write  # real API
//! ```

mod common;

use std::fs;
use std::path::PathBuf;

use common::TestEnv;
use serde_json::Value;

fn todo_store(env: &TestEnv) -> PathBuf {
    env.workspace_root().join(".sudocode-todos.json")
}

fn read_todos(path: &std::path::Path) -> Vec<Value> {
    if !path.exists() {
        return vec![];
    }
    let text = fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return vec![];
    }
    serde_json::from_str::<Vec<Value>>(&text).unwrap_or_default()
}

// ──────────────────────────────────────────────────────────────────────
// 1. Pending list — verbatim persistence
// ──────────────────────────────────────────────────────────────────────

/// Agent writes 3 todos (mix of pending + in_progress). Store must
/// contain exactly those 3 items with the exact status strings so the
/// UI/next agent turn can render them correctly.
#[test]
fn todo_write_pending_list_persists_verbatim() {
    let env = TestEnv::new("todo-write-pending");

    assert!(
        !todo_store(&env).exists(),
        "fresh workspace should not have .sudocode-todos.json"
    );

    let prompt = env.prompt(
        "Please write a 3-item todo list using the TodoWrite tool. Do not describe it; just call the tool.",
        "todo_write_pending_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TodoWrite",
        &prompt,
    ]);

    sess.expect("TodoWrite")
        .expect("model must invoke TodoWrite (agent trigger)");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "todo_write turn should exit 0; got {exit}");

    let todos = read_todos(&todo_store(&env));
    assert_eq!(
        todos.len(),
        3,
        "store must contain exactly 3 items after the pending list write; got: {todos:?}"
    );
    // Every item must carry a valid status enum plus the required schema fields.
    // We intentionally do NOT require a specific status mix: the prompt asks only
    // for "a 3-item todo list", so a fresh all-`pending` list is correct model
    // behavior (and matches this test's name). The regression guard is that
    // status/content/activeForm survive serialization — not which status the
    // model happened to choose.
    const VALID_STATUSES: [&str; 3] = ["pending", "in_progress", "completed"];
    for (i, item) in todos.iter().enumerate() {
        let status = item.get("status").and_then(|v| v.as_str());
        assert!(
            status.is_some_and(|s| VALID_STATUSES.contains(&s)),
            "todo[{i}] has a missing/invalid status: {item}"
        );
        assert!(
            item.get("content").and_then(|v| v.as_str()).is_some(),
            "todo[{i}] missing content: {item}"
        );
        assert!(
            item.get("activeForm").and_then(|v| v.as_str()).is_some(),
            "todo[{i}] missing activeForm: {item}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. All-completed list — store gets wiped, NOT persisted
// ──────────────────────────────────────────────────────────────────────

/// Agent writes 3 completed items. TodoWrite's semantic is "everything
/// done → clear the store" (matches CC's TodoWrite). The regression
/// worth guarding: a refactor that swaps this to "persist the completed
/// list" produces growing todo files and stale progress state.
#[test]
fn todo_write_all_completed_wipes_store_to_empty_array() {
    let env = TestEnv::new("todo-write-all-completed");

    // Pre-seed a non-trivial list so we can distinguish "wipe" from
    // "no-op". Without this, both behaviours would look the same on disk.
    let seed = serde_json::json!([
        {"content": "old task 1", "activeForm": "old task 1", "status": "pending"},
        {"content": "old task 2", "activeForm": "old task 2", "status": "in_progress"},
    ]);
    fs::write(
        todo_store(&env),
        serde_json::to_string_pretty(&seed).unwrap(),
    )
    .expect("seed .sudocode-todos.json");

    let prompt = env.prompt(
        "Please mark all 3 items complete by calling the TodoWrite tool with statuses set to \"completed\". Do not describe it; just call the tool.",
        "todo_write_all_completed_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TodoWrite",
        &prompt,
    ]);

    sess.expect("TodoWrite")
        .expect("model must invoke TodoWrite");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);

    let todos = read_todos(&todo_store(&env));
    assert_eq!(
        todos.len(),
        0,
        "all-completed write must WIPE the store to an empty array (CC parity), not persist the completed list; got: {todos:?}"
    );
    // File itself should still exist as `[]` — deleting the file would
    // be a separate regression (subsequent reads would treat it as
    // "no prior todos exist" which is subtly different UX).
    assert!(
        todo_store(&env).exists(),
        "store file should still exist after wipe (holding an empty array, not deleted)"
    );
}
