//! Integration test for the `Monitor` tool's lifecycle.
//!
//! `Monitor` deliberately owns no machinery of its own: it reuses the launcher
//! the `bash` tool's `run_in_background` path uses, so a watch and a background
//! command share one registry, one output owner and one cancellation contract.
//! What this test holds is that the reuse is real — a watch registers, captures
//! what the command printed, and reaches a terminal status the REPL can turn
//! into a notification.

use std::time::{Duration, Instant};

use runtime::background_tasks::{BackgroundTaskKind, BackgroundTasks};

fn wait_for<T>(budget: Duration, context: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting: {context}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_watch_registers_captures_output_and_reaches_a_terminal_status() {
    let ws = std::env::temp_dir().join(format!("scode-monitor-tool-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");
    let tasks = BackgroundTasks::default();

    let command = runtime::shell_command_in(&ws, "printf 'WATCHLINE\\n'; exit 3");
    let (id, output_path) = runtime::launch_background_command(
        command,
        runtime::subagent_completion::ShellKind::Bash,
        "printf 'WATCHLINE\\n'; exit 3",
        "watch probe",
        Some(&tasks),
    )
    .expect("watch should start");

    // Registered under the shared registry, titled for the panel and the
    // notification, and kinded as a terminal so the UI groups it correctly.
    let registered = tasks.get(&id).expect("watch must be registered");
    assert_eq!(registered.kind, BackgroundTaskKind::Terminal);
    assert_eq!(registered.title, "watch probe");
    assert_eq!(
        registered.output_path.as_deref(),
        Some(output_path.as_str())
    );

    // It reaches a terminal status carrying the real exit code — this is what
    // `background_completion` turns into a `<task-notification>`.
    let finished = wait_for(Duration::from_secs(20), "watch to finish", || {
        tasks.get(&id).filter(|task| !task.status.is_active())
    });
    assert_eq!(
        finished.exit_code,
        Some(3),
        "the watch must report the command's real exit status"
    );
    assert!(
        !finished.status.is_active(),
        "a finished watch must leave the active states"
    );

    // What the command printed is readable afterwards, both in the task's
    // bounded tail and on disk.
    let logged = std::fs::read_to_string(&output_path).expect("watch log");
    assert!(
        logged.contains("WATCHLINE"),
        "the watch's output must be captured; got {logged:?}"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// Stopping a watch must end it promptly, even while it is quiet — a watch that
/// only notices a stop on its next line of output would hang on `sleep`.
#[test]
fn stopping_a_quiet_watch_ends_it() {
    let ws = std::env::temp_dir().join(format!("scode-monitor-stop-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");
    let tasks = BackgroundTasks::default();

    let command = runtime::shell_command_in(&ws, "sleep 60");
    let (id, _) = runtime::launch_background_command(
        command,
        runtime::subagent_completion::ShellKind::Bash,
        "sleep 60",
        "quiet watch",
        Some(&tasks),
    )
    .expect("watch should start");

    tasks.stop(&id).expect("stop should be accepted");

    let finished = wait_for(Duration::from_secs(20), "quiet watch to stop", || {
        tasks.get(&id).filter(|task| !task.status.is_active())
    });
    assert!(
        !finished.status.is_active(),
        "a stopped watch must reach a terminal status so the model is told"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
