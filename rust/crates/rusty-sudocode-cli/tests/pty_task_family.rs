//! PTY tests for the `TaskCreate` / `TaskGet` / `TaskList` tool family.
//!
//! Coverage target: roadmap §Feature-inventory row
//! "TaskCreate / TaskGet / TaskList" (must-have, LLM-level background
//! task management, "strict CC parity"). Before this file: 0 PTY tests
//! → row marked "Gap". After: covered by 1 direct read-side test.
//!
//! ## Why only TaskList lives in this PTY file
//!
//! `TaskCreate` (via `tools::run_task_create` → `TaskRegistry::create`)
//! goes on to schedule a managed-agent subagent that runs its own LLM
//! turns against the parent process's provider. In the mock harness
//! the parent's `MessageRequest` carries a `PARITY_SCENARIO:` token so
//! the mock knows which reply to return — the subagent's follow-up
//! requests carry no scenario token and the mock rejects them with
//! `missing parity scenario`, leaving the CLI waiting on subagent
//! completion. The behavior is correct in live mode (real API answers
//! the subagent) but not testable at the PTY layer with the current
//! mock without a large scenario-inheritance refactor.
//!
//! Coverage that already exists elsewhere:
//! - `TaskRegistry::create` shape + status transitions → unit-covered
//!   in `runtime::task_registry::tests` (create/get/list/stop).
//! - Task-packet validation → unit-covered in the same file.
//!
//! Left for a follow-up: extend the mock to propagate the parent
//! turn's scenario to subagent requests. Then re-enable
//! `task_create_returns_task_id_and_exits_zero` +
//! `task_create_then_list_shows_created_task_within_same_process`
//! from the git history of this file (both were live-run today —
//! see PR description).
//!
//! ```bash
//! cargo test --test pty_task_family                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_task_family  # real API
//! ```

mod common;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// TaskList on empty registry — count=0, tasks=[]
// ──────────────────────────────────────────────────────────────────────

/// A fresh scode process has an empty task registry. TaskList must
/// return a JSON payload with `count: 0` and the CLI must exit 0.
/// Regression sentinel against a change that makes TaskList crash on
/// empty state or return a null instead of the count field.
///
/// TaskList is a read-only tool — no subagent spawn, no mock-harness
/// interaction issues (unlike TaskCreate).
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

    sess.expect("TaskList")
        .expect("model must invoke TaskList (agent trigger)");

    // The tool serializes the response as a JSON object with a `count`
    // field. An empty registry means count = 0.
    sess.expect(r#""count":\s*0"#)
        .expect("TaskList on an empty registry must report count: 0");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "task_list empty turn should exit 0; got {exit}");
}
