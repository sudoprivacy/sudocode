//! Session-owned work that survives the turn which launched it.
//!
//! One registry, cancellation contract and bounded output view for shells and
//! child agents. Producers publish changes; renderers never scan task stores.

use crate::HookAbortSignal;
use std::{
    collections::BTreeMap,
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
    sink: Option<TaskSink>,
    changed: Arc<Condvar>,
    closing: bool,
}

#[derive(Clone, Default)]
pub struct BackgroundTasks {
    registry: Arc<Mutex<Registry>>,
    parent_id: Option<String>,
}

impl std::fmt::Debug for BackgroundTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTasks").finish_non_exhaustive()
    }
}

impl BackgroundTasks {
    pub fn set_sink(&self, sink: impl Fn(BackgroundTaskEvent) + Send + Sync + 'static) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.sink = Some(Arc::new(sink));
        // Replay under the producer lock so a snapshot cannot overtake a newer
        // update. Renderers opt in: an idle non-subscribing consumer queues no logs.
        for record in registry.tasks.values() {
            Self::emit(&registry, BackgroundTaskEvent::Updated(record.task.clone()));
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
        Self::emit(&registry, BackgroundTaskEvent::Updated(task.clone()));
        registry.tasks.insert(
            task.id.clone(),
            Record {
                task,
                abort,
                started: Instant::now(),
                last_emit: Instant::now(),
            },
        );
        Self::prune(&mut registry);
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
            Self::emit(&registry, event);
        }
        registry.changed.notify_all();
        Self::prune(&mut registry);
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
                Self::emit(&registry, event);
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

    fn emit(registry: &Registry, event: BackgroundTaskEvent) {
        if let Some(sink) = &registry.sink {
            sink(event);
        }
    }

    fn prune(registry: &mut Registry) {
        // Prune on completion as well as insertion: a large burst cannot leave
        // an unbounded finished history when no further tasks are launched.
        let mut completed: Vec<_> = registry
            .tasks
            .values()
            .filter(|r| !r.task.status.is_active())
            .map(|r| (r.task.started_ms, r.task.id.clone()))
            .collect();
        completed.sort();
        for (_, id) in completed.into_iter().rev().skip(COMPLETED_TASK_LIMIT) {
            registry.tasks.remove(&id);
            Self::emit(registry, BackgroundTaskEvent::Removed(id));
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
