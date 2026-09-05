//! Session construction, resolution, and persistence — the engine-side store
//! surface.
//!
//! These helpers create a fresh [`Session`], resolve a user-supplied reference
//! (`latest` / an id / a path) to a [`SessionHandle`], load a persisted session,
//! delete a managed session (GC-ing its offloaded tool-results subtree), and
//! back a session up before a `/clear`. They wrap `runtime::SessionStore`, which
//! is an engine input (the transcript store the turn loop reads and writes), not
//! a rendering concern — so both the REPL and `engine-acp` build/resolve a
//! session through here. The renderer keeps only the session-list / picker /
//! confirmation UI (`cli::session`), which drives these by calling them.

use std::fs;
use std::path::{Path, PathBuf};

use runtime::{Session, SessionStore};

/// A resolved session's identity and on-disk path. A leaf value — it holds no
/// runtime; the core session type carries a `SessionHandle` alongside its
/// `BuiltRuntime` as sibling fields.
#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub id: String,
    pub path: PathBuf,
}

pub fn current_session_store() -> Result<SessionStore, Box<dyn std::error::Error>> {
    let cwd = runtime::current_workspace_root()?;
    SessionStore::from_cwd(&cwd).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

pub fn new_cli_session() -> Result<Session, Box<dyn std::error::Error>> {
    new_cli_session_for(&runtime::current_workspace_root()?)
}

pub fn new_cli_session_for(cwd: &Path) -> Result<Session, Box<dyn std::error::Error>> {
    Ok(Session::new().with_workspace_root(cwd.to_path_buf()))
}

pub fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let cwd = runtime::current_workspace_root()?;
    create_managed_session_handle_for(&cwd, session_id)
}

pub fn create_managed_session_handle_for(
    cwd: &Path,
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = SessionStore::from_cwd(cwd)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
        .create_handle(session_id);
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

pub fn resolve_session_reference(
    reference: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let handle = current_session_store()?
        .resolve_reference(reference)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(SessionHandle {
        id: handle.id,
        path: handle.path,
    })
}

pub fn load_session_reference(
    reference: &str,
) -> Result<(SessionHandle, Session), Box<dyn std::error::Error>> {
    let loaded = current_session_store()?
        .load_session(reference)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok((
        SessionHandle {
            id: loaded.handle.id,
            path: loaded.handle.path,
        },
        loaded.session,
    ))
}

pub fn delete_managed_session(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("session file does not exist: {}", path.display()).into());
    }
    fs::remove_file(path)?;
    // GC the session's offloaded tool-results subtree (write-once blobs spilled
    // from oversized tool outputs). The managed file is `<session-id>.jsonl`, so
    // its stem is the session-id that namespaces the offload dir. Best-effort:
    // an absent dir (session never offloaded anything) is not an error.
    if let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) {
        if let Some(dir) = Session::tool_results_dir_for(path, session_id) {
            let _ = fs::remove_dir_all(dir);
        }
    }
    Ok(())
}

pub fn write_session_clear_backup(
    session: &Session,
    session_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let backup_path = session_clear_backup_path(session_path);
    session.save_to_path(&backup_path)?;
    Ok(backup_path)
}

fn session_clear_backup_path(session_path: &Path) -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_millis());
    let file_name = session_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("session.jsonl");
    session_path.with_file_name(format!("{file_name}.before-clear-{timestamp}.bak"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn delete_managed_session_gcs_offloaded_tool_results() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let sessions_dir = std::env::temp_dir().join(format!("scode-del-gc-{nanos}"));
        fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let session_id = "sess-abc-123";
        let session_file = sessions_dir.join(format!("{session_id}.jsonl"));
        fs::write(&session_file, b"{}\n").expect("write session file");

        // An offloaded blob under the session's namespaced tool-results dir.
        let offload_dir =
            Session::tool_results_dir_for(&session_file, session_id).expect("offload dir");
        fs::create_dir_all(&offload_dir).expect("offload dir");
        fs::write(offload_dir.join("tool-1"), b"big output").expect("write blob");
        assert!(offload_dir.exists());

        delete_managed_session(&session_file).expect("delete should succeed");

        // Both the transcript and the offloaded subtree are gone.
        assert!(!session_file.exists());
        assert!(!offload_dir.exists());

        let _ = fs::remove_dir_all(&sessions_dir);
    }
}
