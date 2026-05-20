//! Turn-based file operation tracking.
//!
//! This module tracks file operations per turn, enabling:
//! - Precise cleanup of draft files on abort
//! - Rollback of file operations
//! - Turn-level file history

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::file_intent::{FileIntent, FileOpKind};

/// Single file operation record.
#[derive(Debug, Clone)]
pub struct FileOp {
    /// Actual file path (may differ from requested if redirected).
    pub path: PathBuf,

    /// Operation type.
    pub kind: FileOpKind,

    /// File intent.
    pub intent: FileIntent,

    /// Original content (for Edit operations, used for rollback).
    pub original_content: Option<String>,

    /// Original requested path (before any redirection).
    pub requested_path: PathBuf,
}

/// Cleanup strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStrategy {
    /// Only cleanup draft files (default).
    DraftsOnly,

    /// Rollback all file operations.
    FullRollback,

    /// No cleanup.
    None,
}

/// Cleanup result.
#[derive(Debug)]
pub enum CleanupResult {
    DraftsCleaned(Vec<PathBuf>),
    FullRollback,
    RollbackErrors(Vec<String>),
    NoAction,
}

/// Turn-based file tracker.
#[derive(Debug)]
pub struct TurnFileTracker {
    /// Current turn ID.
    current_turn_id: Option<String>,

    /// Turn ID -> file operations.
    turn_files: HashMap<String, Vec<FileOp>>,

    /// Workspace root path.
    workspace_root: PathBuf,
}

impl TurnFileTracker {
    /// Create a new tracker.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            current_turn_id: None,
            turn_files: HashMap::new(),
            workspace_root,
        }
    }

    /// Start tracking a new turn.
    pub fn start_turn(&mut self, turn_id: String) {
        self.current_turn_id = Some(turn_id);
    }

    /// End current turn.
    pub fn end_turn(&mut self) {
        self.current_turn_id = None;
    }

    /// Record a file operation.
    pub fn record(&mut self, op: FileOp) {
        if let Some(turn_id) = &self.current_turn_id {
            self.turn_files.entry(turn_id.clone()).or_default().push(op);
        }
    }

    /// Get file operations for a specific turn.
    pub fn get_turn_files(&self, turn_id: &str) -> Option<&Vec<FileOp>> {
        self.turn_files.get(turn_id)
    }

    /// Get current turn ID.
    pub fn current_turn(&self) -> Option<&str> {
        self.current_turn_id.as_deref()
    }

    /// Get current turn's file operations.
    pub fn get_current_turn_ops(&self) -> Vec<FileOp> {
        if let Some(turn_id) = &self.current_turn_id {
            self.turn_files.get(turn_id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Cleanup draft files for a specific turn.
    ///
    /// Returns paths of cleaned files.
    pub fn cleanup_turn_drafts(&mut self, turn_id: &str) -> Vec<PathBuf> {
        let mut cleaned = Vec::new();

        if let Some(ops) = self.turn_files.remove(turn_id) {
            for op in ops {
                if op.intent == FileIntent::Draft {
                    let _ = fs::remove_file(&op.path);
                    cleaned.push(op.path);
                }
            }
        }

        cleaned
    }

    /// Rollback all file operations for a specific turn.
    ///
    /// Returns error messages for failed operations.
    pub fn rollback_turn(&mut self, turn_id: &str) -> Vec<String> {
        let mut errors = Vec::new();

        if let Some(ops) = self.turn_files.remove(turn_id) {
            // Process in reverse order (undo last operations first)
            for op in ops.into_iter().rev() {
                match op.kind {
                    FileOpKind::Create => {
                        // Delete created file
                        if let Err(e) = fs::remove_file(&op.path) {
                            errors.push(format!("Failed to delete {:?}: {}", op.path, e));
                        }
                    }
                    FileOpKind::Edit => {
                        // Restore original content
                        if let Some(original) = op.original_content {
                            if let Err(e) = fs::write(&op.path, original) {
                                errors.push(format!("Failed to restore {:?}: {}", op.path, e));
                            }
                        }
                    }
                }
            }
        }

        errors
    }

    /// Cleanup with specified strategy.
    pub fn cleanup_with_strategy(
        &mut self,
        turn_id: &str,
        strategy: CleanupStrategy,
    ) -> CleanupResult {
        match strategy {
            CleanupStrategy::DraftsOnly => {
                let cleaned = self.cleanup_turn_drafts(turn_id);
                CleanupResult::DraftsCleaned(cleaned)
            }
            CleanupStrategy::FullRollback => {
                let errors = self.rollback_turn(turn_id);
                if errors.is_empty() {
                    CleanupResult::FullRollback
                } else {
                    CleanupResult::RollbackErrors(errors)
                }
            }
            CleanupStrategy::None => CleanupResult::NoAction,
        }
    }

    /// Clear all tracking data (call when session ends).
    pub fn clear(&mut self) {
        self.turn_files.clear();
        self.current_turn_id = None;
    }

    /// Get workspace root.
    pub fn workspace_root(&self) -> &PathBuf {
        &self.workspace_root
    }

    /// Get total file count for current turn.
    pub fn current_turn_file_count(&self) -> usize {
        self.get_current_turn_ops().len()
    }

    /// Get draft file count for current turn.
    pub fn current_turn_draft_count(&self) -> usize {
        self.get_current_turn_ops()
            .iter()
            .filter(|op| op.intent == FileIntent::Draft)
            .count()
    }
}

impl Default for TurnFileTracker {
    fn default() -> Self {
        Self::new(PathBuf::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_test_tracker() -> TurnFileTracker {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir_name = format!("scode-tracker-test-{}-{}", std::process::id(), unique_id);
        let tmp_dir = std::env::temp_dir().join(dir_name);

        // Ensure clean state
        if tmp_dir.exists() {
            fs::remove_dir_all(&tmp_dir).expect("cleanup existing");
        }
        fs::create_dir_all(&tmp_dir).expect("create temp dir");
        TurnFileTracker::new(tmp_dir)
    }

    fn cleanup_test_tracker(tracker: &TurnFileTracker) {
        if tracker.workspace_root.exists() {
            fs::remove_dir_all(&tracker.workspace_root).expect("cleanup");
        }
    }

    #[test]
    fn test_start_and_end_turn() {
        let mut tracker = create_test_tracker();

        tracker.start_turn("turn-1".to_string());
        assert_eq!(tracker.current_turn(), Some("turn-1"));

        tracker.end_turn();
        assert_eq!(tracker.current_turn(), None);

        cleanup_test_tracker(&tracker);
    }

    #[test]
    fn test_record_file_op() {
        let mut tracker = create_test_tracker();

        tracker.start_turn("turn-1".to_string());

        let op = FileOp {
            path: tracker.workspace_root.join("test.py"),
            kind: FileOpKind::Create,
            intent: FileIntent::Final,
            original_content: None,
            requested_path: tracker.workspace_root.join("test.py"),
        };

        tracker.record(op);

        assert_eq!(tracker.current_turn_file_count(), 1);

        cleanup_test_tracker(&tracker);
    }

    #[test]
    fn test_cleanup_draft_files() {
        let mut tracker = create_test_tracker();

        // Create drafts directory
        let drafts_dir = tracker.workspace_root.join(".drafts");
        fs::create_dir_all(&drafts_dir).expect("create drafts dir");

        // Create a draft file
        let draft_file = drafts_dir.join("temp.py");
        fs::write(&draft_file, "test content").expect("write draft file");

        tracker.start_turn("turn-1".to_string());

        tracker.record(FileOp {
            path: draft_file.clone(),
            kind: FileOpKind::Create,
            intent: FileIntent::Draft,
            original_content: None,
            requested_path: tracker.workspace_root.join("temp.py"),
        });

        tracker.record(FileOp {
            path: tracker.workspace_root.join("final.md"),
            kind: FileOpKind::Create,
            intent: FileIntent::Final,
            original_content: None,
            requested_path: tracker.workspace_root.join("final.md"),
        });

        let cleaned = tracker.cleanup_turn_drafts("turn-1");

        assert_eq!(cleaned.len(), 1);
        assert!(!draft_file.exists()); // Draft file should be removed

        cleanup_test_tracker(&tracker);
    }

    #[test]
    fn test_cleanup_strategy() {
        let mut tracker = create_test_tracker();

        tracker.start_turn("turn-1".to_string());

        tracker.record(FileOp {
            path: tracker.workspace_root.join(".drafts/temp.py"),
            kind: FileOpKind::Create,
            intent: FileIntent::Draft,
            original_content: None,
            requested_path: tracker.workspace_root.join("temp.py"),
        });

        let result = tracker.cleanup_with_strategy("turn-1", CleanupStrategy::DraftsOnly);
        assert!(matches!(result, CleanupResult::DraftsCleaned(_)));

        cleanup_test_tracker(&tracker);
    }
}