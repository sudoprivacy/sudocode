//! File path redirection for draft files.
//!
//! This module handles redirecting draft files to the `.drafts/` directory
//! and managing naming conflicts.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default drafts directory name.
pub const DRAFTS_DIR_NAME: &str = ".drafts";

/// Redirect a draft file to the `.drafts/` directory.
///
/// If the file already exists in `.drafts/`, append a timestamp to avoid collision.
pub fn redirect_to_drafts(requested_path: &Path, workspace_root: &Path) -> PathBuf {
    let drafts_dir = workspace_root.join(DRAFTS_DIR_NAME);

    // Ensure .drafts/ directory exists
    if !drafts_dir.exists() {
        if let Err(_e) = fs::create_dir_all(&drafts_dir) {
            // Fallback: return original path
            return requested_path.to_path_buf();
        }
    }

    // Extract file name
    let file_name = requested_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("draft");

    let mut dest_path = drafts_dir.join(file_name);

    // Handle naming collision
    if dest_path.exists() {
        let stem = requested_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("draft");
        let ext = requested_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e))
            .unwrap_or_default();

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        dest_path = drafts_dir.join(format!("{}_{}{}", stem, timestamp, ext));
    }

    dest_path
}

/// Check if a path is inside the `.drafts/` directory.
pub fn is_in_drafts(path: &Path, workspace_root: &Path) -> bool {
    let drafts_dir = workspace_root.join(DRAFTS_DIR_NAME);
    path.starts_with(&drafts_dir)
}

/// Get the drafts directory path for a workspace.
pub fn get_drafts_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(DRAFTS_DIR_NAME)
}

/// Clean up old draft files (optional utility).
///
/// Remove draft files older than `max_age_days` days.
pub fn cleanup_old_drafts(workspace_root: &Path, max_age_days: u64) -> Vec<PathBuf> {
    let drafts_dir = workspace_root.join(DRAFTS_DIR_NAME);
    let mut removed = Vec::new();

    if !drafts_dir.exists() {
        return removed;
    }

    let now = SystemTime::now();
    let max_age = std::time::Duration::from_secs(max_age_days * 24 * 60 * 60);

    if let Ok(entries) = fs::read_dir(&drafts_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(modified) = metadata.modified() {
                        if let Ok(age) = now.duration_since(modified) {
                            if age > max_age {
                                let _ = fs::remove_file(&path);
                                removed.push(path);
                            }
                        }
                    }
                }
            }
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_redirect_to_drafts() {
        let tmp_dir = std::env::temp_dir().join(format!("scode-test-{}", std::process::id()));
        fs::create_dir_all(&tmp_dir).expect("create temp dir");

        let requested = tmp_dir.join("temp_script.py");
        let result = redirect_to_drafts(&requested, &tmp_dir);

        assert!(result.starts_with(&tmp_dir.join(DRAFTS_DIR_NAME)));
        assert_eq!(result.file_name().unwrap(), "temp_script.py");

        // Cleanup
        fs::remove_dir_all(&tmp_dir).expect("cleanup");
    }

    #[test]
    fn test_redirect_with_collision() {
        let dir_name = format!("scode-test-collision-{}", std::process::id());
        let tmp_dir = std::env::temp_dir().join(dir_name);
        fs::create_dir_all(&tmp_dir).expect("create temp dir");

        // Create existing file in drafts
        let drafts_dir = tmp_dir.join(DRAFTS_DIR_NAME);
        fs::create_dir_all(&drafts_dir).expect("create drafts dir");
        fs::write(drafts_dir.join("temp.py"), "").expect("write existing file");

        let requested = tmp_dir.join("temp.py");
        let result = redirect_to_drafts(&requested, &tmp_dir);

        // Should have timestamp suffix
        assert!(result.starts_with(&drafts_dir));
        let name = result.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("temp_"));
        assert!(name.ends_with(".py"));

        // Cleanup
        fs::remove_dir_all(&tmp_dir).expect("cleanup");
    }

    #[test]
    fn test_is_in_drafts() {
        let workspace = Path::new("/workspace");
        let drafts_file = workspace.join(".drafts/temp.py");
        let root_file = workspace.join("report.md");

        assert!(is_in_drafts(&drafts_file, workspace));
        assert!(!is_in_drafts(&root_file, workspace));
    }

    #[test]
    fn test_get_drafts_dir() {
        let workspace = Path::new("/workspace");
        let drafts = get_drafts_dir(workspace);
        assert_eq!(drafts, workspace.join(".drafts"));
    }
}
