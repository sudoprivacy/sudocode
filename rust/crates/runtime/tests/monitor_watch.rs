//! Integration test for the `Monitor` watch semantics.
//!
//! The contract the tool description promises is "each stdout line is a
//! notification, and the watch ends when the command exits". Both halves have
//! to be observable, or the model is told to wait for events that never come.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use runtime::background_tasks::{self, TaskKind, TaskState};

/// These tests share one process-global completion queue, and draining it takes
/// everything (in production there is a single consumer: the REPL). Run them
/// one at a time so one test cannot swallow another's notifications.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Collect this task's queued notifications until `want` end-of-task arrives.
fn collect(task_id: &str, budget: Duration) -> Vec<background_tasks::Completion> {
    let deadline = Instant::now() + budget;
    let mut mine = Vec::new();
    loop {
        for completion in background_tasks::drain_completions() {
            if completion.task_id == task_id {
                let terminal = completion.event_line.is_none();
                mine.push(completion);
                if terminal {
                    return mine;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "monitor {task_id} never reported an end-of-watch notification; \
             collected so far: {mine:#?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn monitor_streams_each_line_then_reports_the_exit() {
    let _serial = serial();
    let ws = std::env::temp_dir().join(format!("scode-monitor-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");

    let task_id = background_tasks::start_monitor(
        &ws,
        "printf 'line-one\\nline-two\\n'",
        "monitor probe",
        Some(30_000),
        false,
    )
    .expect("monitor should start");

    let task = background_tasks::get(&task_id).expect("registered while running");
    assert_eq!(task.kind, TaskKind::Monitor);
    assert_eq!(task.description.as_deref(), Some("monitor probe"));

    let notifications = collect(&task_id, Duration::from_secs(20));

    // Every stdout line surfaced as its own event, in order.
    let lines: Vec<String> = notifications
        .iter()
        .filter_map(|c| c.event_line.clone())
        .collect();
    assert_eq!(
        lines,
        vec!["line-one".to_string(), "line-two".to_string()],
        "each stdout line must be one notification, in order"
    );

    // A line event reads as "still running", not as a termination.
    let first_event = notifications
        .iter()
        .find(|c| c.event_line.is_some())
        .expect("a line event");
    let xml = background_tasks::render_notification(first_event);
    assert!(xml.contains("Monitor event from monitor probe: line-one"));
    assert!(
        xml.contains("still running"),
        "a line event must not read as the end of the watch: {xml}"
    );

    // And the watch ends exactly once, with the real exit status.
    let terminal = notifications
        .last()
        .expect("at least one notification")
        .clone();
    assert!(terminal.event_line.is_none(), "last one ends the watch");
    assert_eq!(terminal.state, TaskState::Exited);
    assert_eq!(terminal.exit_code, Some(0));

    // Output was persisted too, so the model can re-read it.
    let captured = std::fs::read_to_string(&task.output_path).expect("monitor log");
    assert!(captured.contains("line-one") && captured.contains("line-two"));

    let _ = std::fs::remove_dir_all(&ws);
}

/// Stopping a watch must also notify: a monitor that vanishes silently leaves
/// the model waiting for events that will never arrive.
#[test]
fn stopping_a_monitor_reports_it_as_stopped() {
    let _serial = serial();
    let ws = std::env::temp_dir().join(format!("scode-monitor-stop-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");

    // A watch that would otherwise run far longer than the test.
    let task_id =
        background_tasks::start_monitor(&ws, "sleep 60", "stop probe", Some(60_000), false)
            .expect("monitor should start");

    assert!(
        background_tasks::stop(&task_id),
        "stopping a running watch should report that it acted"
    );
    assert!(
        !background_tasks::stop(&task_id),
        "stopping an already-stopped watch is not a second stop"
    );

    let terminal = collect(&task_id, Duration::from_secs(20))
        .into_iter()
        .next_back()
        .expect("a terminal notification");
    assert_eq!(terminal.state, TaskState::Stopped);
    assert!(
        background_tasks::render_notification(&terminal).contains("stopped"),
        "the notification must say the watch was stopped"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

/// `stop` kills the watch's whole tree — and nothing outside it.
///
/// A monitor runs `bash -lc`, which usually has children (pipelines, nested
/// commands). Stopping must take the tree, so the group is signalled; that is
/// only safe because the child gets its OWN process group. Without that, the
/// group kill lands on the parent's group — our own test harness, and in CI the
/// runner itself (observed once as a runner SIGTERM, exit 143). This test holds
/// that line: the grandchild dies, and this process is still here to assert it.
#[test]
#[cfg(unix)]
fn stop_kills_the_watch_tree_without_touching_the_parent_group() {
    let _serial = serial();
    let ws = std::env::temp_dir().join(format!("scode-monitor-group-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");
    let flag = ws.join("grandchild-alive");

    // A grandchild that keeps touching a file: observable proof of life that
    // outlives the shell unless the whole group is signalled.
    let script = format!(
        "( while true; do touch {flag}; sleep 0.1; done ) & printf 'watching\\n'; wait",
        flag = flag.display()
    );
    let task_id = background_tasks::start_monitor(&ws, &script, "group probe", Some(30_000), false)
        .expect("monitor should start");

    // Wait until the grandchild is demonstrably alive.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !flag.exists() {
        assert!(Instant::now() < deadline, "grandchild never started");
        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(background_tasks::stop(&task_id), "stop should act");

    // After the stop, the grandchild must stop touching the file.
    std::thread::sleep(Duration::from_millis(400));
    let settled = std::fs::metadata(&flag)
        .and_then(|m| m.modified())
        .expect("flag metadata");
    std::thread::sleep(Duration::from_millis(600));
    let after = std::fs::metadata(&flag)
        .and_then(|m| m.modified())
        .expect("flag metadata");
    assert_eq!(
        settled, after,
        "the grandchild kept running after stop: the group kill missed the tree"
    );

    // And this process survived its own group kill — if the group had been
    // ours, the harness would have died instead of reaching this line.
    let _ = std::fs::remove_dir_all(&ws);
}
