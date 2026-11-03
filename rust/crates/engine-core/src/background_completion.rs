//! Turn-boundary delivery for this session's background tasks.
//!
//! Upstream's `BackgroundTasks` registry publishes every state change, and the
//! REPL already forwards those to the task panel so a human can watch them.
//! The model is a second, independent consumer: a background shell or monitor
//! the model started has to tell it when the work ends, or the tool
//! description's promise — "end your turn, you will be notified" — is a lie and
//! the model goes back to blocking the turn with `sleep`.
//!
//! Claude Code delivers the same thing the same way: worker results arrive as
//! user-role messages carrying `<task-notification>` XML. So this renders
//! through [`runtime::coordinator_mode::render_task_notification`], the declared
//! SSOT for that format, rather than inventing a second shape.
//!
//! Only terminal transitions are delivered, and only once per task: the
//! producer re-emits `Updated` for elapsed-time ticks and output growth, so a
//! naive "status is terminal" check would notify on every tick after the task
//! finished.

use std::collections::BTreeSet;

use engine_events::{BackgroundTask, BackgroundTaskEvent, BackgroundTaskKind};

/// What the REPL needs to deliver one finished background task.
pub struct Finished {
    /// Stable identity used by session-owned queues.
    pub id: String,
    /// The `<task-notification>` XML handed to the model as its next prompt.
    pub prompt: String,
    /// One-line summary for the terminal / queued chip.
    pub display: String,
}

/// Tracks which background tasks have already been reported to the model.
#[derive(Default)]
pub struct Completions {
    notified: BTreeSet<String>,
}

impl Completions {
    /// Translate a background-task event into a model-facing notification.
    ///
    /// Returns `None` while a task is still active, for a task already
    /// reported, and for `Removed` (a task leaving the registry is a UI
    /// concern — the model was told when it ended).
    pub fn observe(&mut self, event: &BackgroundTaskEvent) -> Option<Finished> {
        let BackgroundTaskEvent::Updated(task) = event else {
            return None;
        };
        // Sub-agents register here too (`BackgroundTaskKind::Agent`), but the
        // engine already delivers their results as `SubagentCompletionReady`.
        // Reporting them from here as well would hand the model the same
        // `<task-notification>` twice. Shells are the kind nobody else reports.
        if task.kind != BackgroundTaskKind::Terminal {
            return None;
        }
        if task.status.is_active() {
            return None;
        }
        if !self.notified.insert(task.id.clone()) {
            return None;
        }
        Some(Self::render(task))
    }

    fn render(task: &BackgroundTask) -> Finished {
        let status =
            runtime::coordinator_mode::normalize_task_notification_status(task.status.label());
        let label = if task.title.trim().is_empty() {
            task.command.clone().unwrap_or_else(|| task.id.clone())
        } else {
            task.title.clone()
        };
        let summary = match task.exit_code {
            Some(code) => format!("Background task \"{label}\" {status} (exit {code})"),
            None => format!("Background task \"{label}\" {status}"),
        };
        // The tail the producer already keeps bounded. Empty output means the
        // command printed nothing, and an empty `<result>` is noise, so it is
        // omitted rather than emitted blank.
        let result = (!task.output.trim().is_empty()).then(|| task.output.clone());

        let prompt = runtime::coordinator_mode::render_task_notification(
            &runtime::coordinator_mode::TaskNotificationView {
                agent_id: &task.id,
                status,
                summary: &summary,
                result: result.as_deref(),
                color: None,
                duration_ms: Some(task.elapsed_ms),
                tool_uses: None,
                total_tokens: None,
            },
        );
        Finished {
            id: task.id.clone(),
            prompt,
            display: summary,
        }
    }
}
