use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use runtime::{Session, SessionStore};

use crate::cli::lifecycle::{classify_session_lifecycle_for, SessionLifecycleSummary};

pub(crate) const LATEST_SESSION_REFERENCE: &str = "latest";
pub(crate) const SESSION_REFERENCE_ALIASES: &[&str] = &[LATEST_SESSION_REFERENCE, "last", "recent"];

#[derive(Debug, Clone)]
pub(crate) struct SessionHandle {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedSessionSummary {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) updated_at_ms: u64,
    pub(crate) modified_epoch_millis: u128,
    pub(crate) message_count: usize,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) branch_name: Option<String>,
    pub(crate) lifecycle: SessionLifecycleSummary,
}

pub(crate) fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(current_session_store()?.sessions_dir().to_path_buf())
}

pub(crate) fn current_session_store() -> Result<SessionStore, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    SessionStore::from_cwd(&cwd).map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

pub(crate) fn new_cli_session() -> Result<Session, Box<dyn std::error::Error>> {
    new_cli_session_for(&env::current_dir()?)
}

pub(crate) fn new_cli_session_for(cwd: &Path) -> Result<Session, Box<dyn std::error::Error>> {
    Ok(Session::new().with_workspace_root(cwd.to_path_buf()))
}

pub(crate) fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    create_managed_session_handle_for(&cwd, session_id)
}

pub(crate) fn create_managed_session_handle_for(
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

pub(crate) fn resolve_session_reference(
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

pub(crate) fn resolve_managed_session_path(
    session_id: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    current_session_store()?
        .resolve_managed_path(session_id)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
}

pub(crate) fn list_managed_sessions(
) -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let store = current_session_store()?;
    let lifecycle = classify_session_lifecycle_for(store.workspace_root());
    Ok(store
        .list_sessions()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?
        .into_iter()
        .map(|session| ManagedSessionSummary {
            id: session.id,
            path: session.path,
            updated_at_ms: session.updated_at_ms,
            modified_epoch_millis: session.modified_epoch_millis,
            message_count: session.message_count,
            parent_session_id: session.parent_session_id,
            branch_name: session.branch_name,
            lifecycle: lifecycle.clone(),
        })
        .collect())
}

pub(crate) fn latest_managed_session() -> Result<ManagedSessionSummary, Box<dyn std::error::Error>>
{
    let store = current_session_store()?;
    let lifecycle = classify_session_lifecycle_for(store.workspace_root());
    let session = store
        .latest_session()
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(ManagedSessionSummary {
        id: session.id,
        path: session.path,
        updated_at_ms: session.updated_at_ms,
        modified_epoch_millis: session.modified_epoch_millis,
        message_count: session.message_count,
        parent_session_id: session.parent_session_id,
        branch_name: session.branch_name,
        lifecycle,
    })
}

pub(crate) fn load_session_reference(
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

pub(crate) fn delete_managed_session(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("session file does not exist: {}", path.display()).into());
    }
    fs::remove_file(path)?;
    Ok(())
}

pub(crate) fn confirm_session_deletion(session_id: &str) -> bool {
    print!("Delete session '{session_id}'? This cannot be undone. [y/N]: ");
    io::stdout().flush().unwrap_or(());
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

pub(crate) fn render_session_list(
    active_session_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let sessions = list_managed_sessions()?;
    let mut lines = vec![
        "Sessions".to_string(),
        format!("  Directory         {}", sessions_dir()?.display()),
    ];
    if sessions.is_empty() {
        lines.push("  No managed sessions saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    for session in sessions {
        let marker = if session.id == active_session_id {
            "● current"
        } else {
            "○ saved"
        };
        let lineage = match (
            session.branch_name.as_deref(),
            session.parent_session_id.as_deref(),
        ) {
            (Some(branch_name), Some(parent_session_id)) => {
                format!(" branch={branch_name} from={parent_session_id}")
            }
            (None, Some(parent_session_id)) => format!(" from={parent_session_id}"),
            (Some(branch_name), None) => format!(" branch={branch_name}"),
            (None, None) => String::new(),
        };
        lines.push(format!(
            "  {id:<20} {marker:<10} lifecycle={lifecycle} msgs={msgs:<4} modified={modified}{lineage} path={path}",
            id = session.id,
            lifecycle = session.lifecycle.signal(),
            msgs = session.message_count,
            modified = format_session_modified_age(session.modified_epoch_millis),
            lineage = lineage,
            path = session.path.display(),
        ));
    }
    Ok(lines.join("\n"))
}

/// Compact one-line description of a session for the interactive picker.
///
/// Excludes ANSI styling because `dialoguer::FuzzySelect` matches against the
/// raw string, and escape characters would be visible in the fuzzy filter.
pub(crate) fn format_session_picker_entry(
    session: &ManagedSessionSummary,
    active_session_id: &str,
) -> String {
    let marker = if session.id == active_session_id {
        "●"
    } else {
        "○"
    };
    let lineage = match (
        session.branch_name.as_deref(),
        session.parent_session_id.as_deref(),
    ) {
        (Some(branch_name), Some(parent_session_id)) => {
            format!(" branch={branch_name} from={parent_session_id}")
        }
        (None, Some(parent_session_id)) => format!(" from={parent_session_id}"),
        (Some(branch_name), None) => format!(" branch={branch_name}"),
        (None, None) => String::new(),
    };
    format!(
        "{marker} {id:<20}  msgs={msgs:<4}  modified={modified}  lifecycle={lifecycle}{lineage}",
        id = session.id,
        msgs = session.message_count,
        modified = format_session_modified_age(session.modified_epoch_millis),
        lifecycle = session.lifecycle.signal(),
    )
}

pub(crate) fn format_session_modified_age(modified_epoch_millis: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(modified_epoch_millis, |duration| duration.as_millis());
    let delta_seconds = now
        .saturating_sub(modified_epoch_millis)
        .checked_div(1_000)
        .unwrap_or_default();
    match delta_seconds {
        0..=4 => "just-now".to_string(),
        5..=59 => format!("{delta_seconds}s-ago"),
        60..=3_599 => format!("{}m-ago", delta_seconds / 60),
        3_600..=86_399 => format!("{}h-ago", delta_seconds / 3_600),
        _ => format!("{}d-ago", delta_seconds / 86_400),
    }
}

pub(crate) fn write_session_clear_backup(
    session: &Session,
    session_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let backup_path = session_clear_backup_path(session_path);
    session.save_to_path(&backup_path)?;
    Ok(backup_path)
}

pub(crate) fn session_clear_backup_path(session_path: &Path) -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_millis());
    let file_name = session_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("session.jsonl");
    session_path.with_file_name(format!("{file_name}.before-clear-{timestamp}.bak"))
}
