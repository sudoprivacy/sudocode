//! Individual memory entry: frontmatter + body parsing.
//!
//! A memory file is markdown with a small YAML-ish frontmatter block:
//!
//! ```markdown
//! ---
//! name: short-kebab-case-slug
//! description: one-line summary
//! metadata:
//!   type: feedback
//! ---
//!
//! Body text. Can use [[other-slug]] links.
//! ```
//!
//! The parser is intentionally minimal — it does not pull in `serde_yaml`,
//! since the frontmatter shape is fixed.

use std::fmt;
use std::path::{Path, PathBuf};

/// One of the four memory categories used by the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryType {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Feedback => "feedback",
            Self::Project => "project",
            Self::Reference => "reference",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "user" => Some(Self::User),
            "feedback" => Some(Self::Feedback),
            "project" => Some(Self::Project),
            "reference" => Some(Self::Reference),
            _ => None,
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A parsed memory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub name: String,
    pub description: String,
    pub memory_type: MemoryType,
    pub body: String,
    pub path: PathBuf,
}

/// Errors raised while parsing a memory file.
#[derive(Debug)]
pub enum MemoryParseError {
    MissingFrontmatter,
    UnterminatedFrontmatter,
    MissingField(&'static str),
    UnknownType(String),
}

impl fmt::Display for MemoryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFrontmatter => f.write_str("memory file is missing `---` frontmatter"),
            Self::UnterminatedFrontmatter => {
                f.write_str("memory file frontmatter has no closing `---`")
            }
            Self::MissingField(name) => write!(f, "memory frontmatter missing field `{name}`"),
            Self::UnknownType(value) => write!(f, "unknown memory type `{value}`"),
        }
    }
}

impl std::error::Error for MemoryParseError {}

impl MemoryEntry {
    /// Parse a memory file from disk. `path` is captured on the returned entry.
    pub fn from_file(path: &Path) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Self::parse(&raw, path)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }

    /// Parse a memory file from an in-memory string.
    pub fn parse(raw: &str, path: &Path) -> Result<Self, MemoryParseError> {
        let mut lines = raw.lines();
        let first = lines.next().ok_or(MemoryParseError::MissingFrontmatter)?;
        if first.trim() != "---" {
            return Err(MemoryParseError::MissingFrontmatter);
        }

        let mut frontmatter = Vec::new();
        let mut found_close = false;
        for line in lines.by_ref() {
            if line.trim() == "---" {
                found_close = true;
                break;
            }
            frontmatter.push(line);
        }
        if !found_close {
            return Err(MemoryParseError::UnterminatedFrontmatter);
        }

        let body: String = lines.collect::<Vec<_>>().join("\n");
        let body = body.trim_start_matches('\n').trim_end().to_string();

        let mut name: Option<String> = None;
        let mut description: Option<String> = None;
        let mut memory_type: Option<MemoryType> = None;
        let mut in_metadata = false;

        for line in &frontmatter {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            let content = line.trim_start();

            if indent == 0 {
                in_metadata = false;
                if let Some(value) = content.strip_prefix("name:") {
                    name = Some(unquote(value.trim()).to_string());
                } else if let Some(value) = content.strip_prefix("description:") {
                    description = Some(unquote(value.trim()).to_string());
                } else if let Some(value) = content.strip_prefix("type:") {
                    // Allow `type: ...` at the top level too, for friendliness.
                    let raw_type = unquote(value.trim());
                    memory_type = Some(
                        MemoryType::parse(raw_type)
                            .ok_or_else(|| MemoryParseError::UnknownType(raw_type.to_string()))?,
                    );
                } else if content.starts_with("metadata:") {
                    in_metadata = true;
                }
            } else if in_metadata {
                if let Some(value) = content.strip_prefix("type:") {
                    let raw_type = unquote(value.trim());
                    memory_type = Some(
                        MemoryType::parse(raw_type)
                            .ok_or_else(|| MemoryParseError::UnknownType(raw_type.to_string()))?,
                    );
                }
            }
        }

        Ok(Self {
            name: name.ok_or(MemoryParseError::MissingField("name"))?,
            description: description.ok_or(MemoryParseError::MissingField("description"))?,
            memory_type: memory_type.ok_or(MemoryParseError::MissingField("metadata.type"))?,
            body,
            path: path.to_path_buf(),
        })
    }
}

fn unquote(s: &str) -> &str {
    let trimmed = s.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(raw: &str) -> MemoryEntry {
        MemoryEntry::parse(raw, Path::new("/tmp/sample.md")).expect("parse")
    }

    #[test]
    fn parses_minimal_entry() {
        let raw = "---\nname: greeting\ndescription: how to greet\nmetadata:\n  type: feedback\n---\n\nBody text here.\n";
        let parsed = entry(raw);
        assert_eq!(parsed.name, "greeting");
        assert_eq!(parsed.description, "how to greet");
        assert_eq!(parsed.memory_type, MemoryType::Feedback);
        assert_eq!(parsed.body, "Body text here.");
        assert_eq!(parsed.path, Path::new("/tmp/sample.md"));
    }

    #[test]
    fn allows_quoted_values() {
        let raw =
            "---\nname: \"greet\"\ndescription: 'a hello'\nmetadata:\n  type: 'user'\n---\nbody\n";
        let parsed = entry(raw);
        assert_eq!(parsed.name, "greet");
        assert_eq!(parsed.description, "a hello");
        assert_eq!(parsed.memory_type, MemoryType::User);
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let err = MemoryEntry::parse("no frontmatter here", Path::new("/x")).unwrap_err();
        assert!(matches!(err, MemoryParseError::MissingFrontmatter));
    }

    #[test]
    fn rejects_unterminated_frontmatter() {
        let err =
            MemoryEntry::parse("---\nname: x\ndescription: y\n", Path::new("/x")).unwrap_err();
        assert!(matches!(err, MemoryParseError::UnterminatedFrontmatter));
    }

    #[test]
    fn rejects_missing_field() {
        let raw = "---\nname: only-name\nmetadata:\n  type: user\n---\nbody\n";
        let err = MemoryEntry::parse(raw, Path::new("/x")).unwrap_err();
        assert!(matches!(err, MemoryParseError::MissingField("description")));
    }

    #[test]
    fn rejects_unknown_type() {
        let raw = "---\nname: x\ndescription: y\nmetadata:\n  type: bogus\n---\nbody\n";
        let err = MemoryEntry::parse(raw, Path::new("/x")).unwrap_err();
        assert!(matches!(err, MemoryParseError::UnknownType(_)));
    }

    #[test]
    fn accepts_top_level_type_shorthand() {
        let raw = "---\nname: a\ndescription: b\ntype: project\n---\nbody\n";
        let parsed = entry(raw);
        assert_eq!(parsed.memory_type, MemoryType::Project);
    }
}
