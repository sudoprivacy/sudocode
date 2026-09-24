//! Live PTY test: `write_plan` "Clear context & execute" is a human-in-the-loop
//! compaction — it resets the session but carries the todo list forward, the
//! same light action auto-compaction uses. Also covers the approval indication.
//!
//! Seeds a todo store, drives `write_plan` → choose option 1 (clear + execute),
//! and asserts the recursive execute turn runs AND the seeded todo content is
//! re-injected into the fresh session (todo panel / continuity block present).
//!
//! Live-only: the recursive execute turn needs a real model; the mock can't
//! answer the cleared, marker-less prompt (same reason the existing clear-context
//! test is live-gated).
//!
//! ```bash
//! SCODE_TEST_BACKEND=live cargo test --test pty_plan_clear_context_carries_todos
//! ```

mod common;

use std::time::Duration;

use common::{TestEnv, LIVE_TURN_BUDGET};

#[test]
fn clear_context_carries_todos_forward() {
    let env = TestEnv::new("plan-clear-todos");
    if env.is_mock() {
        eprintln!("SKIP: recursive clear-context execute needs a real model");
        return;
    }

    // Seed a todo store the CLI will read (same env var the todo tests use).
    let todo_store = env.workspace_root().join("todos.json");
    let sentinel = "carryforwardtodo";
    std::fs::write(
        &todo_store,
        format!(
            r#"[{{"content":"{sentinel}","status":"in_progress","activeForm":"doing {sentinel}"}}]"#
        ),
    )
    .expect("seed todo store");

    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan,read_file,glob_search",
        ],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_TODO_STORE", todo_store.to_str().unwrap()),
        ],
    );
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call write_plan with a short markdown plan as `content`. Just call the tool.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(Duration::from_secs(30));
    if sess.expect("Choose an action").is_err() {
        eprintln!("SKIP: live model did not call write_plan");
        return;
    }

    // Option 1: clear context & execute.
    sess.send("1\r").expect("send choice 1");

    // After the clear, the recursive execute turn runs with the approved plan +
    // the carried-forward todo. The sentinel from the seeded store must reappear
    // in the fresh session (re-injected continuity block), proving todos survived
    // the clear. Generous budget: clear + a real execute turn.
    sess.set_default_timeout(LIVE_TURN_BUDGET);
    sess.expect(sentinel)
        .expect("seeded todo must be carried into the cleared session");

    // Best-effort teardown (multi-turn /exit sync is non-deterministic over PTY).
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(30));
    let _ = sess.expect_eof();
}
