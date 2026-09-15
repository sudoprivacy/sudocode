//! Todo store — SSOT for the session's todo checklist.
//!
//! Mirrors Claude Code's `TodoWrite`: the whole list is sent on every call and
//! replaces the previous one. There is no per-item id and no partial update —
//! the model always re-sends the full list, so the store is a single
//! whole-list `set`, persisted to `.sudocode-todos.json` (or `$SUDOCODE_TODO_STORE`)
//! on every write so the UI can render live progress.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Lifecycle of a single todo. The three states Claude Code exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Completed => write!(f, "completed"),
        }
    }
}

/// One todo item. Positional (no id): the list is identified by order, exactly
/// as Claude Code's `TodoWrite` sends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    /// Imperative description of the work ("Run the tests").
    pub content: String,
    pub status: TodoStatus,
    /// Present-continuous label shown while `in_progress` ("Running the tests").
    #[serde(rename = "activeForm")]
    pub active_form: String,
}

/// Whole-list todo store. Cloneable handle over shared state.
#[derive(Debug, Clone, Default)]
pub struct TodoStore {
    inner: Arc<Mutex<StoreInner>>,
}

#[derive(Debug, Default)]
struct StoreInner {
    todos: Vec<Todo>,
    store_path: Option<PathBuf>,
}

/// Resolve the on-disk store path for todo persistence.
///
/// Priority: `$SUDOCODE_TODO_STORE`, then `<workspace_root>/.sudocode-todos.json`.
pub fn todo_store_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("SUDOCODE_TODO_STORE") {
        return Ok(PathBuf::from(path));
    }
    let cwd = crate::current_workspace_root().map_err(|error| error.to_string())?;
    Ok(cwd.join(".sudocode-todos.json"))
}

impl TodoStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load a persisted list from `path`. A missing / unreadable / unparseable
    /// file yields an empty store bound to that path (not an error).
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let todos = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Todo>>(&text).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(StoreInner {
                todos,
                store_path: Some(path.to_owned()),
            })),
        }
    }

    /// Set the persistence path (enables save-on-write).
    pub fn set_store_path(&self, path: PathBuf) {
        let mut inner = self.inner.lock().expect("todo store lock poisoned");
        inner.store_path = Some(path);
    }

    /// Replace the entire list with `todos`, persist, and report which items
    /// transitioned *into* `completed` by this write (by content string) so the
    /// verification watcher can count newly-finished work. Returns the new list.
    pub fn set(&self, todos: Vec<Todo>) -> Vec<Todo> {
        let mut inner = self.inner.lock().expect("todo store lock poisoned");
        inner.todos = todos;
        Self::save(&inner);
        inner.todos.clone()
    }

    /// Snapshot the current list.
    #[must_use]
    pub fn list(&self) -> Vec<Todo> {
        let inner = self.inner.lock().expect("todo store lock poisoned");
        inner.todos.clone()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("todo store lock poisoned");
        inner.todos.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Persist the current list to the configured path (best-effort).
    fn save(inner: &StoreInner) {
        let Some(path) = &inner.store_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(&inner.todos).unwrap_or_default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(content: &str, status: TodoStatus) -> Todo {
        Todo {
            content: content.to_owned(),
            status,
            active_form: format!("Doing {content}"),
        }
    }

    #[test]
    fn set_replaces_the_whole_list() {
        let store = TodoStore::new();
        store.set(vec![
            todo("a", TodoStatus::Pending),
            todo("b", TodoStatus::Pending),
        ]);
        assert_eq!(store.len(), 2);

        // A second write replaces, not appends.
        store.set(vec![todo("c", TodoStatus::InProgress)]);
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].content, "c");
        assert_eq!(list[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn set_empty_wipes_the_list() {
        let store = TodoStore::new();
        store.set(vec![todo("a", TodoStatus::Completed)]);
        store.set(Vec::new());
        assert!(store.is_empty());
        assert!(store.list().is_empty());
    }

    #[test]
    fn active_form_serializes_as_camel_case() {
        let json = serde_json::to_string(&todo("run tests", TodoStatus::InProgress)).unwrap();
        assert!(json.contains("\"activeForm\""), "{json}");
        assert!(!json.contains("active_form"), "{json}");
    }

    #[test]
    fn status_display_matches_wire_strings() {
        assert_eq!(TodoStatus::Pending.to_string(), "pending");
        assert_eq!(TodoStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TodoStatus::Completed.to_string(), "completed");
    }

    #[test]
    fn persistence_round_trip() {
        let dir = std::env::temp_dir().join(format!("todo_store_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("todos.json");
        let _ = std::fs::remove_file(&path);

        let store = TodoStore::load(&path);
        store.set(vec![
            todo("first", TodoStatus::Completed),
            todo("second", TodoStatus::InProgress),
        ]);

        let reloaded = TodoStore::load(&path);
        let list = reloaded.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].content, "first");
        assert_eq!(list[0].status, TodoStatus::Completed);
        assert_eq!(list[1].active_form, "Doing second");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_is_empty_not_error() {
        let path = std::env::temp_dir().join(format!("todo_missing_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = TodoStore::load(&path);
        assert!(store.is_empty());
    }
}
