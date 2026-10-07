//! Integration test for the background-command reaper.
//!
//! `bash` with `run_in_background` detaches and returns immediately, so the
//! only proof that the model will ever hear about the command again is: the
//! task is registered, its output lands in the task file, and the reaper turns
//! the exit into exactly one drainable completion.

use std::time::{Duration, Instant};

use runtime::background_tasks::{self, TaskKind, TaskState};
use runtime::{execute_bash, BashCommandInput};

fn wait_for_completion(task_id: &str, budget: Duration) -> background_tasks::Completion {
    let deadline = Instant::now() + budget;
    loop {
        for completion in background_tasks::drain_completions() {
            if completion.task_id == task_id {
                return completion;
            }
        }
        assert!(
            Instant::now() < deadline,
            "reaper never reported {task_id}: a finished background command that \
             produces no completion is invisible to the model forever"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn background_command_is_registered_captured_and_reaped() {
    let input = BashCommandInput {
        command: "printf 'bg-marker'; exit 7".to_string(),
        timeout: None,
        description: Some("reaper probe".to_string()),
        run_in_background: Some(true),
        dangerously_disable_sandbox: None,
        namespace_restrictions: None,
        isolate_network: None,
        filesystem_mode: None,
        allowed_mounts: None,
    };

    let out = execute_bash(input).expect("background spawn should succeed");

    // The call returns immediately with a task id and nothing to show.
    let task_id = out
        .background_task_id
        .clone()
        .expect("a background run must hand back a task id");
    assert!(out.stdout.is_empty(), "background output is not inlined");
    assert_eq!(out.exit_code, None, "no exit code is known yet");

    // Registered while running, so it can be found, stopped, or inspected.
    let task = background_tasks::get(&task_id).expect("task must be registered");
    assert_eq!(task.kind, TaskKind::BackgroundCommand);
    assert_eq!(task.description.as_deref(), Some("reaper probe"));
    let output_path = task.output_path.clone();
    assert_eq!(
        out.raw_output_path.as_deref(),
        Some(output_path.to_string_lossy().as_ref()),
        "the tool result must point at the file the output is going to"
    );

    // The reaper reports the exit exactly once, with the real status.
    let completion = wait_for_completion(&task_id, Duration::from_secs(20));
    assert_eq!(completion.state, TaskState::Exited);
    assert_eq!(
        completion.exit_code,
        Some(7),
        "real exit status is reported"
    );

    // Output was captured, not discarded to /dev/null.
    let captured = std::fs::read_to_string(&output_path).expect("task log must exist");
    assert!(
        captured.contains("bg-marker"),
        "the command's output must be readable afterwards; got {captured:?}"
    );

    // And the notification names the task and its output file.
    let xml = background_tasks::render_notification(&completion);
    assert!(xml.contains("<task-notification>"));
    assert!(xml.contains("reaper probe"));
    assert!(xml.contains("exited with code 7"));

    let _ = std::fs::remove_file(&output_path);
}
