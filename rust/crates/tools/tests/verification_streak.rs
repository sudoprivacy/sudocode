//! Integration tests for the auto-verification streak nudge
//! (`runtime::verification_watcher` + wiring in `run_task_update`
//! and `prepare_agent_job`).
//!
//! ## What this file locks in (long-workflow, data-flow chained)
//!
//! Each test represents a real coordinator/model behaviour trace.
//! The counter is process-global so tests serialise on a mutex —
//! parallel writes would race the atomic and confuse the assertions.
//!
//! 1. **Streak → nudge → reset → streak → nudge** — three
//!    TaskUpdate(status=completed) calls each mark a new task done.
//!    After the third, the tool result MUST include the
//!    `<system-reminder>` nudge. Following that, a fresh streak
//!    fires the nudge AGAIN because it was consumed after firing.
//! 2. **Verification spawn resets the counter mid-streak** —
//!    accumulate 2 completions, dispatch an
//!    `Agent(subagent_type="Verification")`, then accumulate 2 more:
//!    total is 4 but no nudge fires because the reset zeroed us.
//! 3. **Env override disables the feature** — with threshold `0`
//!    even a 10-completion streak yields NO nudge.
//! 4. **Same-content re-completion is NOT re-counted** — completing
//!    a task that was already completed must NOT re-increment.
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

fn temp_task_store(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "sudocode-task-store-{label}-{nanos}-{}",
        std::process::id()
    ))
}

/// Reset process-global state that survives across #[test] runs.
fn reset_all() {
    verification_watcher::reset_all_for_test();
    std::env::remove_var(VERIFICATION_STREAK_ENV);
}

/// Helper: create a task via the tool dispatch and return its task_id.
fn create_task(subject: &str) -> String {
    let result = tools::execute_tool(
        "TaskCreate",
        &serde_json::json!({
            "subject": subject,
            "description": format!("Test task: {subject}"),
        }),
    )
    .expect("TaskCreate should succeed");
    let output: serde_json::Value = serde_json::from_str(&result).expect("valid json");
    output["task_id"]
        .as_str()
        .expect("task_id should be a string")
        .to_string()
}

/// Helper: complete a task via TaskUpdate and return the JSON output.
fn complete_task(task_id: &str) -> String {
    tools::execute_tool(
        "TaskUpdate",
        &serde_json::json!({
            "taskId": task_id,
            "status": "completed",
        }),
    )
    .expect("TaskUpdate should succeed")
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
    let store = temp_task_store("streak-nudge-restreak");
    std::env::set_var("SUDOCODE_TASK_STORE", store.to_str().unwrap());

    // Create 6 tasks — we'll complete them in two batches of 3.
    let ids: Vec<String> = (b'a'..=b'f')
        .map(|c| create_task(&format!("task_{}", c as char)))
        .collect();

    // Complete first 3 — streak should reach threshold on the third.
    let r1 = complete_task(&ids[0]);
    assert!(!r1.contains("system-reminder"), "no nudge at 1 completion");

    let r2 = complete_task(&ids[1]);
    assert!(!r2.contains("system-reminder"), "no nudge at 2 completions");

    let r3 = complete_task(&ids[2]);
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
    let r4 = complete_task(&ids[3]);
    assert!(
        !r4.contains("<system-reminder>"),
        "streak reset after nudge"
    );
    let r5 = complete_task(&ids[4]);
    assert!(!r5.contains("<system-reminder>"), "still under threshold");

    // Third fresh completion -> streak 3 again -> nudge fires again.
    let r6 = complete_task(&ids[5]);
    assert!(
        r6.contains("<system-reminder>"),
        "second streak MUST re-fire nudge"
    );

    std::env::remove_var("SUDOCODE_TASK_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}

#[test]
fn dispatching_verification_agent_resets_streak_mid_way() {
    let _guard = env_lock();
    reset_all();
    let store = temp_task_store("verif-mid-reset");
    std::env::set_var("SUDOCODE_TASK_STORE", store.to_str().unwrap());

    // Two completions → streak = 2.
    let id1 = create_task("x");
    let id2 = create_task("y");
    let r1 = complete_task(&id1);
    assert!(!r1.contains("<system-reminder>"));
    let r2 = complete_task(&id2);
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
    let id3 = create_task("z");
    let id4 = create_task("w");
    let r3 = complete_task(&id3);
    assert!(
        !r3.contains("<system-reminder>"),
        "streak reset means we should NOT nudge yet — got: {r3}"
    );
    let r4 = complete_task(&id4);
    assert!(!r4.contains("<system-reminder>"));

    // Third fresh completion after reset -> nudge fires.
    let id5 = create_task("v");
    let r5 = complete_task(&id5);
    assert!(r5.contains("<system-reminder>"), "post-reset streak nudges");

    std::env::remove_var("SUDOCODE_TASK_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}

#[test]
fn env_override_zero_disables_nudge_entirely() {
    let _guard = env_lock();
    reset_all();
    std::env::set_var(VERIFICATION_STREAK_ENV, "0");
    let store = temp_task_store("streak-disabled");
    std::env::set_var("SUDOCODE_TASK_STORE", store.to_str().unwrap());

    for i in 0..10 {
        let id = create_task(&format!("t{i}"));
        let out = complete_task(&id);
        assert!(
            !out.contains("<system-reminder>"),
            "disabled feature MUST never nudge (iter {i})"
        );
    }

    std::env::remove_var("SUDOCODE_TASK_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}

#[test]
fn already_completed_task_does_not_re_increment_on_second_update() {
    let _guard = env_lock();
    reset_all();
    let store = temp_task_store("no-recount");
    std::env::set_var("SUDOCODE_TASK_STORE", store.to_str().unwrap());

    // Create 3 tasks and complete them all → nudge fires (streak=3).
    let id1 = create_task("a");
    let id2 = create_task("b");
    let id3 = create_task("c");
    complete_task(&id1);
    complete_task(&id2);
    let r3 = complete_task(&id3);
    assert!(r3.contains("<system-reminder>"));

    // Streak reset. Now re-complete the same task (already completed).
    // Delta must be 0 — same subject string, previously completed.
    let r4 = complete_task(&id1);
    assert_eq!(
        verification_watcher::current_streak(),
        0,
        "re-completing same task must NOT re-increment"
    );
    assert!(!r4.contains("<system-reminder>"), "no re-fire");

    std::env::remove_var("SUDOCODE_TASK_STORE");
    let _ = std::fs::remove_file(&store);
    reset_all();
}
