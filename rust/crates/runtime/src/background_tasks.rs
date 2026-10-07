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
use std::sync::{Arc, Mutex, OnceLock};

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
    /// One `Monitor` stdout line. `None` for an end-of-task completion.
    ///
    /// A monitor reports many times (one notification per line) and then once
    /// more when the watch ends, so "a thing to tell the model" cannot be
    /// modelled as terminal-only. Carrying the line here keeps ONE delivery
    /// pipe and one XML shape instead of a second parallel channel.
    pub event_line: Option<String>,
}

#[derive(Default)]
struct Registry {
    tasks: BTreeMap<String, BackgroundTask>,
    /// OS pids, kept for diagnostics only.
    pids: BTreeMap<String, u32>,
    /// The live child for each running task, shared with its watch thread.
    ///
    /// `stop` must not wait for the reader to return from a blocking read — a
    /// quiet watch (`sleep 60`) would never notice — so it kills through this
    /// handle. Killing via the `GroupChild` keeps the signal inside the watch's
    /// own process group, which is the part that must never be hand-rolled.
    children: BTreeMap<String, Arc<Mutex<command_group::GroupChild>>>,
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
        event_line: None,
    };
    reg.pending.push(completion);
}

/// Queue one `Monitor` stdout line as its own notification.
///
/// Each line is an event the model should hear about while the watch is still
/// running, so this does NOT change the task's state — only `finish` ends a
/// task. A line for an already-finished task is dropped: the watch is over and
/// a late line would read as if it were still live.
pub fn record_event_line(task_id: &str, line: &str) {
    let mut reg = lock();
    let Some(task) = reg.tasks.get(task_id) else {
        return;
    };
    if task.state != TaskState::Running {
        return;
    }
    let completion = Completion {
        task_id: task.task_id.clone(),
        kind: task.kind,
        command: task.command.clone(),
        description: task.description.clone(),
        output_path: task.output_path.clone(),
        state: TaskState::Running,
        exit_code: None,
        event_line: Some(line.to_string()),
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
    let label_for_event = completion
        .description
        .as_deref()
        .unwrap_or(&completion.command)
        .to_string();
    if let Some(line) = &completion.event_line {
        return format!(
            "<task-notification>\n\
             Monitor event from {label_for_event}: {line}\n\
             task-id: {id}\n\
             The watch is still running; you will be notified again on the next event.\n\
             </task-notification>",
            id = completion.task_id,
        );
    }
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
    if let Some(line) = &completion.event_line {
        let label = completion
            .description
            .as_deref()
            .unwrap_or(&completion.command);
        return format!("Monitor: {label} — {line}");
    }
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
            event_line: None,
        };
        let xml = render_notification(&completion);
        assert!(xml.contains("<task-notification>"));
        assert!(xml.contains("background command"));
        assert!(xml.contains("npm test"));
        assert!(xml.contains("exited with code 1"));
        assert!(xml.contains("/tmp/t-1.log"));
    }
}

/// Default and maximum watch lifetime for `Monitor`, mirroring CC's bounds:
/// a watch that outlives the thing it watches is a leak, so there is always a
/// ceiling even when the caller asks for none.
pub const MONITOR_DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// Hard ceiling for a non-persistent watch.
pub const MONITOR_MAX_TIMEOUT_MS: u64 = 3_600_000;

/// Start a `Monitor` watch over `command`.
///
/// Streams the child's stdout line by line: each line is queued as its own
/// notification (`record_event_line`) so the model hears about it while the
/// watch is live. Exit, the timeout, or an explicit [`stop`] ends the watch and
/// queues one terminal completion. `persistent` drops the timeout so the watch
/// lives as long as the session.
///
/// Returns the task id. The spawn itself is synchronous; everything after is
/// on a dedicated thread, so the tool call returns immediately — the point of
/// a monitor is that the turn ends and notifications arrive later.
///
/// # Errors
/// When the output file cannot be created or the child cannot be spawned.
pub fn start_monitor(
    workspace_root: &Path,
    command: &str,
    description: &str,
    timeout_ms: Option<u64>,
    persistent: bool,
) -> std::io::Result<String> {
    use command_group::CommandGroup;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let task_id = format!("mon-{}", next_task_seq());
    let output_path = output_path_for(workspace_root, &task_id)?;
    let mut log = std::fs::File::create(&output_path)?;

    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `group()` puts the watch in its own process group and owns the killing,
    // which is the same primitive `bash.rs` uses for foreground commands.
    // Hand-rolling it (`kill -TERM -<pid>`) is how a group kill ends up aimed
    // at OUR group when the child is not actually a group leader — in CI that
    // took out the test harness and the runner (SIGTERM, exit 143).
    let mut child = cmd.group().spawn()?;

    let stdout = child.inner().stdout.take();
    register(BackgroundTask {
        task_id: task_id.clone(),
        kind: TaskKind::Monitor,
        command: command.to_string(),
        description: Some(description.to_string()),
        output_path: output_path.clone(),
        state: TaskState::Running,
        exit_code: None,
    });
    register_pid(&task_id, child.id());
    let child = Arc::new(Mutex::new(child));
    lock().children.insert(task_id.clone(), Arc::clone(&child));

    // Deadline, unless the caller asked for a session-lifetime watch.
    let deadline = if persistent {
        None
    } else {
        let ms = timeout_ms
            .unwrap_or(MONITOR_DEFAULT_TIMEOUT_MS)
            .min(MONITOR_MAX_TIMEOUT_MS);
        Some(std::time::Instant::now() + std::time::Duration::from_millis(ms))
    };

    let watch_id = task_id.clone();
    std::thread::Builder::new()
        .name(format!("monitor-{task_id}"))
        .spawn(move || {
            // One line read = one event. Reading line-wise (not chunked) is
            // what makes "each stdout line is a notification" true rather than
            // approximately true.
            // One exit path, so the watch always ends with exactly one terminal
            // notification. `finish` is idempotent, which is what lets a stop
            // claim the reason first (see `stop`) and this call become the
            // no-op, instead of the two racing to label the same ending.
            let mut timed_out = false;
            if let Some(stdout) = stdout {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    let _ = writeln!(log, "{line}");
                    // A stop already ended the watch (it killed the tree
                    // itself): drop the rest of the stream rather than
                    // reporting events for a dead watch.
                    if is_finished(&watch_id) {
                        break;
                    }
                    record_event_line(&watch_id, &line);
                    if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        timed_out = true;
                        break;
                    }
                }
            }
            if timed_out {
                if let Ok(mut c) = child.lock() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                finish(&watch_id, TaskState::Stopped, None);
                return;
            }
            let code = child
                .lock()
                .ok()
                .and_then(|mut c| c.wait().ok())
                .and_then(|status| status.code());
            finish(&watch_id, TaskState::Exited, code);
        })
        .map_err(|e| std::io::Error::other(format!("spawn monitor thread: {e}")))?;

    Ok(task_id)
}

/// `true` once a task has left the running state.
#[must_use]
pub fn is_finished(task_id: &str) -> bool {
    lock()
        .tasks
        .get(task_id)
        .is_some_and(|t| t.state != TaskState::Running)
}

/// Remember a task's OS pid so [`stop`] can signal it.
pub fn register_pid(task_id: &str, pid: u32) {
    lock().pids.insert(task_id.to_string(), pid);
}

/// Stop a running background task (monitor or background command).
///
/// Kills the process and records the stop as the task's terminal state, so the
/// model is told the watch ended rather than silently losing it. Returns
/// `false` when the task is unknown or already finished.
pub fn stop(task_id: &str) -> bool {
    let pid = lock().pids.get(task_id).copied();
    if is_finished(task_id) || get(task_id).is_none() {
        return false;
    }
    // Claim the terminal state BEFORE signalling. The watch thread is parked on
    // `child.wait()`, which returns as soon as the signal lands and would
    // otherwise report a deliberate stop as an ordinary exit — whichever call
    // reached `finish` first would decide, and the notification would lie about
    // why the watch ended. `finish` is idempotent, so claiming it here makes the
    // reaper's later call the no-op.
    finish(task_id, TaskState::Stopped, None);
    // Kill through the watch's own `GroupChild`: the library confines the
    // signal to that watch's process group, and doing it here (rather than
    // asking the reader thread) means a quiet watch dies too instead of
    // surviving until its next line of output. `pid` is diagnostics only — it
    // is deliberately NOT used to build a signal target by hand.
    let _ = pid;
    let child = lock().children.remove(task_id);
    if let Some(child) = child {
        if let Ok(mut c) = child.lock() {
            let _ = c.kill();
        }
    }
    true
}
