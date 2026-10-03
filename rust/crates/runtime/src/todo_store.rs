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
///
/// Built per use, not once per process: a daemon hosts many agents and an ACP
/// process serves many sessions, and a single global handed all of them the same
/// list. Every mutation persists, so a fresh handle over the same path is the
/// same store.
#[derive(Clone, Default)]
pub struct TodoStore {
    inner: Arc<Mutex<StoreInner>>,
}

#[derive(Default)]
struct StoreInner {
    todos: Vec<Todo>,
    store_path: Option<PathBuf>,
    /// Where a save goes. `None` is the host disk, which is what a store built
    /// without one has always meant.
    fs: Option<Arc<dyn crate::fs_backend::FsBackend>>,
}

/// The file a todo list is persisted to.
///
/// One name for both hosts; only the directory differs, which is the whole
/// difference between a CLI session and a co-hosted one.
const TODO_FILE: &str = ".sudocode-todos.json";

/// Where the list is persisted, and the filesystem that path is spelled for.
///
/// One decision, because the two cannot be chosen apart — see
/// [`crate::fs_backend::host_fs`] for the rule. In order:
///
/// 1. `$SUDOCODE_TODO_STORE`, on the HOST: an operator typed that path, and it
///    may name a file outside anything this session's filesystem can reach.
/// 2. the root `fs` gives for [`ManagedRoot::Todos`], on `fs`: what stops a
///    co-hosted agent's list from landing on the daemon's local disk at a path
///    derived from the daemon's own directory — which every co-hosted agent
///    shares, so they were overwriting each other's todos.
/// 3. `<working root>/.sudocode-todos.json`, on the HOST: the layout a standalone
///    session has always had, in the place it has always had it.
fn todo_store_location(
    fs: &Arc<dyn crate::fs_backend::FsBackend>,
) -> Result<(PathBuf, Arc<dyn crate::fs_backend::FsBackend>), String> {
    let host = || Arc::clone(crate::fs_backend::host_fs_arc());
    if let Ok(path) = std::env::var("SUDOCODE_TODO_STORE") {
        return Ok((PathBuf::from(path), host()));
    }
    if let Some(root) = fs.managed_root(crate::fs_backend::ManagedRoot::Todos) {
        return Ok((PathBuf::from(root).join(TODO_FILE), Arc::clone(fs)));
    }
    // The session's own working root, not the process's: one source for where
    // the workspace is, which for a host-spelled kernel session is not the
    // process directory.
    let root = fs.working_root().map_err(|error| error.to_string())?;
    Ok((PathBuf::from(root).join(TODO_FILE), host()))
}

/// Where the list is persisted. The path half of [`todo_store_location`], for a
/// caller that wants to name the file rather than read it.
pub fn todo_store_path(fs: &Arc<dyn crate::fs_backend::FsBackend>) -> Result<PathBuf, String> {
    todo_store_location(fs).map(|(path, _)| path)
}

impl TodoStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The store this session's todos live in, on the filesystem that roots it
    /// — see [`todo_store_location`]. The one way a caller opens it: resolving
    /// the path without the filesystem it is spelled for is how a write ends up
    /// aimed at a namespace that cannot hold it.
    #[must_use]
    pub fn open(fs: &Arc<dyn crate::fs_backend::FsBackend>) -> Self {
        match todo_store_location(fs) {
            Ok((path, on)) => Self::load(&path, on),
            // No working root means no path to persist to. An in-memory list is
            // still a working TodoWrite, which is better than refusing the tool
            // over a directory question.
            Err(_) => Self::new(),
        }
    }

    /// Load a persisted list from `path`. A missing / unreadable / unparseable
    /// file yields an empty store bound to that path (not an error).
    #[must_use]
    pub fn load(path: &Path, fs: Arc<dyn crate::fs_backend::FsBackend>) -> Self {
        let todos = fs
            .read_to_string(&path.to_string_lossy())
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Todo>>(&text).ok())
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(StoreInner {
                todos,
                store_path: Some(path.to_owned()),
                fs: Some(fs),
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
        let body = serde_json::to_string_pretty(&inner.todos).unwrap_or_default();
        let path = path.to_string_lossy();
        // The store's own filesystem, so a co-hosted agent's list lands where its
        // path says rather than on the daemon's disk. `None` is a store built
        // without one (`TodoStore::new`), which has always meant the host.
        let fs: &dyn crate::fs_backend::FsBackend = match &inner.fs {
            Some(fs) => fs.as_ref(),
            None => crate::fs_backend::host_fs(),
        };
        if let Some(parent) = std::path::Path::new(path.as_ref()).parent() {
            let _ = fs.create_dir_all(&parent.to_string_lossy());
        }
        if let Err(error) = fs.write(&path, body.as_bytes()) {
            // Not swallowed: a list the model believes it saved and that is gone
            // next turn is worse than a line on stderr. A refused write looked
            // exactly like a successful one before this.
            eprintln!("sudocode: failed to persist the todo list to {path}: {error}");
        }
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

        let store = TodoStore::load(&path, Arc::new(crate::fs_backend::StdFsBackend));
        store.set(vec![
            todo("first", TodoStatus::Completed),
            todo("second", TodoStatus::InProgress),
        ]);

        let reloaded = TodoStore::load(&path, Arc::new(crate::fs_backend::StdFsBackend));
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
        let store = TodoStore::load(&path, Arc::new(crate::fs_backend::StdFsBackend));
        assert!(store.is_empty());
    }
}
