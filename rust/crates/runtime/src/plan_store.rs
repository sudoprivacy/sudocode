//! Plan store — SSOT for the session's implementation plan.
//!
//! `write_plan` is the model's "I've designed an approach, here it is for your
//! approval" step. The plan text is the single source of truth: it is written
//! to a file (this module), shown to the user, and — on approval — fed back as
//! the execution prompt. Nothing reads the plan out of chat messages; the file
//! is authoritative.
//!
//! Path resolution (per session): `$SUDOCODE_PLAN_FILE` when the CLI set it to
//! the session-scoped path, else `<the session's working root>/.sudocode/plan.md`
//! on the session's own filesystem.

use std::path::PathBuf;
use std::sync::Arc;

use crate::fs_backend::FsBackend;

/// Shared framing for plans and delegated tasks, with stable source bytes.
#[must_use]
pub fn compose_task_framing(context: &str, constraints: &str, acceptance: &str) -> String {
    format!(
        "## Context\n{}\n\n## Constraints\n{}\n\n## Acceptance Criteria\n{}\n",
        context.trim(),
        constraints.trim(),
        acceptance.trim(),
    )
}

/// Construct the exact document saved by dispatch and reconstructed on replay.
#[must_use]
pub fn compose_document(context: &str, constraints: &str, acceptance: &str, body: &str) -> String {
    format!(
        "{}\n{}\n",
        compose_task_framing(context, constraints, acceptance),
        body.trim()
    )
}

/// Successful review decisions are tool results, not execution failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanReviewStatus {
    DraftSaved,
    Approved,
    RevisionRequested,
    Cancelled,
}

/// Shared result contract for plan dispatch, terminal cards and saved replay.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReviewResult {
    pub ok: bool,
    pub status: PlanReviewStatus,
    pub plan_file: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

impl PlanReviewResult {
    #[must_use]
    pub fn new(status: PlanReviewStatus, plan_file: String, message: String) -> Self {
        Self {
            ok: true,
            status,
            plan_file,
            message,
            feedback: None,
        }
    }
}

/// Where the plan file is, and the filesystem that path is spelled for.
///
/// One decision, because the two cannot be chosen apart — see
/// [`crate::fs_backend::host_fs`] for the rule. `$SUDOCODE_PLAN_FILE` is a HOST
/// path: the CLI points it at a session directory on disk. Otherwise the plan is
/// a workspace artifact and belongs in the session's own working root, on the
/// session's filesystem.
///
/// The process working directory is what this used to ask, and for a co-hosted
/// agent that is the DAEMON's directory — one plan file shared by every agent on
/// that daemon, so two of them planning at once overwrote each other.
fn plan_location(fs: &Arc<dyn FsBackend>) -> Result<(PathBuf, Arc<dyn FsBackend>), String> {
    if let Ok(path) = std::env::var("SUDOCODE_PLAN_FILE") {
        return Ok((
            PathBuf::from(path),
            Arc::clone(crate::fs_backend::host_fs_arc()),
        ));
    }
    let root = fs.working_root().map_err(|error| error.to_string())?;
    Ok((
        PathBuf::from(root).join(".sudocode").join("plan.md"),
        Arc::clone(fs),
    ))
}

/// Resolve the plan file for the current session. The path half of
/// [`plan_location`], for a caller that wants to name the file rather than read
/// it.
pub fn plan_file_path(fs: &Arc<dyn FsBackend>) -> Result<PathBuf, String> {
    plan_location(fs).map(|(path, _)| path)
}

/// Overwrite the plan file with `content`, creating parent dirs as needed.
/// Whole-file replace: the latest plan supersedes any earlier draft.
pub fn write_plan(content: &str, fs: &Arc<dyn FsBackend>) -> Result<PathBuf, String> {
    let (path, on) = plan_location(fs)?;
    if let Some(parent) = path.parent() {
        on.create_dir_all(&parent.to_string_lossy())
            .map_err(|e| format!("create plan dir {}: {e}", parent.display()))?;
    }
    on.write(&path.to_string_lossy(), content.as_bytes())
        .map_err(|e| format!("write plan {}: {e}", path.display()))?;
    Ok(path)
}

/// Read the current plan text, or `None` when no plan file exists or it is empty
/// (whitespace-only counts as empty — an empty plan is "no plan").
#[must_use]
pub fn read_plan(fs: &Arc<dyn FsBackend>) -> Option<String> {
    let (path, on) = plan_location(fs).ok()?;
    let text = on.read_to_string(&path.to_string_lossy()).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host filesystem, which is where `$SUDOCODE_PLAN_FILE` points these
    /// tests anyway.
    fn runtime_host() -> &'static Arc<dyn FsBackend> {
        crate::fs_backend::host_fs_arc()
    }

    /// Point the plan file at a temp path for the duration of one test.
    /// Holds the process-wide env lock because `SUDOCODE_PLAN_FILE` is global —
    /// parallel tests mutating it would race each other.
    struct ScopedPlanFile {
        dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl ScopedPlanFile {
        fn new(label: &str) -> Self {
            let guard = crate::test_env_lock();
            let dir =
                std::env::temp_dir().join(format!("plan_store_{label}_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("SUDOCODE_PLAN_FILE", dir.join("plan.md"));
            Self { dir, _guard: guard }
        }
    }
    impl Drop for ScopedPlanFile {
        fn drop(&mut self) {
            std::env::remove_var("SUDOCODE_PLAN_FILE");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let _guard = ScopedPlanFile::new("roundtrip");
        assert!(
            read_plan(runtime_host()).is_none(),
            "no plan before first write"
        );
        write_plan("## Plan\n1. do X\n2. do Y", runtime_host()).unwrap();
        assert_eq!(
            read_plan(runtime_host()).as_deref(),
            Some("## Plan\n1. do X\n2. do Y")
        );
    }

    #[test]
    fn latest_write_supersedes() {
        let _guard = ScopedPlanFile::new("supersede");
        write_plan("first draft", runtime_host()).unwrap();
        write_plan("second draft", runtime_host()).unwrap();
        assert_eq!(read_plan(runtime_host()).as_deref(), Some("second draft"));
    }

    #[test]
    fn empty_or_whitespace_plan_reads_as_none() {
        let _guard = ScopedPlanFile::new("empty");
        write_plan("   \n\t", runtime_host()).unwrap();
        assert!(
            read_plan(runtime_host()).is_none(),
            "whitespace-only plan is no plan"
        );
    }

    #[test]
    fn env_var_overrides_path() {
        let _guard = ScopedPlanFile::new("envpath");
        let path = plan_file_path(runtime_host()).unwrap();
        assert!(path.ends_with("plan.md"));
        assert!(path.to_string_lossy().contains("plan_store_envpath"));
    }
}
