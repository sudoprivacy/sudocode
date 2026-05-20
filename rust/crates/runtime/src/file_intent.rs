//! File intent detection for classifying files as Final or Draft.
//!
//! This module provides local (no LLM) file intent detection based on:
//! 1. File markers (@final/@draft) in content
//! 2. User request matching (files explicitly requested by user)
//! 3. File name patterns
//! 4. File extensions

use std::collections::HashSet;
use std::path::Path;

/// File intent classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIntent {
    /// Final product - user will directly use this file.
    /// Keep in workspace root.
    Final,

    /// Draft/intermediate file - auxiliary file during execution.
    /// Move to .drafts/ directory.
    Draft,
}

impl Default for FileIntent {
    fn default() -> Self {
        Self::Final // Conservative default
    }
}

/// File operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOpKind {
    /// New file created.
    Create,

    /// Existing file edited.
    Edit,
}

/// User request intent analysis result.
#[derive(Debug, Default)]
pub struct UserRequestIntent {
    /// File names explicitly requested by user.
    pub requested_files: HashSet<String>,

    /// File type keywords requested by user.
    pub requested_types: HashSet<String>,
}

impl UserRequestIntent {
    /// Analyze user request to extract file intent.
    pub fn analyze(user_request: &str) -> Self {
        let mut result = Self::default();

        let lower = user_request.to_lowercase();

        // 1. Extract explicitly mentioned file names
        // Patterns: "创建 xxx.py", "写一个 xxx.sh", "create xxx.ts"
        let file_patterns = [
            (r"创建?\s*([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", true),
            (r"写一个\s*([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", true),
            (r"生成\s*([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", true),
            (r"新建\s*([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", true),
            (r"create\s+([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", false),
            (r"write\s+([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", false),
            (r"add\s+([a-zA-Z0-9_-]+\.[a-zA-Z0-9]+)", false),
        ];

        for (pattern, is_chinese) in &file_patterns {
            let regex_pattern = if *is_chinese {
                pattern.to_string()
            } else {
                pattern.to_string()
            };

            if let Ok(re) = regex::Regex::new(&regex_pattern) {
                for cap in re.captures_iter(&lower) {
                    if let Some(file) = cap.get(1) {
                        result.requested_files.insert(file.as_str().to_string());
                    }
                }
            }
        }

        // 2. Extract requested file types
        let type_keywords = [
            ("python", vec!["python", "py"]),
            ("shell", vec!["shell", "sh", "bash"]),
            ("script", vec!["脚本", "script"]),
            ("utility", vec!["工具", "utility", "util", "helper"]),
            ("typescript", vec!["typescript", "ts"]),
            ("javascript", vec!["javascript", "js"]),
            ("rust", vec!["rust", "rs"]),
            ("go", vec!["go", "golang"]),
        ];

        for (type_name, keywords) in &type_keywords {
            for keyword in keywords {
                if lower.contains(keyword) {
                    result.requested_types.insert(type_name.to_string());
                    break;
                }
            }
        }

        result
    }

    /// Check if a file is requested by user.
    pub fn is_requested_file(&self, file_path: &str) -> bool {
        let file_name = Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_lowercase())
            .unwrap_or_default();

        // 1. Direct file name match
        if self.requested_files.contains(&file_name) {
            return true;
        }

        // 2. Extension match with requested types
        let ext = Path::new(file_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();

        let ext_to_type: std::collections::HashMap<&str, &str> = [
            ("py", "python"),
            ("sh", "shell"),
            ("bash", "shell"),
            ("zsh", "shell"),
            ("ts", "typescript"),
            ("tsx", "typescript"),
            ("js", "javascript"),
            ("jsx", "javascript"),
            ("rs", "rust"),
            ("go", "go"),
        ]
        .iter()
        .cloned()
        .collect();

        if let Some(type_name) = ext_to_type.get(ext.as_str()) {
            if self.requested_types.contains(*type_name) || self.requested_types.contains("script")
            {
                return true;
            }
        }

        false
    }
}

/// File intent marker in content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntentMarker {
    Final,
    Draft,
}

/// Detect intent marker from file content (first 10 lines).
fn detect_intent_marker(content: &str) -> Option<IntentMarker> {
    let lines: Vec<&str> = content.lines().take(10).collect();

    // Comment syntax map
    let comment_prefixes: &[&str] = &["#", "//", "--", ";", "<!--"];

    for line in &lines {
        let trimmed = line.trim();

        for prefix in comment_prefixes {
            if trimmed.starts_with(prefix) {
                let comment_content = if *prefix == "<!--" {
                    // HTML comment: <!-- @final -->
                    if trimmed.starts_with("<!--") && trimmed.ends_with("-->") {
                        trimmed[4..trimmed.len() - 3].trim()
                    } else {
                        continue;
                    }
                } else {
                    trimmed[prefix.len()..].trim()
                };

                // Check for markers
                let lower = comment_content.to_lowercase();

                if lower.contains("@final") {
                    return Some(IntentMarker::Final);
                }
                if lower.contains("@draft") {
                    return Some(IntentMarker::Draft);
                }
            }
        }
    }

    None
}

/// Draft file name patterns.
const DRAFT_PATTERNS: &[&str] = &[
    // Temporary file prefixes
    r"^temp[_-]",
    r"^tmp[_-]",
    r"^temporary[_-]",
    // Draft/work in progress
    r"^draft[_-]",
    r"^wip[_-]",
    r"^scratch[_-]",
    r"^proto[_-]",
    r"^poc[_-]",
    // Step files
    r"^step[_-]?\d+",
    r"^phase[_-]?\d+",
    // Suffix markers (before extension)
    r"[_-]draft\.",
    r"[_-]wip\.",
    r"[_-]temp\.",
    r"[_-]tmp\.",
    r"[_-]backup\.",
    r"[_-]bak\.",
    r"[_-]old\.",
    // End of name suffix markers
    r"[_-]draft$",
    r"[_-]wip$",
    r"[_-]temp$",
    r"[_-]tmp$",
    r"[_-]backup$",
    r"[_-]bak$",
    r"[_-]old$",
];

/// Final file name patterns (override draft patterns).
const FINAL_PATTERNS: &[&str] = &[
    // Suffix markers (before extension)
    r"[_-]final\.",
    r"[_-]result\.",
    r"[_-]output\.",
    r"[_-]completed\.",
    r"[_-]done\.",
    // End of name suffix markers
    r"[_-]final$",
    r"[_-]result$",
    r"[_-]output$",
    r"[_-]completed$",
    r"[_-]done$",
];

/// Draft file extensions.
const DRAFT_EXTENSIONS: &[&str] = &[".tmp", ".temp", ".bak", ".backup", ".log", ".cache"];

/// Final file extensions.
const FINAL_EXTENSIONS: &[&str] = &[
    // Documents
    ".md", ".txt", ".pdf", ".docx", ".pptx", // Data files
    ".json", ".yaml", ".yml", ".csv", ".xlsx",
    // Code files (user may explicitly request)
    ".py", ".sh", ".bash", ".zsh", ".ts", ".tsx", ".js", ".jsx", ".rs", ".go", ".java", ".kt", ".c",
    ".cpp", ".h", ".hpp", ".rb", ".php", ".lua", // Config files
    ".toml", ".ini", ".conf", ".cfg", // Web/images
    ".html", ".css", ".scss", ".png", ".jpg", ".svg",
];

/// Check if file name matches draft patterns.
fn matches_draft_pattern(file_path: &str) -> bool {
    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let lower = file_name.to_lowercase();

    for pattern in DRAFT_PATTERNS {
        if let Ok(re) = regex::Regex::new(pattern) {
            if re.is_match(&lower) {
                return true;
            }
        }
    }

    false
}

/// Check if file name matches final patterns.
fn matches_final_pattern(file_path: &str) -> bool {
    let file_name = Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    let lower = file_name.to_lowercase();

    for pattern in FINAL_PATTERNS {
        if let Ok(re) = regex::Regex::new(pattern) {
            if re.is_match(&lower) {
                return true;
            }
        }
    }

    false
}

/// Detect file intent from path, content, and optional user request.
///
/// Priority:
/// 1. File markers (@final/@draft) - highest
/// 2. User request matching - user explicitly requested = Final
/// 3. Final protection patterns - override draft rules
/// 4. Draft patterns
/// 5. Extension rules
/// 6. Default - Final (conservative)
pub fn detect_file_intent(
    file_path: &str,
    content: &str,
    user_intent: Option<&UserRequestIntent>,
) -> FileIntent {
    // 1. Check markers (highest priority)
    if let Some(marker) = detect_intent_marker(content) {
        return match marker {
            IntentMarker::Final => FileIntent::Final,
            IntentMarker::Draft => FileIntent::Draft,
        };
    }

    // 2. User request matching
    if let Some(intent) = user_intent {
        if intent.is_requested_file(file_path) {
            return FileIntent::Final;
        }
    }

    // 3. Final protection patterns
    if matches_final_pattern(file_path) {
        return FileIntent::Final;
    }

    // 4. Draft patterns
    if matches_draft_pattern(file_path) {
        return FileIntent::Draft;
    }

    // 5. Extension rules
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    if DRAFT_EXTENSIONS.contains(&ext.as_str()) {
        return FileIntent::Draft;
    }

    if FINAL_EXTENSIONS.contains(&ext.as_str()) {
        return FileIntent::Final;
    }

    // 6. Default Final (conservative)
    FileIntent::Final
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_intent_marker_final() {
        let content = "# @final\nThis is the final report.";
        assert_eq!(detect_intent_marker(content), Some(IntentMarker::Final));
    }

    #[test]
    fn test_detect_intent_marker_draft() {
        let content = "// @draft\nTemporary script.";
        assert_eq!(detect_intent_marker(content), Some(IntentMarker::Draft));
    }

    #[test]
    fn test_detect_intent_marker_none() {
        let content = "No marker here.";
        assert_eq!(detect_intent_marker(content), None);
    }

    #[test]
    fn test_matches_draft_pattern() {
        assert!(matches_draft_pattern("temp_script.py"));
        assert!(matches_draft_pattern("tmp_data.json"));
        assert!(matches_draft_pattern("report-draft.md"));
        assert!(matches_draft_pattern("step_1_output.txt"));
        assert!(!matches_draft_pattern("report.md"));
        assert!(!matches_draft_pattern("process_data.py"));
    }

    #[test]
    fn test_matches_final_pattern() {
        assert!(matches_final_pattern("report-final.md"));
        assert!(matches_final_pattern("data_result.json"));
        assert!(matches_final_pattern("output_completed.csv"));
        assert!(!matches_final_pattern("report-draft.md"));
    }

    #[test]
    fn test_user_request_intent_file_name() {
        let intent = UserRequestIntent::analyze("帮我创建 process_data.py 脚本");
        assert!(intent.requested_files.contains("process_data.py"));
    }

    #[test]
    fn test_user_request_intent_file_type() {
        let intent = UserRequestIntent::analyze("写一个 Python 脚本处理数据");
        assert!(intent.requested_types.contains("python"));
        assert!(intent.requested_types.contains("script"));
    }

    #[test]
    fn test_is_requested_file() {
        let intent = UserRequestIntent::analyze("创建 process_data.py");
        assert!(intent.is_requested_file("process_data.py"));
        assert!(intent.is_requested_file("/workspace/process_data.py"));
    }

    #[test]
    fn test_detect_file_intent_with_marker() {
        let content = "# @draft\nTemporary file.";
        assert_eq!(
            detect_file_intent("temp.py", content, None),
            FileIntent::Draft
        );
    }

    #[test]
    fn test_detect_file_intent_with_user_request() {
        let intent = UserRequestIntent::analyze("写一个 Python 脚本");
        assert_eq!(
            detect_file_intent("process.py", "", Some(&intent)),
            FileIntent::Final
        );
    }

    #[test]
    fn test_detect_file_intent_draft_pattern() {
        assert_eq!(
            detect_file_intent("temp_script.py", "", None),
            FileIntent::Draft
        );
    }

    #[test]
    fn test_detect_file_intent_final_extension() {
        assert_eq!(detect_file_intent("report.md", "", None), FileIntent::Final);
        assert_eq!(
            detect_file_intent("process.py", "", None),
            FileIntent::Final
        );
    }

    #[test]
    fn test_detect_file_intent_final_pattern_override() {
        // Final pattern should override draft pattern
        assert_eq!(
            detect_file_intent("temp-final.py", "", None),
            FileIntent::Final
        );
    }
}
