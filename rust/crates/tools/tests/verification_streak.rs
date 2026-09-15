//! Integration tests for the auto-verification streak nudge
//! (`runtime::verification_watcher` + wiring in `run_todo_write`
//! and `prepare_agent_job`).
//!
//! ## What this file locks in (long-workflow, data-flow chained)
//!
//! Each test represents a real coordinator/model behaviour trace.
//! The counter is process-global so tests serialise on a mutex —
//! parallel writes would race the atomic and confuse the assertions.
//!
//! 1. **Streak → nudge → reset → streak → nudge** — each TodoWrite
//!    marks one more todo `completed` (whole-list replace). After the
//!    third distinct completion, the tool result MUST include the
//!    `<system-reminder>` nudge. Following that, a fresh streak fires
//!    the nudge AGAIN because it was consumed after firing.
//! 2. **Verification spawn resets the counter mid-streak** —
//!    accumulate 2 completions, dispatch an
//!    `Agent(subagent_type="Verification")`, then accumulate 2 more:
//!    total is 4 but no nudge fires because the reset zeroed us.
//! 3. **Env override disables the feature** — with threshold `0`
//!    even a 10-completion streak yields NO nudge.
//! 4. **Same-content re-completion is NOT re-counted** — re-sending a
//!    list whose completed items are unchanged must NOT re-increment.
//!
//! Data-flow contract: each scenario carries state THROUGH tests
//! via the `runtime::verification_watcher` counter — reading its
//! post-conditions is what proves the wiring works.

use runtime::verification_watcher::{
    self, streak_threshold, DEFAULT_VERIFICATION_STREAK_THRESHOLD, VERIFICATION_STREAK_ENV,
};
use tools::testing::prepare_agent_job_for_test;

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

/// A tracked todo list the test mutates and re-sends whole on each write —
/// exactly how a model drives `TodoWrite`. `mark_completed` flips one item to
/// `completed` and returns the tool's JSON output for the resulting write.
struct TodoList {
    contents: Vec<String>,
    completed: std::collections::BTreeSet<String>,
}

impl TodoList {
    fn new(contents: &[&str]) -> Self {
        Self {
            contents: contents.iter().map(|s| (*s).to_string()).collect(),
            completed: std::collections::BTreeSet::new(),
        }
    }

    /// Re-send the whole list, marking `content` completed, and return the
    /// tool's JSON output string.
    fn mark_completed(&mut self, content: &str) -> String {
        self.completed.insert(content.to_string());
        self.write()
    }

    fn write(&self) -> String {
        let todos: Vec<serde_json::Value> = self
            .contents
            .iter()
            .map(|c| {
                let status = if self.completed.contains(c) {
                    "completed"
                } else {
                    "pending"
                };
                serde_json::json!({
                    "content": c,
                    "status": status,
                    "activeForm": format!("Doing {c}"),
                })
            })
            .collect();
        tools::execute_tool("TodoWrite", &serde_json::json!({ "todos": todos }))
            .expect("TodoWrite should succeed")
    }
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
    std::env::set_var("SUDOCODE_TODO_STORE", store.to_str().unwrap());

    // One list of 6 todos — we'll complete them in two batches of 3.
    let mut list = TodoList::new(&["a", "b", "c", "d", "e", "f"]);

    // Complete first 3 — streak should reach threshold on the third.
    let r1 = list.mark_completed("a");
    assert!(!r1.contains("system-reminder"), "no nudge at 1 completion");

    let r2 = list.mark_completed("b");
    assert!(!r2.contains("system-reminder"), "no nudge at 2 completions");

    let r3 = list.mark_completed("c");
    assert!(
        r3.contains("<system-reminder>"),
        "3-completion streak MUST emit nudge; got: {r3}"
    );
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "should_nudge_and_consume MUST reset counter to 0"
    );

    // After reset, 2 more completions still under threshold → no nudge.
    let r4 = list.mark_completed("d");
    assert!(
        !r4.contains("<system-reminder>"),
        "streak reset after nudge"
    );
    let r5 = list.mark_completed("e");
    assert!(!r5.contains("<system-reminder>"), "still under threshold");

    // Third fresh completion -> streak 3 again -> nudge fires again.
    let r6 = list.mark_completed("f");
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
    std::env::set_var("SUDOCODE_TODO_STORE", store.to_str().unwrap());

    let mut list = TodoList::new(&["x", "y", "z", "w", "v"]);

    // Two completions → streak = 2.
    let r1 = list.mark_completed("x");
    assert!(!r1.contains("<system-reminder>"));
    let r2 = list.mark_completed("y");
    assert!(!r2.contains("<system-reminder>"));
    assert_eq!(verification_watcher::current_streak(), 2);

    // Model dispatches a Verification sub-agent — the reset MUST fire.
    let _ = prepare_agent_job_for_test("Verification", "Verify the current work.");
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "Verification dispatch MUST reset streak counter"
    );

    // 2 more completions AFTER the reset → still under threshold.
    let r3 = list.mark_completed("z");
    assert!(
        !r3.contains("<system-reminder>"),
        "streak reset means we should NOT nudge yet — got: {r3}"
    );
    let r4 = list.mark_completed("w");
    assert!(!r4.contains("<system-reminder>"));

    // Third fresh completion after reset -> nudge fires.
    let r5 = list.mark_completed("v");
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
    std::env::set_var("SUDOCODE_TODO_STORE", store.to_str().unwrap());

    let contents: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
    let refs: Vec<&str> = contents.iter().map(String::as_str).collect();
    let mut list = TodoList::new(&refs);
    for (i, c) in contents.iter().enumerate() {
        let out = list.mark_completed(c);
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
fn already_completed_todo_does_not_re_increment_on_second_write() {
    let _guard = env_lock();
    reset_all();
    let store = temp_todo_store("no-recount");
    std::env::set_var("SUDOCODE_TODO_STORE", store.to_str().unwrap());

    // Complete 3 distinct todos → nudge fires (streak=3).
    let mut list = TodoList::new(&["a", "b", "c"]);
    list.mark_completed("a");
    list.mark_completed("b");
    let r3 = list.mark_completed("c");
    assert!(r3.contains("<system-reminder>"));

    // Streak reset. Re-send the same list (a already completed). Its
    // completion was already counted, so the counter must NOT move.
    let r4 = list.write();
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "re-sending an already-completed todo must NOT re-increment"
    );
    assert!(!r4.contains("<system-reminder>"), "no re-fire");

    std::env::remove_var("SUDOCODE_TODO_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}
