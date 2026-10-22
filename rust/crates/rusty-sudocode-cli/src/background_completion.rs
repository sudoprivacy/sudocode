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

use engine_events::{
    BackgroundTask, BackgroundTaskEvent, BackgroundTaskKind, BackgroundTaskStatus,
};

/// What the REPL needs to deliver one finished background task.
pub struct Finished {
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
            prompt,
            display: summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: BackgroundTaskStatus) -> BackgroundTask {
        let mut task = BackgroundTask::new(
            id.to_string(),
            BackgroundTaskKind::Terminal,
            "npm test".to_string(),
        );
        task.status = status;
        task.exit_code = Some(0);
        task
    }

    #[test]
    fn an_active_task_is_not_reported() {
        let mut completions = Completions::default();
        let event = BackgroundTaskEvent::Updated(task("t-active", BackgroundTaskStatus::Running));
        assert!(completions.observe(&event).is_none());
    }

    #[test]
    fn a_finished_task_is_reported_once_as_task_notification_xml() {
        let mut completions = Completions::default();
        let event = BackgroundTaskEvent::Updated(task("t-done", BackgroundTaskStatus::Completed));

        let finished = completions
            .observe(&event)
            .expect("a terminal task must be reported");
        assert!(
            finished.prompt.contains("<task-notification>"),
            "must use the shared renderer's XML: {}",
            finished.prompt
        );
        assert!(finished.prompt.contains("<task-id>t-done</task-id>"));
        assert!(finished.prompt.contains("<status>completed</status>"));
        assert!(finished.display.contains("npm test"));

        // The producer keeps emitting `Updated` after the task ends (elapsed
        // ticks, output growth); the model must not be told twice.
        assert!(
            completions.observe(&event).is_none(),
            "a task is reported to the model exactly once"
        );
    }

    #[test]
    fn a_failed_task_reports_as_failed_and_a_stop_as_killed() {
        let mut completions = Completions::default();
        let failed = completions
            .observe(&BackgroundTaskEvent::Updated(task(
                "t-fail",
                BackgroundTaskStatus::Failed,
            )))
            .expect("failed task reported");
        assert!(failed.prompt.contains("<status>failed</status>"));

        let killed = completions
            .observe(&BackgroundTaskEvent::Updated(task(
                "t-stop",
                BackgroundTaskStatus::Cancelled,
            )))
            .expect("cancelled task reported");
        assert!(
            killed.prompt.contains("<status>killed</status>"),
            "a stop normalizes to the documented `killed` status: {}",
            killed.prompt
        );
    }

    /// Sub-agents register as background tasks too, and the engine already
    /// delivers their results. Reporting them here as well handed the model the
    /// same `<task-notification>` twice — caught by
    /// `pty_subagent_workflow`'s "coordinator and event bridge must not
    /// duplicate the notification".
    #[test]
    fn a_finished_sub_agent_is_left_to_the_engine() {
        let mut completions = Completions::default();
        let mut agent = BackgroundTask::new(
            "agent-x7q".to_string(),
            BackgroundTaskKind::Agent,
            "refactor auth".to_string(),
        );
        agent.status = BackgroundTaskStatus::Completed;
        assert!(
            completions
                .observe(&BackgroundTaskEvent::Updated(agent))
                .is_none(),
            "a sub-agent result belongs to the engine's own delivery path"
        );
    }

    #[test]
    fn removal_is_a_ui_concern_and_not_delivered() {
        let mut completions = Completions::default();
        assert!(completions
            .observe(&BackgroundTaskEvent::Removed("t-gone".to_string()))
            .is_none());
    }
}
