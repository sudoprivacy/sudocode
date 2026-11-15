//! Session-owned work that survives the turn which launched it.
//!
//! One registry, cancellation contract and bounded output view for shells and
//! child agents. Producers publish changes; renderers never scan task stores.

use crate::HookAbortSignal;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const TASK_OUTPUT_TAIL_BYTES: usize = 24 * 1024;
const COMPLETED_TASK_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskKind {
    Terminal,
    Agent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Running,
    Stopping,
    ExecutionUnconfirmed,
    Completed,
    Failed,
    Cancelled,
}

impl BackgroundTaskStatus {
    #[inline]
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::ExecutionUnconfirmed => "execution_unconfirmed",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: String,
    pub kind: BackgroundTaskKind,
    pub title: String,
    pub command: Option<String>,
    pub parent_id: Option<String>,
    pub background: bool,
    pub status: BackgroundTaskStatus,
    pub started_ms: u64,
    pub elapsed_ms: u64,
    pub exit_code: Option<i32>,
    pub output_path: Option<String>,
    pub output: String,
    pub activity: String,
}

impl BackgroundTask {
    #[must_use]
    pub fn new(id: String, kind: BackgroundTaskKind, title: String) -> Self {
        Self {
            id,
            kind,
            title,
            command: None,
            parent_id: None,
            background: true,
            status: BackgroundTaskStatus::Running,
            started_ms: u64::try_from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
            elapsed_ms: 0,
            exit_code: None,
            output_path: None,
            output: String::new(),
            activity: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundTaskEvent {
    Updated(BackgroundTask),
    Removed(String),
}

/// Lightweight work metadata, without cloning task output or installing a sink.
pub use crate::runtime_status::BackgroundTaskSummary;

type TaskSink = Arc<dyn Fn(BackgroundTaskEvent) + Send + Sync>;
struct Record {
    task: BackgroundTask,
    abort: HookAbortSignal,
    started: Instant,
    last_emit: Instant,
}
#[derive(Default)]
struct Registry {
    tasks: BTreeMap<String, Record>,
    // Oldest first. IDs only: task state and output live exclusively in tasks.
    history: VecDeque<String>,
    sink: Option<TaskSink>,
    changed: Arc<Condvar>,
    closing: bool,
    paused: bool,
    pause_epoch: u64,
    operations: usize,
}

impl Registry {
    fn emit(&self, event: BackgroundTaskEvent) {
        if let Some(sink) = &self.sink {
            sink(event);
        }
    }

    fn insert(&mut self, record: Record) {
        let history_id = (!record.task.status.is_active()).then(|| record.task.id.clone());
        self.emit(BackgroundTaskEvent::Updated(record.task.clone()));
        if let Some(previous) = self.tasks.insert(record.task.id.clone(), record) {
            self.forget_history(&previous.task.id);
        }
        if let Some(id) = history_id {
            self.retain_finished(id);
        }
    }

    fn forget_history(&mut self, id: &str) {
        if let Some(index) = self.history.iter().position(|entry| entry == id) {
            self.history.remove(index);
        }
    }

    fn remove(&mut self, id: String) {
        self.forget_history(&id);
        if self.tasks.remove(&id).is_some() {
            self.emit(BackgroundTaskEvent::Removed(id));
        }
    }

    fn retain_finished(&mut self, id: String) {
        self.history.push_back(id);
        if self.history.len() > COMPLETED_TASK_LIMIT {
            let oldest = self
                .history
                .pop_front()
                .expect("history exceeded its limit");
            self.remove(oldest);
        }
    }

    fn status_changed(
        &mut self,
        id: &str,
        previous: BackgroundTaskStatus,
        current: BackgroundTaskStatus,
    ) {
        // Output, activity and changes between terminal statuses do not touch
        // retention. Only entering/leaving the inactive history changes it.
        if previous.is_active() != current.is_active() {
            if current.is_active() {
                self.forget_history(id);
            } else {
                self.retain_finished(id.to_owned());
            }
        }
    }

    fn inspect(&mut self, id: &str) {
        let Some(index) = self.history.iter().position(|entry| entry == id) else {
            return;
        };
        if index + 1 < self.history.len() {
            let id = self.history.remove(index).expect("history entry found");
            self.history.push_back(id);
        }
    }

    fn clear_results(&mut self) {
        // The journal can purge stored results while workers still own tasks.
        // Keep unconfirmed execution just as before, and publish all removals.
        let obsolete: Vec<_> = self
            .history
            .iter()
            .filter(|id| self.tasks[*id].task.status != BackgroundTaskStatus::ExecutionUnconfirmed)
            .cloned()
            .collect();
        for id in obsolete {
            self.remove(id);
        }
    }
}

/// Retained by the actual tool worker, including after its caller is cancelled.
#[derive(Clone)]
pub struct ToolOperation {
    _lease: Arc<ToolOperationLease>,
}

struct ToolOperationLease {
    registry: Arc<Mutex<Registry>>,
}

impl Drop for ToolOperationLease {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.operations -= 1;
        registry.changed.notify_all();
    }
}

#[derive(Clone, Default)]
pub struct BackgroundTasks {
    registry: Arc<Mutex<Registry>>,
    parent_id: Option<String>,
    completion: Arc<Mutex<Option<Arc<crate::subagent_completion::CompletionJournal>>>>,
    tool_id: Option<String>,
    assistant_message_id: Option<String>,
}

impl std::fmt::Debug for BackgroundTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTasks").finish_non_exhaustive()
    }
}

impl BackgroundTasks {
    /// Admit one operation before dispatch. All child contexts share this registry.
    pub fn begin_operation(&self) -> Result<ToolOperation, String> {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.paused || registry.closing {
            return Err(
                "Tool execution is paused by the user; wait for explicit return of control"
                    .to_string(),
            );
        }
        registry.operations += 1;
        Ok(ToolOperation {
            _lease: Arc::new(ToolOperationLease {
                registry: Arc::clone(&self.registry),
            }),
        })
    }

    /// Fence admission before signalling every current child, including late producers.
    pub fn begin_pause(&self) -> u64 {
        let ids = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !registry.paused {
                registry.pause_epoch = registry
                    .pause_epoch
                    .checked_add(1)
                    .expect("tool pause epoch exhausted");
                registry.paused = true;
            }
            (
                registry.pause_epoch,
                registry.tasks.keys().cloned().collect::<Vec<_>>(),
            )
        };
        for id in ids.1 {
            let _ = self.stop(&id);
        }
        ids.0
    }

    /// A timeout preserves the fence; it never grants human control prematurely.
    pub fn wait_paused(&self, epoch: u64, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if !registry.paused || registry.pause_epoch != epoch {
                return Err("Tool pause ownership changed".to_string());
            }
            if registry.operations == 0
                && registry
                    .tasks
                    .values()
                    .all(|record| !record.task.status.is_active())
            {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(
                    "Tool operations are still stopping; control remains fenced".to_string()
                );
            }
            let changed = Arc::clone(&registry.changed);
            registry = changed
                .wait_timeout(registry, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    pub fn resume_operations(&self, epoch: u64) -> Result<(), String> {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.closing || !registry.paused || registry.pause_epoch != epoch {
            return Err("Tool pause ownership changed".to_string());
        }
        if registry.operations != 0
            || registry
                .tasks
                .values()
                .any(|record| record.task.status.is_active())
        {
            return Err("Tool operations are still stopping; control remains fenced".to_string());
        }
        registry.paused = false;
        Ok(())
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .paused
    }

    #[must_use]
    pub fn tool_control(&self) -> (u64, bool) {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (registry.pause_epoch, registry.paused)
    }

    #[must_use]
    pub fn with_completion(
        self,
        journal: Option<Arc<crate::subagent_completion::CompletionJournal>>,
    ) -> Self {
        if self.parent_id.is_none() {
            if let Some(journal) = &journal {
                let registry = Arc::downgrade(&self.registry);
                journal.set_cache_clear(move || {
                    if let Some(registry) = registry.upgrade() {
                        let mut registry = registry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        registry.clear_results();
                        registry.changed.notify_all();
                    }
                });
            }
            *self
                .completion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = journal;
        }
        self
    }

    /// Per-invocation identity, never a mutable session-global current tool ID.
    #[must_use]
    pub fn for_tool(&self, assistant_message_id: Option<&str>, tool: Option<&str>) -> Self {
        let mut tasks = self.clone();
        tasks.tool_id = tool.map(str::to_owned);
        tasks.assistant_message_id = assistant_message_id.map(str::to_owned);
        tasks
    }

    pub fn prepare_terminal(
        &self,
        task: &str,
        shell: crate::subagent_completion::ShellKind,
    ) -> Result<Option<Arc<crate::subagent_completion::CompletionJournal>>, String> {
        if self.parent_id.is_some() {
            return Ok(None);
        }
        let journal = self
            .completion
            .lock()
            .map_err(|_| "completion binding poisoned")?
            .clone();
        let Some(journal) = journal.filter(|j| j.tasks_enabled()) else {
            return Ok(None);
        };
        journal.register_terminal(
            self.assistant_message_id.as_deref(),
            self.tool_id
                .as_deref()
                .ok_or("terminal tool identity missing")?,
            task,
            shell,
        )?;
        Ok(Some(journal))
    }

    pub fn durable_terminal(&self, task: &str) -> Result<Option<serde_json::Value>, String> {
        let journal = self
            .completion
            .lock()
            .map_err(|_| "completion binding poisoned")?
            .clone();
        journal.map_or(Ok(None), |j| j.terminal_result(task))
    }

    pub fn set_sink(&self, sink: impl Fn(BackgroundTaskEvent) + Send + Sync + 'static) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.sink = Some(Arc::new(sink));
        // Replay under the producer lock so a snapshot cannot overtake a newer
        // update. Renderers opt in: an idle non-subscribing consumer queues no logs.
        for record in registry.tasks.values() {
            registry.emit(BackgroundTaskEvent::Updated(record.task.clone()));
        }
    }

    pub fn register(&self, mut task: BackgroundTask, abort: HookAbortSignal) {
        task.parent_id = task.parent_id.or_else(|| self.parent_id.clone());
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A producer may register just as its parent is being stopped. Inherit
        // cancellation while holding the same lock used by recursive stop.
        let should_stop = registry.closing
            || registry.paused
            || task.parent_id.as_ref().is_some_and(|id| {
                registry.tasks.get(id).is_some_and(|parent| {
                    matches!(
                        parent.task.status,
                        BackgroundTaskStatus::Stopping | BackgroundTaskStatus::Cancelled
                    )
                })
            });
        if should_stop {
            task.status = BackgroundTaskStatus::Stopping;
        }
        let signal = abort.clone();
        registry.insert(Record {
            task,
            abort,
            started: Instant::now(),
            last_emit: Instant::now(),
        });
        registry.changed.notify_all();
        drop(registry);
        if should_stop {
            signal.abort();
        }
    }

    pub fn update(&self, id: &str, update: impl FnOnce(&mut BackgroundTask)) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = registry.tasks.get_mut(id) else {
            return;
        };
        let previous = (
            record.task.status,
            record.task.background,
            record.task.activity.clone(),
        );
        let first_output = record.task.output.is_empty();
        update(&mut record.task);
        truncate_tail(&mut record.task.output);
        record.task.elapsed_ms =
            u64::try_from(record.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let status = record.task.status;
        // Shell producers already sample at 200ms. Coalesce agent token updates;
        // lifecycle, first output and activity changes remain immediate.
        if previous
            != (
                record.task.status,
                record.task.background,
                record.task.activity.clone(),
            )
            || record.task.kind == BackgroundTaskKind::Terminal
            || (first_output && !record.task.output.is_empty())
            || record.last_emit.elapsed() >= Duration::from_millis(200)
        {
            record.last_emit = Instant::now();
            let event = BackgroundTaskEvent::Updated(record.task.clone());
            registry.emit(event);
        }
        registry.status_changed(id, previous.0, status);
        registry.changed.notify_all();
    }

    /// Retain an explicitly inspected result more recently. Passive reads and
    /// output updates do not affect history, and active/missing tasks are no-ops.
    pub fn inspect(&self, id: &str) {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .inspect(id);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<BackgroundTask> {
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .get(id)
            .map(|r| r.task.clone())
    }

    /// Wait on the producer's state changes, with bounded cancellation checks.
    #[must_use]
    pub fn wait(
        &self,
        id: &str,
        timeout: Duration,
        abort: Option<&HookAbortSignal>,
    ) -> Option<BackgroundTask> {
        let deadline = Instant::now() + timeout;
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let record = registry.tasks.get(id)?;
            if !record.task.status.is_active()
                || Instant::now() >= deadline
                || abort.is_some_and(HookAbortSignal::is_aborted)
            {
                return Some(record.task.clone());
            }
            let changed = Arc::clone(&registry.changed);
            registry = changed
                .wait_timeout(
                    registry,
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(200)),
                )
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    #[must_use]
    pub fn summary(&self) -> BackgroundTaskSummary {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut summary = BackgroundTaskSummary {
            tool_operations: registry.operations,
            ..BackgroundTaskSummary::default()
        };
        for record in registry
            .tasks
            .values()
            .filter(|record| record.task.background)
        {
            if record.task.status == BackgroundTaskStatus::ExecutionUnconfirmed {
                summary.execution_unconfirmed += 1;
            } else if record.task.status.is_active() {
                match record.task.kind {
                    BackgroundTaskKind::Terminal => summary.terminals += 1,
                    BackgroundTaskKind::Agent => summary.agents += 1,
                }
            }
        }
        summary
    }

    #[must_use]
    pub fn snapshots(&self) -> Vec<BackgroundTask> {
        let registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry
            .tasks
            .values()
            .map(|record| record.task.clone())
            .collect()
    }

    /// Child producers inherit ownership as well as the registry.
    #[must_use]
    pub fn for_agent(&self, id: &str) -> Self {
        Self {
            registry: Arc::clone(&self.registry),
            parent_id: Some(id.to_string()),
            completion: Arc::clone(&self.completion),
            tool_id: None,
            assistant_message_id: None,
        }
    }

    pub fn stop(&self, id: &str) -> Result<(), String> {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !registry.tasks.contains_key(id) {
            return Err(format!("Task {id} is no longer available"));
        }
        let mut ids = vec![id.to_string()];
        let mut index = 0;
        while index < ids.len() {
            let parent = &ids[index];
            let children: Vec<_> = registry
                .tasks
                .values()
                .filter(|record| record.task.parent_id.as_deref() == Some(parent.as_str()))
                .map(|record| record.task.id.clone())
                .collect();
            for child in children {
                if !ids.contains(&child) {
                    ids.push(child);
                }
            }
            index += 1;
        }
        let mut signals = Vec::new();
        for id in ids {
            let Some(record) = registry.tasks.get_mut(&id) else {
                continue;
            };
            if record.task.status.is_active() {
                record.task.status = BackgroundTaskStatus::Stopping;
                signals.push(record.abort.clone());
                let event = BackgroundTaskEvent::Updated(record.task.clone());
                registry.emit(event);
            }
        }
        drop(registry);
        for signal in signals {
            signal.abort();
        }
        Ok(())
    }

    /// Signal close immediately, including producers that register while the turn drains.
    pub fn begin_shutdown(&self) {
        let ids = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.closing = true;
            registry.tasks.keys().cloned().collect::<Vec<_>>()
        };
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    /// Close owns the children; cancelling a parent turn does not stop background work.
    pub fn shutdown(&self) {
        self.begin_shutdown();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && self.snapshots().iter().any(|task| task.status.is_active())
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn truncate_tail(text: &mut String) {
    if text.len() > TASK_OUTPUT_TAIL_BYTES {
        let mut start = text.len() - TASK_OUTPUT_TAIL_BYTES;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    }
    // A large agent result must not leave its old allocation behind after
    // clipping. Ordinary small appends keep spare capacity for streaming.
    if text.capacity() > TASK_OUTPUT_TAIL_BYTES * 2 {
        text.shrink_to(TASK_OUTPUT_TAIL_BYTES);
    }
}
