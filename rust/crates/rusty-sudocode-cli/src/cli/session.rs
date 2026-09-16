use std::io::{self, Write};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use engine_host::session::current_session_store;

use crate::cli::lifecycle::{classify_session_lifecycle_for, SessionLifecycleSummary};

pub(crate) const LATEST_SESSION_REFERENCE: &str = "latest";

#[derive(Debug, Clone)]
pub(crate) struct ManagedSessionSummary {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) updated_at_ms: u64,
    pub(crate) modified_epoch_millis: u128,
    pub(crate) message_count: usize,
    pub(crate) summary: Option<String>,
    pub(crate) parent_session_id: Option<String>,
    pub(crate) branch_name: Option<String>,
    pub(crate) lifecycle: SessionLifecycleSummary,
}

pub(crate) fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(current_session_store()?.sessions_dir().to_path_buf())
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
            summary: session.summary,
            parent_session_id: session.parent_session_id,
            branch_name: session.branch_name,
            lifecycle: lifecycle.clone(),
        })
        .collect())
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
        let summary = session
            .summary
            .as_deref()
            .map(format_session_summary)
            .filter(|summary| !summary.is_empty())
            .map(|summary| format!(" summary={summary}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {id:<20} {marker:<10} lifecycle={lifecycle} msgs={msgs:<4} modified={modified}{lineage}{summary} path={path}",
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
    let summary = session
        .summary
        .as_deref()
        .map(format_session_summary)
        .filter(|summary| !summary.is_empty())
        .map(|summary| format!("  summary={summary}"))
        .unwrap_or_default();
    format!(
        "{marker} {id:<20}  msgs={msgs:<4}  modified={modified}  lifecycle={lifecycle}{lineage}{summary}",
        id = session.id,
        msgs = session.message_count,
        modified = format_session_modified_age(session.modified_epoch_millis),
        lifecycle = session.lifecycle.signal(),
    )
}

pub(crate) fn format_session_summary(summary: &str) -> String {
    let summary = summary
        .lines()
        .map(str::trim)
        .find(|line| {
            !line.is_empty()
                && !matches!(
                    *line,
                    "<summary>" | "</summary>" | "Summary:" | "Conversation summary:"
                )
        })
        .unwrap_or("")
        .trim_start_matches(['-', '*', ' ']);
    let mut end = summary.len();
    for (index, _) in summary.char_indices().nth(96).into_iter() {
        end = index;
    }
    if end < summary.len() {
        format!("{}…", &summary[..end])
    } else {
        summary.to_string()
    }
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
