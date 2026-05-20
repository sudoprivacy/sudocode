//! File snapshot scanning for detecting file changes during Bash execution.
//!
//! This module provides before/after scanning to detect files created or modified
//! by shell commands.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Directories to skip during scanning.
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", "vendor", ".venv", "venv", "__pycache__",
    ".drafts",
];

/// File names to skip during scanning.
const SKIP_FILES: &[&str] = &[".DS_Store", "Thumbs.db"];

/// File change snapshot.
#[derive(Debug, Default)]
pub struct FileChangeSnapshot {
    /// Files before execution.
    pub before: HashSet<PathBuf>,

    /// Files after execution.
    pub after: HashSet<PathBuf>,

    /// New files created.
    pub created: Vec<PathBuf>,

    /// Files modified.
    pub modified: Vec<PathBuf>,

    /// Files deleted.
    pub deleted: Vec<PathBuf>,
}

impl FileChangeSnapshot {
    /// Check if a path should be ignored.
    fn should_ignore(path: &Path) -> bool {
        // Check file name
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if SKIP_FILES.contains(&name) {
                return true;
            }
        }

        // Check if any parent directory is in skip list
        for component in path.components() {
            if let std::path::Component::Normal(os_str) = component {
                if let Some(name) = os_str.to_str() {
                    if SKIP_DIRS.contains(&name) {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Scan workspace directory for files.
    pub fn scan(workspace_root: &Path) -> HashSet<PathBuf> {
        let mut files = HashSet::new();
        let mut queue = vec![workspace_root.to_path_buf()];

        while let Some(dir) = queue.pop() {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();

                    if Self::should_ignore(&path) {
                        continue;
                    }

                    if path.is_dir() {
                        queue.push(path);
                    } else if path.is_file() {
                        files.insert(path);
                    }
                }
            }
        }

        files
    }

    /// Capture snapshot before execution.
    pub fn capture_before(workspace_root: &Path) -> Self {
        Self {
            before: Self::scan(workspace_root),
            after: HashSet::new(),
            created: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        }
    }

    /// Capture snapshot after execution and compute diff.
    pub fn capture_after(&mut self, workspace_root: &Path) {
        self.after = Self::scan(workspace_root);

        // New files
        self.created = self.after.difference(&self.before).cloned().collect();

        // Deleted files
        self.deleted = self.before.difference(&self.after).cloned().collect();

        // Modified files (check mtime)
        for _path in self.before.intersection(&self.after) {
            // We can't compare mtime directly since we don't have before mtime
            // In practice, we'd need to store mtime in the before snapshot
            // For now, we skip modified detection (can be enhanced later)
        }
    }

    /// Get total change count.
    pub fn change_count(&self) -> usize {
        self.created.len() + self.modified.len() + self.deleted.len()
    }

    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        self.change_count() > 0
    }
}

/// Enhanced snapshot with mtime tracking for accurate modified detection.
#[derive(Debug, Default)]
pub struct FileChangeSnapshotWithMtime {
    /// Files with mtime before execution.
    pub before: HashMap<PathBuf, std::time::SystemTime>,

    /// Files with mtime after execution.
    pub after: HashMap<PathBuf, std::time::SystemTime>,

    /// New files created.
    pub created: Vec<PathBuf>,

    /// Files modified.
    pub modified: Vec<PathBuf>,

    /// Files deleted.
    pub deleted: Vec<PathBuf>,
}

impl FileChangeSnapshotWithMtime {
    /// Scan workspace directory for files with mtime.
    pub fn scan_with_mtime(workspace_root: &Path) -> HashMap<PathBuf, std::time::SystemTime> {
        let mut files = HashMap::new();
        let mut queue = vec![workspace_root.to_path_buf()];

        while let Some(dir) = queue.pop() {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();

                    if FileChangeSnapshot::should_ignore(&path) {
                        continue;
                    }

                    if path.is_dir() {
                        queue.push(path);
                    } else if path.is_file() {
                        if let Ok(metadata) = entry.metadata() {
                            if let Ok(mtime) = metadata.modified() {
                                files.insert(path, mtime);
                            }
                        }
                    }
                }
            }
        }

        files
    }

    /// Capture snapshot before execution.
    pub fn capture_before(workspace_root: &Path) -> Self {
        Self {
            before: Self::scan_with_mtime(workspace_root),
            after: HashMap::new(),
            created: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        }
    }

    /// Capture snapshot after execution and compute diff.
    pub fn capture_after(&mut self, workspace_root: &Path) {
        self.after = Self::scan_with_mtime(workspace_root);

        let before_paths: HashSet<_> = self.before.keys().cloned().collect();
        let after_paths: HashSet<_> = self.after.keys().cloned().collect();

        // New files
        self.created = after_paths.difference(&before_paths).cloned().collect();

        // Deleted files
        self.deleted = before_paths.difference(&after_paths).cloned().collect();

        // Modified files (mtime changed)
        for path in before_paths.intersection(&after_paths) {
            let before_mtime = self.before.get(path);
            let after_mtime = self.after.get(path);

            if let (Some(before), Some(after)) = (before_mtime, after_mtime) {
                if before != after {
                    self.modified.push(path.clone());
                }
            }
        }
    }

    /// Get total change count.
    pub fn change_count(&self) -> usize {
        self.created.len() + self.modified.len() + self.deleted.len()
    }

    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        self.change_count() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_test_workspace() -> PathBuf {
        // Use a unique temp directory for each test
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let tmp_dir = std::env::temp_dir().join(format!("scode-snapshot-test-{}-{}", std::process::id(), unique_id));

        // Ensure clean state
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir).expect("cleanup existing");
        }
        fs::create_dir_all(&tmp_dir).expect("create temp dir");

        // Verify directory is empty
        let entries: Vec<_> = fs::read_dir(&tmp_dir).unwrap().collect();
        assert!(entries.is_empty(), "temp dir should be empty");

        tmp_dir
    }

    #[test]
    fn test_scan_workspace() {
        let workspace = create_test_workspace();

        // Create some files
        fs::write(workspace.join("file1.txt"), "content1").expect("write file1");
        fs::write(workspace.join("file2.py"), "content2").expect("write file2");

        let files = FileChangeSnapshot::scan(&workspace);

        assert!(files.contains(&workspace.join("file1.txt")));
        assert!(files.contains(&workspace.join("file2.py")));
        assert_eq!(files.len(), 2);

        fs::remove_dir_all(&workspace).expect("cleanup");
    }

    #[test]
    fn test_skip_directories() {
        let workspace = create_test_workspace();

        // Create file in root
        fs::write(workspace.join("root.txt"), "content").expect("write root file");

        // Create file in node_modules (should be skipped)
        let node_modules = workspace.join("node_modules");
        fs::create_dir_all(&node_modules).expect("create node_modules");
        fs::write(node_modules.join("package.js"), "content").expect("write package");

        let files = FileChangeSnapshot::scan(&workspace);

        assert!(files.contains(&workspace.join("root.txt")));
        assert!(!files.contains(&node_modules.join("package.js")));
        assert_eq!(files.len(), 1);

        fs::remove_dir_all(&workspace).expect("cleanup");
    }

    #[test]
    fn test_capture_before_after() {
        let workspace = create_test_workspace();

        // Before: create one file
        fs::write(workspace.join("existing.txt"), "before").expect("write existing");

        let mut snapshot = FileChangeSnapshotWithMtime::capture_before(&workspace);

        // Simulate execution: create new file, modify existing
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(workspace.join("new.txt"), "new content").expect("write new");
        fs::write(workspace.join("existing.txt"), "after").expect("modify existing");

        snapshot.capture_after(&workspace);

        assert!(snapshot.created.contains(&workspace.join("new.txt")));
        assert!(snapshot.modified.contains(&workspace.join("existing.txt")));
        assert!(snapshot.has_changes());

        fs::remove_dir_all(&workspace).expect("cleanup");
    }

    #[test]
    fn test_detect_deleted() {
        let workspace = create_test_workspace();

        // Before: create two files
        fs::write(workspace.join("keep.txt"), "keep").expect("write keep");
        fs::write(workspace.join("delete.txt"), "delete").expect("write delete");

        let mut snapshot = FileChangeSnapshotWithMtime::capture_before(&workspace);

        // Simulate execution: delete one file
        fs::remove_file(workspace.join("delete.txt")).expect("delete file");

        snapshot.capture_after(&workspace);

        assert!(snapshot.deleted.contains(&workspace.join("delete.txt")));
        assert!(!snapshot.deleted.contains(&workspace.join("keep.txt")));

        fs::remove_dir_all(&workspace).expect("cleanup");
    }
}