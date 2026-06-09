//! `/undo` — revert the most recent `edit_file` / `write_file` tool result
//! recorded in the live session.
//!
//! Limitations baked in (see `rust/SPIKE-179-s1-s2-s3.md` §S3):
//!
//! - Walks `Session.messages` only — does not reach into rotated logs or
//!   anything `/compact` has already summarized away.
//! - Tracks already-undone tool-use ids in-memory so repeated `/undo` calls
//!   step further back in history instead of re-undoing the same edit.
//! - No automatic conflict check against the on-disk file: if the user
//!   hand-edited the file between the tool call and the undo, we still
//!   restore the recorded pre-image. Recovery is via git.
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use runtime::{ContentBlock, ConversationMessage};
use serde_json::Value;

use super::format::split_hook_feedback;

/// Outcome of inspecting the live session for an undoable file mutation.
#[derive(Debug)]
pub(crate) struct UndoableEdit {
    pub(crate) tool_use_id: String,
    pub(crate) tool_name: String,
    pub(crate) file_path: String,
    /// `Some(content)` when the tool replaced an existing file; `None` when
    /// `write_file` created a file that did not exist before. The latter
    /// case undoes by deleting the file.
    pub(crate) original_file: Option<String>,
}

/// Scan messages newest-first and return the first `edit_file` / `write_file`
/// tool result whose `tool_use_id` is not in `already_undone`. Error results
/// are skipped — only successful edits left a pre-image worth restoring.
pub(crate) fn find_last_undoable_edit(
    messages: &[ConversationMessage],
    already_undone: &HashSet<String>,
) -> Option<UndoableEdit> {
    for message in messages.iter().rev() {
        for block in message.blocks.iter().rev() {
            let ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } = block
            else {
                continue;
            };
            if *is_error {
                continue;
            }
            if !matches!(tool_name.as_str(), "edit_file" | "write_file") {
                continue;
            }
            if already_undone.contains(tool_use_id) {
                continue;
            }
            if let Some(edit) =
                parse_undoable_edit(tool_use_id.as_str(), tool_name.as_str(), output)
            {
                return Some(edit);
            }
        }
    }
    None
}

fn parse_undoable_edit(tool_use_id: &str, tool_name: &str, output: &str) -> Option<UndoableEdit> {
    let (payload, _) = split_hook_feedback(output);
    let value: Value = serde_json::from_str(payload).ok()?;
    let file_path = value
        .get("filePath")
        .or_else(|| value.get("file_path"))
        .and_then(Value::as_str)?
        .to_string();
    let original_file = value
        .get("originalFile")
        .or_else(|| value.get("original_file"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // write_file with kind=="create" emits originalFile=null. edit_file
    // always has an originalFile (it cannot operate on a missing file).
    Some(UndoableEdit {
        tool_use_id: tool_use_id.to_string(),
        tool_name: tool_name.to_string(),
        file_path,
        original_file,
    })
}

/// Apply an undo to disk. Returns a one-line human-readable summary of what
/// changed, suitable for printing back to the user.
pub(crate) fn apply_undo(edit: &UndoableEdit) -> std::io::Result<String> {
    let path = Path::new(&edit.file_path);
    match edit.original_file.as_deref() {
        Some(original) => {
            fs::write(path, original)?;
            Ok(format!(
                "Restored {} ({} bytes) — undone {}",
                edit.file_path,
                original.len(),
                edit.tool_name,
            ))
        }
        None => {
            if path.exists() {
                fs::remove_file(path)?;
                Ok(format!(
                    "Deleted {} — undone {} (file was newly created)",
                    edit.file_path, edit.tool_name,
                ))
            } else {
                Ok(format!(
                    "Already absent: {} — undone {} (file was newly created)",
                    edit.file_path, edit.tool_name,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::{ContentBlock, ConversationMessage, MessageRole};

    fn tool_result(id: &str, name: &str, output: String, is_error: bool) -> ConversationMessage {
        ConversationMessage {
            role: MessageRole::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                tool_name: name.to_string(),
                output,
                is_error,
            }],
            usage: None,
            model: None,
        }
    }

    fn edit_output(file_path: &str, original: &str) -> String {
        serde_json::json!({
            "filePath": file_path,
            "oldString": "x",
            "newString": "y",
            "originalFile": original,
            "userModified": false,
            "replaceAll": false,
        })
        .to_string()
    }

    #[test]
    fn finds_most_recent_edit_in_reverse_order() {
        let messages = vec![
            tool_result("id1", "edit_file", edit_output("/tmp/a.txt", "v1"), false),
            tool_result("id2", "edit_file", edit_output("/tmp/b.txt", "v2"), false),
        ];
        let undone = HashSet::new();
        let edit = find_last_undoable_edit(&messages, &undone).unwrap();
        assert_eq!(edit.tool_use_id, "id2");
        assert_eq!(edit.file_path, "/tmp/b.txt");
        assert_eq!(edit.original_file.as_deref(), Some("v2"));
    }

    #[test]
    fn skips_already_undone_ids() {
        let messages = vec![
            tool_result("id1", "edit_file", edit_output("/tmp/a.txt", "v1"), false),
            tool_result("id2", "edit_file", edit_output("/tmp/b.txt", "v2"), false),
        ];
        let mut undone = HashSet::new();
        undone.insert("id2".to_string());
        let edit = find_last_undoable_edit(&messages, &undone).unwrap();
        assert_eq!(edit.tool_use_id, "id1");
    }

    #[test]
    fn skips_errored_tool_results() {
        let messages = vec![
            tool_result("id1", "edit_file", edit_output("/tmp/a.txt", "v1"), false),
            tool_result(
                "id2",
                "edit_file",
                "{\"error\":\"denied\"}".to_string(),
                true,
            ),
        ];
        let edit = find_last_undoable_edit(&messages, &HashSet::new()).unwrap();
        assert_eq!(edit.tool_use_id, "id1");
    }

    #[test]
    fn ignores_unrelated_tool_results() {
        let messages = vec![
            tool_result("id1", "bash", "stdout".to_string(), false),
            tool_result("id2", "read_file", "{}".to_string(), false),
        ];
        assert!(find_last_undoable_edit(&messages, &HashSet::new()).is_none());
    }

    #[test]
    fn parses_through_hook_feedback_suffix() {
        let polluted = format!(
            "{}\n\nHook feedback:\nlint clean",
            edit_output("/tmp/c.txt", "old")
        );
        let messages = vec![tool_result("id1", "edit_file", polluted, false)];
        let edit = find_last_undoable_edit(&messages, &HashSet::new()).unwrap();
        assert_eq!(edit.file_path, "/tmp/c.txt");
        assert_eq!(edit.original_file.as_deref(), Some("old"));
    }

    #[test]
    fn write_file_create_carries_no_original_file() {
        let create = serde_json::json!({
            "type": "create",
            "filePath": "/tmp/new.txt",
            "content": "hello",
            "originalFile": Value::Null,
        })
        .to_string();
        let messages = vec![tool_result("id1", "write_file", create, false)];
        let edit = find_last_undoable_edit(&messages, &HashSet::new()).unwrap();
        assert!(edit.original_file.is_none());
    }

    #[test]
    fn apply_undo_restores_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        fs::write(&path, "modified").unwrap();
        let edit = UndoableEdit {
            tool_use_id: "id1".into(),
            tool_name: "edit_file".into(),
            file_path: path.to_string_lossy().into_owned(),
            original_file: Some("original".to_string()),
        };
        let msg = apply_undo(&edit).unwrap();
        assert!(msg.contains("Restored"), "{msg}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn apply_undo_deletes_newly_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        fs::write(&path, "content from write_file").unwrap();
        let edit = UndoableEdit {
            tool_use_id: "id1".into(),
            tool_name: "write_file".into(),
            file_path: path.to_string_lossy().into_owned(),
            original_file: None,
        };
        let msg = apply_undo(&edit).unwrap();
        assert!(msg.contains("Deleted"), "{msg}");
        assert!(!path.exists());
    }
}
