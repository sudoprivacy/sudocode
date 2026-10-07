//! Registry + reaper for background work this session owns.
//!
//! Two tools detach a child process and return before it finishes: `bash`
//! with `run_in_background` and `Monitor`. Both need the same three things —
//! somewhere to find the task again (to stop it or read its output), a reaper
//! that notices the exit, and a completion record the REPL can turn into a
//! `<task-notification>` for the next turn. Keeping them in ONE registry is
//! what makes "stop a background task" and "tell me when it ends" mean the
//! same thing regardless of which tool started it.
//!
//! Mirrors the shape of the sub-agent completion registry (`tools`'s
//! `global_agent_registry`): a process-global `OnceLock` holding a mutex'd
//! map, written by whoever spawns and drained by the renderer at a turn
//! boundary.
//!
//! Output is captured to a file rather than discarded: the model is told it
//! will be notified when the command ends, and the first thing it will want
//! is what the command printed. A detached child writing to `/dev/null` makes
//! that promise unkeepable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// What a background task is doing, as far as this session knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Spawned and not yet reaped.
    Running,
    /// Exited on its own; `exit_code` carries the status when one was reported.
    Exited,
    /// Stopped by us (`TaskStop` / shutdown), not by the command finishing.
    Stopped,
}

/// Which tool started the task — selects how a completion reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// `bash` with `run_in_background`: one notification when it exits.
    BackgroundCommand,
    /// `Monitor`: one notification per stdout line, plus one when it ends.
    Monitor,
}

/// A background task's record.
#[derive(Debug, Clone)]
pub struct BackgroundTask {
    pub task_id: String,
    pub kind: TaskKind,
    /// The command line, for the notification text.
    pub command: String,
    /// Caller-supplied label; `Monitor` requires one, background bash may not
    /// have one.
    pub description: Option<String>,
    /// Where the child's combined output is being written.
    pub output_path: PathBuf,
    pub state: TaskState,
    pub exit_code: Option<i32>,
}

/// A finished task, ready to be rendered as a notification.
#[derive(Debug, Clone)]
pub struct Completion {
    pub task_id: String,
    pub kind: TaskKind,
    pub command: String,
    pub description: Option<String>,
    pub output_path: PathBuf,
    pub state: TaskState,
    pub exit_code: Option<i32>,
}

#[derive(Default)]
struct Registry {
    tasks: BTreeMap<String, BackgroundTask>,
    /// Finished tasks not yet delivered to the model.
    pending: Vec<Completion>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> std::sync::MutexGuard<'static, Registry> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Monotonic per-process counter for task ids.
///
/// Ids must be unique within a session and stable in the notification text, so
/// a counter (not a PID) backs them: a PID is reused by the OS and says
/// nothing about which of this session's tasks it was.
pub fn next_task_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Record a freshly spawned task. Called by the spawning tool.
pub fn register(task: BackgroundTask) {
    lock().tasks.insert(task.task_id.clone(), task);
}

/// Snapshot of a task, if this session still knows it.
#[must_use]
pub fn get(task_id: &str) -> Option<BackgroundTask> {
    lock().tasks.get(task_id).cloned()
}

/// Every task this session has started, oldest id first.
#[must_use]
pub fn list() -> Vec<BackgroundTask> {
    lock().tasks.values().cloned().collect()
}

/// Mark a task finished and queue its completion for delivery.
///
/// Idempotent per task: a task already in a terminal state does not queue a
/// second completion, so a reaper racing an explicit stop cannot notify twice.
pub fn finish(task_id: &str, state: TaskState, exit_code: Option<i32>) {
    let mut reg = lock();
    let Some(task) = reg.tasks.get_mut(task_id) else {
        return;
    };
    if task.state != TaskState::Running {
        return;
    }
    task.state = state;
    task.exit_code = exit_code;
    let completion = Completion {
        task_id: task.task_id.clone(),
        kind: task.kind,
        command: task.command.clone(),
        description: task.description.clone(),
        output_path: task.output_path.clone(),
        state,
        exit_code,
    };
    reg.pending.push(completion);
}

/// Take every undelivered completion. Called at a turn boundary.
#[must_use]
pub fn drain_completions() -> Vec<Completion> {
    std::mem::take(&mut lock().pending)
}

/// Render a completion as the `<task-notification>` body the model reads.
///
/// Same XML shape the sub-agent completions use, so the model learns one
/// format for "a background thing you started has news".
#[must_use]
pub fn render_notification(completion: &Completion) -> String {
    let label = completion
        .description
        .as_deref()
        .unwrap_or(&completion.command);
    let outcome = match (completion.state, completion.exit_code) {
        (TaskState::Stopped, _) => "stopped".to_string(),
        (_, Some(code)) => format!("exited with code {code}"),
        (_, None) => "exited".to_string(),
    };
    let kind = match completion.kind {
        TaskKind::BackgroundCommand => "background command",
        TaskKind::Monitor => "monitor",
    };
    format!(
        "<task-notification>\n\
         The {kind} you started ({label}) {outcome}.\n\
         task-id: {id}\n\
         output: {output}\n\
         Read the output file if you need what it printed.\n\
         </task-notification>",
        id = completion.task_id,
        output = completion.output_path.display(),
    )
}

/// A one-line summary for the terminal, shown where a queued chip or echo goes.
#[must_use]
pub fn render_display(completion: &Completion) -> String {
    let label = completion
        .description
        .as_deref()
        .unwrap_or(&completion.command);
    let outcome = match (completion.state, completion.exit_code) {
        (TaskState::Stopped, _) => "stopped".to_string(),
        (_, Some(code)) => format!("exited {code}"),
        (_, None) => "exited".to_string(),
    };
    let kind = match completion.kind {
        TaskKind::BackgroundCommand => "Background",
        TaskKind::Monitor => "Monitor",
    };
    format!("{kind}: {label} — {outcome}")
}

/// Where a task's output file lives: under the session's workspace so it is
/// readable with the ordinary file tools and cleaned up with the workspace.
///
/// # Errors
/// When the directory cannot be created.
pub fn output_path_for(workspace_root: &Path, task_id: &str) -> std::io::Result<PathBuf> {
    let dir = workspace_root.join(".sudocode-tasks");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{task_id}.log")))
}

#[cfg(test)]
mod registry_docs {
    use super::*;

    fn task(id: &str, kind: TaskKind) -> BackgroundTask {
        BackgroundTask {
            task_id: id.to_string(),
            kind,
            command: format!("cmd-{id}"),
            description: None,
            output_path: PathBuf::from(format!("/tmp/{id}.log")),
            state: TaskState::Running,
            exit_code: None,
        }
    }

    #[test]
    fn finish_queues_one_completion_and_is_idempotent() {
        register(task("t-idem", TaskKind::BackgroundCommand));
        finish("t-idem", TaskState::Exited, Some(0));
        // A racing stop must not produce a second notification.
        finish("t-idem", TaskState::Stopped, None);
        let mine: Vec<_> = drain_completions()
            .into_iter()
            .filter(|c| c.task_id == "t-idem")
            .collect();
        assert_eq!(mine.len(), 1, "exactly one completion per task");
        assert_eq!(mine[0].state, TaskState::Exited);
        assert_eq!(mine[0].exit_code, Some(0));
    }

    #[test]
    fn drain_takes_completions_once() {
        register(task("t-drain", TaskKind::Monitor));
        finish("t-drain", TaskState::Exited, Some(3));
        let first: Vec<_> = drain_completions()
            .into_iter()
            .filter(|c| c.task_id == "t-drain")
            .collect();
        assert_eq!(first.len(), 1);
        let second: Vec<_> = drain_completions()
            .into_iter()
            .filter(|c| c.task_id == "t-drain")
            .collect();
        assert!(second.is_empty(), "a completion is delivered once");
    }

    #[test]
    fn notification_names_the_task_and_its_output() {
        let completion = Completion {
            task_id: "t-1".to_string(),
            kind: TaskKind::BackgroundCommand,
            command: "npm test".to_string(),
            description: None,
            output_path: PathBuf::from("/tmp/t-1.log"),
            state: TaskState::Exited,
            exit_code: Some(1),
        };
        let xml = render_notification(&completion);
        assert!(xml.contains("<task-notification>"));
        assert!(xml.contains("background command"));
        assert!(xml.contains("npm test"));
        assert!(xml.contains("exited with code 1"));
        assert!(xml.contains("/tmp/t-1.log"));
    }
}
