//! Plan store — SSOT for the session's implementation plan.
//!
//! `write_plan` is the model's "I've designed an approach, here it is for your
//! approval" step. The plan text is the single source of truth: it is written
//! to a file (this module), shown to the user, and — on approval — fed back as
//! the execution prompt. Nothing reads the plan out of chat messages; the file
//! is authoritative.
//!
//! Path resolution (per session): `$SUDOCODE_PLAN_FILE` when the CLI set it to
//! the session-scoped path, else `<workspace_root>/.sudocode/plan.md`.

use std::path::PathBuf;

/// Resolve the on-disk plan file for the current session.
///
/// Priority: `$SUDOCODE_PLAN_FILE` (the CLI points this at the session dir so
/// concurrent sessions don't collide), then `<workspace_root>/.sudocode/plan.md`.
pub fn plan_file_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("SUDOCODE_PLAN_FILE") {
        return Ok(PathBuf::from(path));
    }
    let cwd = crate::current_workspace_root().map_err(|error| error.to_string())?;
    Ok(cwd.join(".sudocode").join("plan.md"))
}

/// Overwrite the plan file with `content`, creating parent dirs as needed.
/// Whole-file replace: the latest plan supersedes any earlier draft.
pub fn write_plan(content: &str) -> Result<PathBuf, String> {
    let path = plan_file_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create plan dir {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("write plan {}: {e}", path.display()))?;
    Ok(path)
}

/// Read the current plan text, or `None` when no plan file exists or it is empty
/// (whitespace-only counts as empty — an empty plan is "no plan").
#[must_use]
pub fn read_plan() -> Option<String> {
    let path = plan_file_path().ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the plan file at a temp path for the duration of one test.
    /// Holds the process-wide env lock because `SUDOCODE_PLAN_FILE` is global —
    /// parallel tests mutating it would race each other.
    struct ScopedPlanFile {
        _dir: PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl ScopedPlanFile {
        fn new(label: &str) -> Self {
            let guard = crate::test_env_lock();
            let dir =
                std::env::temp_dir().join(format!("plan_store_{label}_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            std::env::set_var("SUDOCODE_PLAN_FILE", dir.join("plan.md"));
            Self {
                _dir: dir,
                _guard: guard,
            }
        }
    }
    impl Drop for ScopedPlanFile {
        fn drop(&mut self) {
            std::env::remove_var("SUDOCODE_PLAN_FILE");
            let _ = std::fs::remove_dir_all(&self._dir);
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let _guard = ScopedPlanFile::new("roundtrip");
        assert!(read_plan().is_none(), "no plan before first write");
        write_plan("## Plan\n1. do X\n2. do Y").unwrap();
        assert_eq!(read_plan().as_deref(), Some("## Plan\n1. do X\n2. do Y"));
    }

    #[test]
    fn latest_write_supersedes() {
        let _guard = ScopedPlanFile::new("supersede");
        write_plan("first draft").unwrap();
        write_plan("second draft").unwrap();
        assert_eq!(read_plan().as_deref(), Some("second draft"));
    }

    #[test]
    fn empty_or_whitespace_plan_reads_as_none() {
        let _guard = ScopedPlanFile::new("empty");
        write_plan("   \n\t").unwrap();
        assert!(read_plan().is_none(), "whitespace-only plan is no plan");
    }

    #[test]
    fn env_var_overrides_path() {
        let _guard = ScopedPlanFile::new("envpath");
        let path = plan_file_path().unwrap();
        assert!(path.ends_with("plan.md"));
        assert!(path.to_string_lossy().contains("plan_store_envpath"));
    }
}
