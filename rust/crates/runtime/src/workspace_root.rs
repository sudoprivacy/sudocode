//! Per-turn workspace root — the directory a turn resolves relative paths,
//! subprocess working directories, project config and project state
//! (`.sudocode-*`, `.nexus/`, memory, session store) against.
//!
//! Historically every one of those sites read `std::env::current_dir()`,
//! and the ACP server made that work by `set_current_dir`-ing to the
//! session's directory before each turn. The process cwd is a single global,
//! so two sessions in different directories could never run turns at the
//! same time: one held the cwd for its whole turn (a 60 s `bash`, a long
//! report) and every other session's turn queued behind it.
//!
//! This module replaces the process cwd with a *thread-scoped* root:
//!
//! * a turn (or any other unit of session work) wraps itself in a
//!   [`WorkspaceRootScope`] naming the session's directory;
//! * every site that used to read the process cwd calls
//!   [`current_workspace_root`] instead, which returns the scoped root when
//!   one is set on the calling thread and falls back to the process cwd
//!   otherwise (the REPL and one-shot CLI paths, which never enter a scope,
//!   behave exactly as before);
//! * work that a turn hands to another thread carries the root across via
//!   [`WorkspaceRootHandoff`].
//!
//! Why thread-local rather than task-local: the conversation runtime drives
//! a turn with `Runtime::block_on`, which polls the turn future on the
//! calling thread, and tool execution ([`crate::ToolExecutor::execute`]) is
//! synchronous on that same thread. Nothing on the turn path is `tokio::spawn`ed
//! (the runtime's spawned tasks are MCP transports and HTTP streams, none of
//! which resolve paths). The only threads a turn creates are the sub-agent
//! workers and the WebFetch/WebSearch helper threads; the former re-enter
//! the scope from a handoff, the latter touch no paths.
//!
//! **Invariant:** outside test code, `runtime`, `tools`, `api` and `commands`
//! contain no `std::env::current_dir()` — every path anchor goes through this
//! module, so a turn can only ever see its own session's directory.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

thread_local! {
    static WORKSPACE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The workspace root the calling thread currently works against.
///
/// Returns the innermost active [`WorkspaceRootScope`] on this thread, or the
/// process working directory when no scope is active. This is the one
/// function that replaces `std::env::current_dir()` on every path a turn can
/// reach.
///
/// # Errors
///
/// Propagates the `current_dir` error when no scope is active and the
/// process cwd cannot be read (deleted directory, permissions).
pub fn current_workspace_root() -> io::Result<PathBuf> {
    match scoped_workspace_root() {
        Some(root) => Ok(root),
        None => std::env::current_dir(),
    }
}

/// Infallible variant of [`current_workspace_root`]: falls back to an empty
/// path (which then behaves like a relative path against the process cwd),
/// mirroring the `unwrap_or_default()` idiom the call sites used to apply to
/// `current_dir()`.
#[must_use]
pub fn current_workspace_root_or_default() -> PathBuf {
    current_workspace_root().unwrap_or_default()
}

/// The root of the innermost active [`WorkspaceRootScope`] on this thread,
/// or `None` when the thread is not inside a scope. Prefer
/// [`current_workspace_root`]; this exists for code that needs to know
/// whether it is running under a scope at all.
#[must_use]
pub fn scoped_workspace_root() -> Option<PathBuf> {
    WORKSPACE_ROOT.with(|cell| cell.borrow().clone())
}

/// RAII guard that makes `root` the workspace root of the current thread for
/// as long as it lives. Scopes nest: dropping the guard restores whatever was
/// active before it was entered.
#[must_use = "the workspace root is only in effect while the scope is alive"]
#[derive(Debug)]
pub struct WorkspaceRootScope {
    root: PathBuf,
    previous: Option<PathBuf>,
}

impl WorkspaceRootScope {
    /// Enter `root` as this thread's workspace root.
    pub fn enter(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let previous = WORKSPACE_ROOT.with(|cell| cell.borrow_mut().replace(root.clone()));
        Self { root, previous }
    }

    /// The root this scope installed.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for WorkspaceRootScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        WORKSPACE_ROOT.with(|cell| {
            *cell.borrow_mut() = previous;
        });
    }
}

/// Snapshot of the calling thread's workspace root, meant to be moved into a
/// closure that runs on another thread (`std::thread::spawn`,
/// `spawn_blocking`, …) and re-entered there. A handoff captured outside any
/// scope re-enters nothing, so the receiving thread keeps falling back to
/// the process cwd — same as before.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceRootHandoff {
    root: Option<PathBuf>,
}

impl WorkspaceRootHandoff {
    /// Capture the current thread's scoped root (if any).
    #[must_use]
    pub fn capture() -> Self {
        Self {
            root: scoped_workspace_root(),
        }
    }

    /// A handoff that installs `root` explicitly, for jobs that already
    /// carry their workspace directory.
    #[must_use]
    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
        }
    }

    /// The captured root, if any.
    #[must_use]
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Re-enter the captured root on the current thread. Returns `None` (and
    /// installs nothing) when the handoff was captured outside a scope.
    #[must_use]
    pub fn enter(&self) -> Option<WorkspaceRootScope> {
        self.root.clone().map(WorkspaceRootScope::enter)
    }
}
