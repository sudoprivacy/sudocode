//! Integration tests for the auto-verification streak nudge
//! (`runtime::verification_watcher` + wiring in `execute_todo_write`
//! and `prepare_agent_job`).
//!
//! ## What this file locks in (long-workflow, data-flow chained)
//!
//! Each test represents a real coordinator/model behaviour trace.
//! The counter is process-global so tests serialise on a mutex —
//! parallel writes would race the atomic and confuse the assertions.
//!
//! 1. **Streak → nudge → reset → streak → nudge** — three
//!    TodoWrite calls each mark a new todo Completed. After the
//!    third, the tool result MUST include the `<system-reminder>`
//!    nudge. Following that, a fresh streak (three more) fires the
//!    nudge AGAIN because it was consumed after firing.
//! 2. **Verification spawn resets the counter mid-streak** —
//!    accumulate 2 completions, dispatch an
//!    `Agent(subagent_type="Verification")`, then accumulate 2 more:
//!    total is 4 but no nudge fires because the reset zeroed us.
//! 3. **Env override disables the feature** — with threshold `0`
//!    even a 10-completion streak yields NO nudge.
//! 4. **Same-content re-completion is NOT re-counted** — a
//!    TodoWrite that persists an already-completed todo through a
//!    subsequent partial write must NOT re-increment.
//!
//! Data-flow contract: each scenario carries state THROUGH tests
//! via the `runtime::verification_watcher` counter — reading its
//! post-conditions is what proves the wiring works.

use runtime::verification_watcher::{
    self, streak_threshold, DEFAULT_VERIFICATION_STREAK_THRESHOLD, VERIFICATION_STREAK_ENV,
};
use tools::testing::{execute_todo_write_for_test, prepare_agent_job_for_test};

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn temp_todo_store(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "sudocode-todo-store-{label}-{nanos}-{}",
        std::process::id()
    ))
}

/// Reset process-global state that survives across #[test] runs.
fn reset_all() {
    verification_watcher::reset_all_for_test();
    std::env::remove_var(VERIFICATION_STREAK_ENV);
}

/// Drive one TodoWrite call with a fresh in-process store — returns
/// the tool's JSON output so tests can grep for the nudge substring.
fn todo_write_completing_one_new(store_env: &str, all_todos_after: &[(&str, &str)]) -> String {
    std::env::set_var("SUDOCODE_TODO_STORE", store_env);
    let todos_json = serde_json::to_string(
        &all_todos_after
            .iter()
            .map(|(content, status)| {
                serde_json::json!({
                    "content": content,
                    "activeForm": *content,
                    "status": status,
                })
            })
            .collect::<Vec<_>>(),
    )
    .expect("todos json");
    let input_json = format!(r#"{{"todos": {todos_json}}}"#);
    execute_todo_write_for_test(&input_json).expect("todo_write returns Ok")
}

#[test]
fn threshold_default_is_three() {
    let _guard = env_lock();
    reset_all();
    assert_eq!(
        streak_threshold(),
        Some(DEFAULT_VERIFICATION_STREAK_THRESHOLD)
    );
    reset_all();
}

#[test]
fn streak_then_nudge_then_reset_then_second_streak_fires_again() {
    let _guard = env_lock();
    reset_all();
    let store = temp_todo_store("streak-nudge-restreak");

    // Turn 1: mark "a" completed. old=[], new=[a completed] -> 1 delta.
    let r1 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("a", "completed"), ("b", "pending"), ("c", "pending")],
    );
    assert!(!r1.contains("system-reminder"), "no nudge at 1 completion");

    // Turn 2: mark "b" completed. old had "a" completed. delta = 1.
    let r2 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("a", "completed"), ("b", "completed"), ("c", "pending")],
    );
    assert!(!r2.contains("system-reminder"), "no nudge at 2 completions");

    // Turn 3: mark "c" completed. delta = 1. Streak now = 3 → NUDGE.
    let r3 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("a", "completed"), ("b", "completed"), ("c", "completed")],
    );
    assert!(
        r3.contains("<system-reminder>"),
        "3-completion streak MUST emit nudge; got: {r3}"
    );
    assert!(
        r3.contains("Agent(subagent_type=\\\"Verification\\\""),
        "nudge text must name the Verification agent (JSON-escaped in the payload)"
    );
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "should_nudge_and_consume MUST reset counter to 0"
    );

    // After reset, 2 more completions still under threshold → no nudge.
    let r4 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[
            ("a", "completed"),
            ("b", "completed"),
            ("c", "completed"),
            ("d", "completed"),
        ],
    );
    assert!(
        !r4.contains("<system-reminder>"),
        "streak reset after nudge"
    );
    let r5 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[
            ("a", "completed"),
            ("b", "completed"),
            ("c", "completed"),
            ("d", "completed"),
            ("e", "completed"),
        ],
    );
    assert!(!r5.contains("<system-reminder>"), "still under threshold");

    // Third fresh completion -> streak 3 again -> nudge fires again.
    let r6 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[
            ("a", "completed"),
            ("b", "completed"),
            ("c", "completed"),
            ("d", "completed"),
            ("e", "completed"),
            ("f", "completed"),
        ],
    );
    assert!(
        r6.contains("<system-reminder>"),
        "second streak MUST re-fire nudge"
    );

    std::env::remove_var("SUDOCODE_TODO_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}

#[test]
fn dispatching_verification_agent_resets_streak_mid_way() {
    let _guard = env_lock();
    reset_all();
    let store = temp_todo_store("verif-mid-reset");

    // Two completions → streak = 2.
    let r1 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("x", "completed"), ("y", "pending")],
    );
    assert!(!r1.contains("<system-reminder>"));
    let r2 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("x", "completed"), ("y", "completed")],
    );
    assert!(!r2.contains("<system-reminder>"));
    assert_eq!(verification_watcher::current_streak(), 2);

    // Model dispatches a Verification sub-agent — even if the spawn
    // itself fails (no real workspace here), the reset MUST fire
    // because the intent alone is what we're counting.
    let _ = prepare_agent_job_for_test("Verification", "Verify the current work.");
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "Verification dispatch MUST reset streak counter"
    );

    // 2 more completions AFTER the reset → still under threshold → no nudge.
    let r3 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("x", "completed"), ("y", "completed"), ("z", "completed")],
    );
    assert!(
        !r3.contains("<system-reminder>"),
        "streak reset means we should NOT nudge yet — got: {r3}"
    );
    let r4 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[
            ("x", "completed"),
            ("y", "completed"),
            ("z", "completed"),
            ("w", "completed"),
        ],
    );
    assert!(!r4.contains("<system-reminder>"));

    // Third fresh completion after reset -> nudge fires.
    let r5 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[
            ("x", "completed"),
            ("y", "completed"),
            ("z", "completed"),
            ("w", "completed"),
            ("v", "completed"),
        ],
    );
    assert!(r5.contains("<system-reminder>"), "post-reset streak nudges");

    std::env::remove_var("SUDOCODE_TODO_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}

#[test]
fn env_override_zero_disables_nudge_entirely() {
    let _guard = env_lock();
    reset_all();
    std::env::set_var(VERIFICATION_STREAK_ENV, "0");
    let store = temp_todo_store("streak-disabled");

    for i in 0..10 {
        let mut todos = Vec::new();
        for j in 0..=i {
            todos.push((format!("t{j}"), "completed".to_string()));
        }
        let todos_ref: Vec<(&str, &str)> = todos
            .iter()
            .map(|(c, s)| (c.as_str(), s.as_str()))
            .collect();
        let out = todo_write_completing_one_new(store.to_str().unwrap(), &todos_ref);
        assert!(
            !out.contains("<system-reminder>"),
            "disabled feature MUST never nudge (iter {i})"
        );
    }

    std::env::remove_var("SUDOCODE_TODO_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}

#[test]
fn already_completed_todos_do_not_re_increment_on_partial_write() {
    let _guard = env_lock();
    reset_all();
    let store = temp_todo_store("no-recount");

    // Turn 1: 3 todos all completed at once → delta = 3 → nudge!
    // (This mirrors the "explicit completion cluster" path.)
    let r1 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("a", "completed"), ("b", "completed"), ("c", "completed")],
    );
    assert!(r1.contains("<system-reminder>"));

    // Streak reset. Now the store has all 3 completed.
    // Turn 2: WRITE THE SAME 3 completed todos again (no change).
    // Delta must be 0 — same content strings, previously completed.
    let r2 = todo_write_completing_one_new(
        store.to_str().unwrap(),
        &[("a", "completed"), ("b", "completed"), ("c", "completed")],
    );
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "no-change TodoWrite must NOT re-increment"
    );
    assert!(!r2.contains("<system-reminder>"), "no re-fire");

    std::env::remove_var("SUDOCODE_TODO_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}
