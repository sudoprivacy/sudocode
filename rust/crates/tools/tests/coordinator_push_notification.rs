//! Integration test for the coordinator push-notification hook in
//! `persist_agent_terminal_state_with_telemetry`.
//!
//! Locks in the pipeline:
//!
//!   persist_terminal (under coord mode)
//!     -> render_manifest_task_notification (SSOT XML shape)
//!     -> runtime::coordinator_notification::emit
//!     -> `<workspace>/.sudocode-inbox/coordinator.jsonl` grows
//!     -> runtime::coordinator_notification::drain returns it
//!
//! With coord mode OFF, emit is a no-op and the mailbox file must
//! not appear — protects non-coordinator sessions from surprise
//! disk artifacts.

use std::path::PathBuf;

use runtime::agent_mailbox;
use runtime::coordinator_mode::COORDINATOR_ENV_VAR;
use runtime::coordinator_notification::{self, COORDINATOR_INBOX_RECIPIENT};
use tools::testing::{persist_terminal_with_telemetry_for_test, seed_agent_manifest_for_test};

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn unique_workspace(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "sudocode-coord-push-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir");
    path
}

#[test]
fn coord_mode_on_persist_terminal_pushes_task_notification_to_inbox() {
    let _g = env_lock();
    std::env::set_var(COORDINATOR_ENV_VAR, "1");

    let ws = unique_workspace("push-on");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-push-target");
    // Persist happens with `current_dir` as its workspace root — the
    // coordinator_notification module reads
    // `.sudocode-inbox/coordinator.jsonl` from cwd. Change cwd for
    // the duration of the persist call so the test's temp dir wins.
    let prior_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&ws).expect("chdir into ws");

    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "completed",
        Some("all done"),
        None,
        Some(tools::testing::AgentRunTelemetryView {
            total_tokens: 1234,
            tool_uses: 5,
        }),
    )
    .expect("persist ok");

    // Drain from the SAME workspace and verify one XML block landed.
    let batch = coordinator_notification::drain(&ws).expect("drain ok");
    assert_eq!(batch.len(), 1, "one task-notification emitted");
    let xml = &batch[0];
    assert!(xml.starts_with("<task-notification>"));
    assert!(xml.contains("<task-id>agent-push-target</task-id>"));
    assert!(xml.contains("<status>completed</status>"));
    // Telemetry threaded through: usage sub-tags must reflect the
    // values we recorded via persist_terminal_with_telemetry.
    assert!(xml.contains("<total_tokens>1234</total_tokens>"));
    assert!(xml.contains("<tool_uses>5</tool_uses>"));

    // Restore cwd + env before returning.
    std::env::set_current_dir(prior_cwd).ok();
    std::env::remove_var(COORDINATOR_ENV_VAR);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn coord_mode_off_persist_terminal_does_not_touch_inbox() {
    let _g = env_lock();
    std::env::remove_var(COORDINATOR_ENV_VAR);

    let ws = unique_workspace("push-off");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-push-noop");
    let prior_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&ws).expect("chdir into ws");

    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "completed",
        Some("done quietly"),
        None,
        None,
    )
    .expect("persist ok");

    let inbox = agent_mailbox::mailbox_path(&ws, COORDINATOR_INBOX_RECIPIENT);
    assert!(
        !inbox.exists(),
        "coord mode OFF must not create the coordinator inbox file"
    );
    let batch = coordinator_notification::drain(&ws).unwrap_or_default();
    assert!(batch.is_empty());

    std::env::set_current_dir(prior_cwd).ok();
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn two_subagents_completing_produce_two_task_notifications_in_fifo_order() {
    let _g = env_lock();
    std::env::set_var(COORDINATOR_ENV_VAR, "1");

    let ws = unique_workspace("push-fifo");
    let m1 = seed_agent_manifest_for_test(&ws, "agent-first");
    let m2 = seed_agent_manifest_for_test(&ws, "agent-second");
    let prior_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&ws).expect("chdir");

    persist_terminal_with_telemetry_for_test(&m1, "completed", Some("r1"), None, None).unwrap();
    persist_terminal_with_telemetry_for_test(&m2, "failed", None, Some("boom".into()), None)
        .unwrap();

    let batch = coordinator_notification::drain(&ws).expect("drain ok");
    assert_eq!(batch.len(), 2);
    assert!(
        batch[0].contains("agent-first"),
        "first-completed comes first"
    );
    assert!(
        batch[1].contains("agent-second"),
        "second-completed comes second"
    );
    // Second batch is empty — offset advanced.
    assert!(coordinator_notification::drain(&ws).unwrap().is_empty());

    std::env::set_current_dir(prior_cwd).ok();
    std::env::remove_var(COORDINATOR_ENV_VAR);
    let _ = std::fs::remove_dir_all(&ws);
}
