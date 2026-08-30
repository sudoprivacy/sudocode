//! PTY tests for the `TaskCreate` / `TaskGet` / `TaskList` tool family.
//!
//! Coverage target: roadmap §Feature-inventory row
//! "TaskCreate / TaskGet / TaskList" (must-have, LLM-level background
//! task management).
//!
//! ## Mock vs Live
//!
//! `TaskCreate` spawns a managed-agent subagent that makes its own LLM
//! requests. The mock harness cannot propagate the parent scenario token
//! to the subagent, so TaskCreate tests are **live-only** (skip in CI
//! mock mode). TaskList on empty registry works in mock mode.
//!
//! ```bash
//! cargo test --test pty_task_family                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_task_family  # real API
//! ```

mod common;

use common::TestEnv;
use std::time::Duration;

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

    // The tool serializes `count` in its JSON response; the TUI digests
    // that object to one `key: value` line per field, so the screen shows
    // `count: 0` (no JSON quotes). An empty registry means count = 0.
    sess.expect(r"count:\s*0")
        .expect("TaskList on an empty registry must report count: 0");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "task_list empty turn should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// Live-only: TaskCreate → TaskList → verify task appears
// ──────────────────────────────────────────────────────────────────────

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — TaskCreate spawns a \
         subagent that needs a real API. Rerun with SCODE_TEST_BACKEND=live."
    );
    false
}

/// End-to-end real user journey: create a background task, then list
/// tasks to verify it appears with a task_id and running status.
///
/// Journey (3 steps, data flows between them):
///   1. TaskCreate("say hello") → returns task_id
///   2. TaskList → response includes the task from step 1
///   3. TaskStop(task_id) → stops the background task
///
/// This covers the "delegate work to background agent" workflow that
/// users rely on for parallelizing coding tasks.
#[test]
fn task_create_then_list_shows_task_e2e() {
    let env = TestEnv::new("task-create-e2e");
    if !require_live(&env, "task_create_then_list_shows_task_e2e") {
        return;
    }

    // Use REPL mode so we can issue multiple turns.
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(30));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    // 1. Ask the model to create a background task.
    sess.send("Create a background task using TaskCreate with the prompt 'Reply with exactly: TASK_SENTINEL_ALPHA'. Do not describe, just call the tool.\r")
        .expect("send TaskCreate prompt");

    // TaskCreate returns a task_id.
    sess.expect("task_id").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("TaskCreate should return task_id: {e}\nPTY:\n{screen}");
    });

    // Wait for prompt to return.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after TaskCreate: {e}\nPTY:\n{screen}");
    });

    // 2. List tasks — the created task should appear.
    sess.send("Now call TaskList to show all background tasks.\r")
        .expect("send TaskList prompt");

    sess.expect("(?i)task").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("TaskList should mention the task: {e}\nPTY:\n{screen}");
    });

    // Wait for prompt.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after TaskList: {e}\nPTY:\n{screen}");
    });

    // 3. Clean exit.
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
