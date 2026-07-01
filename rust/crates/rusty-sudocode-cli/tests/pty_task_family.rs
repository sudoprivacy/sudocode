//! PTY tests for the `TaskCreate` / `TaskGet` / `TaskList` tool family.
//!
//! Coverage target: roadmap §Feature-inventory row
//! "TaskCreate / TaskGet / TaskList" (must-have, LLM-level background
//! task management, "strict CC parity"). Before this file: 0 PTY tests
//! → row marked "Gap". After: 3 tests covering the shapes that catch
//! real regressions.
//!
//! ## What this covers (and what it deliberately doesn't)
//!
//! Task state lives in an in-process `TaskRegistry` (Arc<Mutex<...>>,
//! OnceLock singleton). That means:
//!
//! - Within a SINGLE scode process the tools share state — TaskList
//!   after TaskCreate sees the created task.
//! - Across processes state is lost. Test 3 exercises the
//!   within-process case via a multi-tool turn.
//!
//! What we deliberately DON'T test at the PTY layer:
//! - Task completion / status transitions. Background tasks may still
//!   be running when the parent CLI turn ends; asserting on
//!   `completed` from a PTY expect is inherently flaky. That
//!   assertion belongs in the runtime crate's `task_registry` unit
//!   layer (already covered in `runtime/src/task_registry.rs::tests`).
//! - Cross-process persistence. Not a feature of the current design.
//!
//! ```bash
//! cargo test --test pty_task_family                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_task_family  # real API
//! ```

mod common;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. TaskCreate — returns a task record, exits 0
// ──────────────────────────────────────────────────────────────────────

/// The agent invokes TaskCreate with a prompt + description. The tool
/// must succeed, print a JSON-shaped result containing at minimum a
/// task_id and a pending-status marker, and the CLI must exit 0.
///
/// Regression sentinel: a refactor that swaps the registry backend and
/// breaks TaskCreate's result serialization (or hangs the tool) fails
/// this test.
#[test]
fn task_create_returns_task_id_and_exits_zero() {
    let env = TestEnv::new("task-create-basic");

    let prompt = env.prompt(
        "Please create a background task by calling the TaskCreate tool. Give it a prompt and a description. Do not describe it; just call the tool.",
        "task_create_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TaskCreate",
        &prompt,
    ]);

    sess.expect("TaskCreate")
        .expect("model must invoke TaskCreate (agent trigger)");

    // The tool result contains "task_id" as a JSON key and Pending
    // status (either "Pending" if the registry serializes the enum
    // variant or "pending" if snake-cased). Match both.
    sess.expect(r#"task_id"#)
        .expect("TaskCreate result must include a task_id");
    sess.expect("(?i)(pending)")
        .expect("newly-created task must report a pending status");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "task_create turn should exit 0; got {exit}");

    // Mock-only: the harness should have captured at least one
    // /v1/messages request. If we captured 0, push_images or a
    // similar path silently ate the turn.
    if env.is_mock() {
        assert!(
            env.captured_message_count() >= 1,
            "expected ≥1 /v1/messages request; got 0"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. TaskList on empty registry — count=0, tasks=[]
// ──────────────────────────────────────────────────────────────────────

/// A fresh scode process has an empty task registry. TaskList must
/// return a JSON payload with `count: 0` (or an empty-looking marker)
/// and the CLI must exit 0. Regression sentinel against a change that
/// makes TaskList crash on empty state or return a null instead of
/// the count field.
#[test]
fn task_list_on_empty_registry_returns_zero_count() {
    let env = TestEnv::new("task-list-empty");

    let prompt = env.prompt(
        "Please list the background tasks by calling the TaskList tool. Do not describe it; just call the tool.",
        "task_list_empty_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TaskList",
        &prompt,
    ]);

    sess.expect("TaskList").expect("model must invoke TaskList");

    // The tool serializes the response as a JSON object with a `count`
    // field. An empty registry means count = 0.
    sess.expect(r#""count":\s*0"#)
        .expect("TaskList on an empty registry must report count: 0");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "task_list empty turn should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 3. TaskCreate + TaskList in one turn — list sees the created task
// ──────────────────────────────────────────────────────────────────────

/// Multi-tool turn: mock returns a paired TaskCreate + TaskList
/// invocation. Within the same scode process the OnceLock-backed
/// `TaskRegistry` is shared — TaskList must see the task TaskCreate
/// just inserted.
///
/// Regression sentinel against a change that accidentally makes
/// TaskCreate produce a task in a private/thread-local registry that
/// TaskList can't see (this exact class of bug shows up when the
/// registry is refactored from OnceLock to per-turn state).
#[test]
fn task_create_then_list_shows_created_task_within_same_process() {
    let env = TestEnv::new("task-create-then-list");

    let prompt = env.prompt(
        "Please create ONE background task using the TaskCreate tool AND then list the background tasks using the TaskList tool. Call both tools; do not describe them.",
        "task_create_then_list_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TaskCreate,TaskList",
        &prompt,
    ]);

    // Both tools should fire.
    sess.expect("TaskCreate")
        .expect("model must invoke TaskCreate (agent trigger 1)");
    sess.expect("TaskList")
        .expect("model must invoke TaskList (agent trigger 2)");

    // The list result must show count ≥ 1 — the just-created task.
    // Match either "count": 1 or a larger number (defensive against
    // concurrent test noise if the OnceLock leaks across tests in the
    // same binary — currently unlikely because each `env` gets its own
    // spawned process).
    sess.expect(r#""count":\s*[1-9]"#)
        .expect("TaskList after TaskCreate must report count ≥ 1 (in-process registry share)");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);

    if env.is_mock() {
        // Multi-tool turn = two /v1/messages requests: (1) initial
        // prompt returning tool_use for both, (2) tool_result follow-up
        // returning final text.
        assert!(
            env.captured_message_count() >= 2,
            "expected ≥2 /v1/messages requests for a multi-tool turn; got {}",
            env.captured_message_count()
        );
    }
}
