#![allow(clippy::must_use_candidate, clippy::unnecessary_map_or)]
//! Task registry — SSOT for all task state.
//!
//! Persists to `.sudocode-tasks.json` (or `$SUDOCODE_TASK_STORE`) on
//! every mutation so the ContextSlot can display live progress.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{validate_packet, TaskPacket, TaskPacketValidationError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Created,
    Running,
    Completed,
    Failed,
    Stopped,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub subject: String,
    pub prompt: String,
    pub description: Option<String>,
    #[serde(
        rename = "activeForm",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub active_form: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    pub task_packet: Option<TaskPacket>,
    pub status: TaskStatus,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<TaskMessage>,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub role: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    tasks: HashMap<String, Task>,
    counter: u64,
    store_path: Option<PathBuf>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Resolve the on-disk store path for task persistence.
///
/// Priority: `$SUDOCODE_TASK_STORE` env var, then
/// `<workspace_root>/.sudocode-tasks.json`.
pub fn task_store_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("SUDOCODE_TASK_STORE") {
        return Ok(PathBuf::from(path));
    }
    let cwd = crate::current_workspace_root().map_err(|error| error.to_string())?;
    Ok(cwd.join(".sudocode-tasks.json"))
}

impl TaskRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load persisted tasks from `path`. Missing / unreadable files
    /// produce an empty registry (not an error).
    pub fn load(path: &Path) -> Self {
        let mut inner = RegistryInner {
            tasks: HashMap::new(),
            counter: 0,
            store_path: Some(path.to_owned()),
        };
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(tasks) = serde_json::from_str::<Vec<Task>>(&text) {
                for task in tasks {
                    let id = task.task_id.clone();
                    inner.counter = inner.counter.max(
                        id.rsplit('_')
                            .next()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0),
                    );
                    inner.tasks.insert(id, task);
                }
            }
        }
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Persist current tasks to the configured store path.
    fn save(inner: &RegistryInner) {
        let Some(path) = &inner.store_path else {
            return;
        };
        let mut tasks: Vec<&Task> = inner.tasks.values().collect();
        tasks.sort_by_key(|t| t.created_at);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(&tasks).unwrap_or_default(),
        );
    }

    /// Set the persistence path (enables save-on-mutate).
    pub fn set_store_path(&self, path: PathBuf) {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.store_path = Some(path);
    }

    pub fn create_with_subject(
        &self,
        subject: &str,
        description: Option<&str>,
        active_form: Option<&str>,
        dependencies: Vec<String>,
    ) -> Task {
        self.create_task_full(
            subject.to_owned(),
            subject.to_owned(),
            description.map(str::to_owned),
            active_form.map(str::to_owned),
            None,
            TaskStatus::Pending,
            dependencies,
        )
    }

    pub fn create(&self, prompt: &str, description: Option<&str>) -> Task {
        self.create_task_full(
            prompt.to_owned(),
            prompt.to_owned(),
            description.map(str::to_owned),
            None,
            None,
            TaskStatus::Created,
            Vec::new(),
        )
    }

    pub fn create_from_packet(
        &self,
        packet: TaskPacket,
    ) -> Result<Task, TaskPacketValidationError> {
        let packet = validate_packet(packet)?.into_inner();
        let description = packet
            .scope_path
            .clone()
            .or_else(|| Some(packet.scope.to_string()));
        Ok(self.create_task_full(
            packet.objective.clone(),
            packet.objective.clone(),
            description,
            None,
            Some(packet),
            TaskStatus::Created,
            Vec::new(),
        ))
    }

    fn create_task_full(
        &self,
        subject: String,
        prompt: String,
        description: Option<String>,
        active_form: Option<String>,
        task_packet: Option<TaskPacket>,
        initial_status: TaskStatus,
        dependencies: Vec<String>,
    ) -> Task {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let task_id = format!("task_{:08x}_{}", ts, inner.counter);
        let task = Task {
            task_id: task_id.clone(),
            subject,
            prompt,
            description,
            active_form,
            dependencies,
            task_packet,
            status: initial_status,
            created_at: ts,
            updated_at: ts,
            messages: Vec::new(),
            output: String::new(),
        };
        inner.tasks.insert(task_id, task.clone());
        Self::save(&inner);
        task
    }

    pub fn get(&self, task_id: &str) -> Option<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.get(task_id).cloned()
    }

    pub fn list(&self, status_filter: Option<TaskStatus>) -> Vec<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let mut tasks: Vec<Task> = inner
            .tasks
            .values()
            .filter(|t| status_filter.map_or(true, |s| t.status == s))
            .cloned()
            .collect();
        tasks.sort_by_key(|t| t.created_at);
        tasks
    }

    pub fn stop(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        match task.status {
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped => {
                return Err(format!(
                    "task {task_id} is already in terminal state: {}",
                    task.status
                ));
            }
            _ => {}
        }

        task.status = TaskStatus::Stopped;
        task.updated_at = now_secs();
        let result = task.clone();
        Self::save(&inner);
        Ok(result)
    }

    pub fn update(&self, task_id: &str, message: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        task.messages.push(TaskMessage {
            role: String::from("user"),
            content: message.to_owned(),
            timestamp: now_secs(),
        });
        task.updated_at = now_secs();
        let result = task.clone();
        Self::save(&inner);
        Ok(result)
    }

    pub fn output(&self, task_id: &str) -> Result<String, String> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        Ok(task.output.clone())
    }

    pub fn append_output(&self, task_id: &str, output: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.output.push_str(output);
        task.updated_at = now_secs();
        // No save — output is high-frequency / transient.
        Ok(())
    }

    pub fn set_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.status = status;
        task.updated_at = now_secs();
        Self::save(&inner);
        Ok(())
    }

    /// Update structured fields on a task (subject, description, active_form, status, dependencies).
    /// Returns the updated task.
    pub fn update_fields(
        &self,
        task_id: &str,
        subject: Option<&str>,
        description: Option<&str>,
        active_form: Option<&str>,
        status: Option<TaskStatus>,
        dependencies: Option<Vec<String>>,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if let Some(s) = subject {
            task.subject = s.to_owned();
        }
        if let Some(d) = description {
            task.description = Some(d.to_owned());
        }
        if let Some(af) = active_form {
            task.active_form = Some(af.to_owned());
        }
        if let Some(st) = status {
            task.status = st;
        }
        if let Some(deps) = dependencies {
            task.dependencies = deps;
        }
        task.updated_at = now_secs();
        let result = task.clone();
        Self::save(&inner);
        Ok(result)
    }

    pub fn validate_dependencies(&self, deps: &[String]) -> Result<(), String> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        for dep in deps {
            if !inner.tasks.contains_key(dep) {
                return Err(format!("dependency task not found: {dep}"));
            }
        }
        Ok(())
    }

    pub fn remove(&self, task_id: &str) -> Option<Task> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let removed = inner.tasks.remove(task_id);
        if removed.is_some() {
            Self::save(&inner);
        }
        removed
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_retrieves_tasks() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do something", Some("A test task"));
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.prompt, "Do something");
        assert_eq!(task.subject, "Do something");
        assert_eq!(task.description.as_deref(), Some("A test task"));
        assert_eq!(task.task_packet, None);

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_id, task.task_id);
    }

    #[test]
    fn creates_task_with_subject() {
        let registry = TaskRegistry::new();
        let task = registry.create_with_subject(
            "Fix login bug",
            Some("Investigate auth timeout"),
            Some("Fixing login bug"),
            Vec::new(),
        );
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.subject, "Fix login bug");
        assert_eq!(task.prompt, "Fix login bug");
        assert_eq!(task.active_form.as_deref(), Some("Fixing login bug"));
    }

    #[test]
    fn creates_task_from_packet() {
        use crate::task_packet::TaskScope;
        let registry = TaskRegistry::new();
        let packet = TaskPacket {
            objective: "Ship task packet support".to_string(),
            scope: TaskScope::Module,
            scope_path: Some("runtime/task system".to_string()),
            worktree: Some("/tmp/wt-task".to_string()),
            repo: "sudo-code-parity".to_string(),
            branch_policy: "origin/main only".to_string(),
            acceptance_tests: vec!["cargo test --workspace".to_string()],
            commit_policy: "single commit".to_string(),
            reporting_contract: "print commit sha".to_string(),
            escalation_policy: "manual escalation".to_string(),
        };

        let task = registry
            .create_from_packet(packet.clone())
            .expect("packet-backed task should be created");

        assert_eq!(task.prompt, packet.objective);
        assert_eq!(task.subject, packet.objective);
        assert_eq!(task.description.as_deref(), Some("runtime/task system"));
        assert_eq!(task.task_packet, Some(packet.clone()));

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_packet, Some(packet));
    }

    #[test]
    fn lists_tasks_with_optional_filter() {
        let registry = TaskRegistry::new();
        registry.create("Task A", None);
        let task_b = registry.create("Task B", None);
        registry
            .set_status(&task_b.task_id, TaskStatus::Running)
            .expect("set status should succeed");

        let all = registry.list(None);
        assert_eq!(all.len(), 2);

        let running = registry.list(Some(TaskStatus::Running));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task_id, task_b.task_id);

        let created = registry.list(Some(TaskStatus::Created));
        assert_eq!(created.len(), 1);
    }

    #[test]
    fn stops_running_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Stoppable", None);
        registry
            .set_status(&task.task_id, TaskStatus::Running)
            .unwrap();

        let stopped = registry.stop(&task.task_id).expect("stop should succeed");
        assert_eq!(stopped.status, TaskStatus::Stopped);

        // Stopping again should fail
        let result = registry.stop(&task.task_id);
        assert!(result.is_err());
    }

    #[test]
    fn updates_task_with_messages() {
        let registry = TaskRegistry::new();
        let task = registry.create("Messageable", None);
        let updated = registry
            .update(&task.task_id, "Here's more context")
            .expect("update should succeed");
        assert_eq!(updated.messages.len(), 1);
        assert_eq!(updated.messages[0].content, "Here's more context");
        assert_eq!(updated.messages[0].role, "user");
    }

    #[test]
    fn appends_and_retrieves_output() {
        let registry = TaskRegistry::new();
        let task = registry.create("Output task", None);
        registry
            .append_output(&task.task_id, "line 1\n")
            .expect("append should succeed");
        registry
            .append_output(&task.task_id, "line 2\n")
            .expect("append should succeed");

        let output = registry.output(&task.task_id).expect("output should exist");
        assert_eq!(output, "line 1\nline 2\n");
    }

    #[test]
    fn removes_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Task", None);
        let removed = registry.remove(&task.task_id);
        assert!(removed.is_some());
        assert!(registry.get(&task.task_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn rejects_operations_on_missing_task() {
        let registry = TaskRegistry::new();
        assert!(registry.stop("nonexistent").is_err());
        assert!(registry.update("nonexistent", "msg").is_err());
        assert!(registry.output("nonexistent").is_err());
        assert!(registry.append_output("nonexistent", "data").is_err());
        assert!(registry
            .set_status("nonexistent", TaskStatus::Running)
            .is_err());
    }

    #[test]
    fn task_status_display_all_variants() {
        let cases = [
            (TaskStatus::Pending, "pending"),
            (TaskStatus::InProgress, "in_progress"),
            (TaskStatus::Created, "created"),
            (TaskStatus::Running, "running"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Stopped, "stopped"),
        ];

        let rendered: Vec<_> = cases
            .into_iter()
            .map(|(status, expected)| (status.to_string(), expected))
            .collect();

        assert_eq!(
            rendered,
            vec![
                ("pending".to_string(), "pending"),
                ("in_progress".to_string(), "in_progress"),
                ("created".to_string(), "created"),
                ("running".to_string(), "running"),
                ("completed".to_string(), "completed"),
                ("failed".to_string(), "failed"),
                ("stopped".to_string(), "stopped"),
            ]
        );
    }

    #[test]
    fn stop_rejects_completed_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("done", None);
        registry
            .set_status(&task.task_id, TaskStatus::Completed)
            .expect("set status should succeed");

        let result = registry.stop(&task.task_id);

        let error = result.expect_err("completed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("completed"));
    }

    #[test]
    fn stop_rejects_failed_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("failed", None);
        registry
            .set_status(&task.task_id, TaskStatus::Failed)
            .expect("set status should succeed");

        let result = registry.stop(&task.task_id);

        let error = result.expect_err("failed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("failed"));
    }

    #[test]
    fn stop_succeeds_from_created_state() {
        let registry = TaskRegistry::new();
        let task = registry.create("created task", None);

        let stopped = registry.stop(&task.task_id).expect("stop should succeed");

        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert!(stopped.updated_at >= task.updated_at);
    }

    #[test]
    fn new_registry_is_empty() {
        let registry = TaskRegistry::new();

        let all_tasks = registry.list(None);

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(all_tasks.is_empty());
    }

    #[test]
    fn create_without_description() {
        let registry = TaskRegistry::new();

        let task = registry.create("Do the thing", None);

        assert!(task.task_id.starts_with("task_"));
        assert_eq!(task.description, None);
        assert_eq!(task.task_packet, None);
        assert!(task.messages.is_empty());
        assert!(task.output.is_empty());
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let registry = TaskRegistry::new();

        let removed = registry.remove("missing");

        assert!(removed.is_none());
    }

    #[test]
    fn persistence_round_trip() {
        let dir = std::env::temp_dir().join(format!("task_reg_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tasks.json");
        let _ = std::fs::remove_file(&path);

        // Create tasks with persistence
        let registry = TaskRegistry::load(&path);
        let t1 = registry.create_with_subject(
            "First",
            Some("Do first"),
            Some("Doing first"),
            Vec::new(),
        );
        registry
            .set_status(&t1.task_id, TaskStatus::InProgress)
            .unwrap();
        let _t2 = registry.create_with_subject("Second", None, None, Vec::new());

        // Load into a new registry
        let registry2 = TaskRegistry::load(&path);
        let tasks = registry2.list(None);
        assert_eq!(tasks.len(), 2);

        let reloaded = registry2
            .get(&t1.task_id)
            .expect("task should survive reload");
        assert_eq!(reloaded.subject, "First");
        assert_eq!(reloaded.status, TaskStatus::InProgress);
        assert_eq!(reloaded.active_form.as_deref(), Some("Doing first"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_fields_changes_subject_and_status() {
        let registry = TaskRegistry::new();
        let task = registry.create_with_subject("Original", Some("desc"), None, Vec::new());

        let updated = registry
            .update_fields(
                &task.task_id,
                Some("Renamed"),
                None,
                Some("Renaming"),
                Some(TaskStatus::Completed),
                None,
            )
            .unwrap();

        assert_eq!(updated.subject, "Renamed");
        assert_eq!(updated.active_form.as_deref(), Some("Renaming"));
        assert_eq!(updated.status, TaskStatus::Completed);
        assert_eq!(updated.description.as_deref(), Some("desc"));
    }

    #[test]
    fn pending_and_in_progress_statuses() {
        let registry = TaskRegistry::new();
        let task = registry.create_with_subject("My task", None, None, Vec::new());
        assert_eq!(task.status, TaskStatus::Pending);

        registry
            .set_status(&task.task_id, TaskStatus::InProgress)
            .unwrap();
        let fetched = registry.get(&task.task_id).unwrap();
        assert_eq!(fetched.status, TaskStatus::InProgress);
    }

    #[test]
    fn creates_task_with_dependencies() {
        let registry = TaskRegistry::new();
        let t1 = registry.create_with_subject("First", None, None, Vec::new());
        let t2 = registry.create_with_subject("Second", None, None, vec![t1.task_id.clone()]);
        assert_eq!(t2.dependencies, vec![t1.task_id]);
    }

    #[test]
    fn update_fields_sets_dependencies() {
        let registry = TaskRegistry::new();
        let t1 = registry.create_with_subject("A", None, None, Vec::new());
        let t2 = registry.create_with_subject("B", None, None, Vec::new());
        let updated = registry
            .update_fields(
                &t2.task_id,
                None,
                None,
                None,
                None,
                Some(vec![t1.task_id.clone()]),
            )
            .unwrap();
        assert_eq!(updated.dependencies, vec![t1.task_id]);
    }

    #[test]
    fn validate_dependencies_rejects_missing() {
        let registry = TaskRegistry::new();
        let result = registry.validate_dependencies(&["nonexistent".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn persistence_preserves_dependencies() {
        let dir = std::env::temp_dir().join(format!("task_dep_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("tasks.json");
        let _ = std::fs::remove_file(&path);

        let registry = TaskRegistry::load(&path);
        let t1 = registry.create_with_subject("First", None, None, Vec::new());
        let _t2 = registry.create_with_subject("Second", None, None, vec![t1.task_id.clone()]);

        let registry2 = TaskRegistry::load(&path);
        let tasks = registry2.list(None);
        let t2_reloaded = tasks.iter().find(|t| t.subject == "Second").unwrap();
        assert_eq!(t2_reloaded.dependencies, vec![t1.task_id]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
