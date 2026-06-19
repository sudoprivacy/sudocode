//! `MEMORY.md` is an index of memory entries — a flat markdown bullet list
//! that links to the per-entry files. It is NOT a place to store memory
//! content; entries live in their own files alongside it.
//!
//! Format:
//!
//! ```markdown
//! # Key Learnings
//!
//! ## Section heading
//! - [Title](filename.md) — one-line hook
//! - [Other title](other.md) — hook
//! ```
//!
//! For prompt injection, the raw text is passed through verbatim. This
//! module records light structural information for callers that want it.

use std::path::PathBuf;

/// One bullet item in `MEMORY.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPointer {
    pub title: String,
    pub file: String,
    pub hook: Option<String>,
}

/// Parsed form of `MEMORY.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedIndex {
    pub raw: String,
    pub pointers: Vec<IndexPointer>,
    pub path: Option<PathBuf>,
}

impl ParsedIndex {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.raw.trim().is_empty() && self.pointers.is_empty()
    }

    pub fn parse(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let pointers = collect_pointers(&raw);
        Self {
            raw,
            pointers,
            path: None,
        }
    }
}

fn collect_pointers(raw: &str) -> Vec<IndexPointer> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        else {
            continue;
        };
        let Some(pointer) = parse_pointer(rest) else {
            continue;
        };
        out.push(pointer);
    }
    out
}

fn parse_pointer(rest: &str) -> Option<IndexPointer> {
    let title_start = rest.find('[')?;
    let title_end_rel = rest[title_start + 1..].find(']')?;
    let title_end = title_start + 1 + title_end_rel;
    let after_title = &rest[title_end + 1..];
    let link_start_rel = after_title.find('(')?;
    let link_end_rel = after_title[link_start_rel + 1..].find(')')?;
    let link_start = title_end + 1 + link_start_rel;
    let link_end = link_start + 1 + link_end_rel;
    let title = rest[title_start + 1..title_end].trim().to_string();
    let file = rest[link_start + 1..link_end].trim().to_string();
    let tail = rest[link_end + 1..].trim();
    let hook = if tail.is_empty() {
        None
    } else {
        let trimmed = tail
            .trim_start_matches(|c: char| c == '—' || c == '-' || c == ':' || c.is_whitespace())
            .trim()
            .to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    };
    Some(IndexPointer { title, file, hook })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_index() {
        let raw = "# Key Learnings\n\n## Section\n- [Greeting](greeting.md) — how to greet\n- [Other](other.md) hook only\n";
        let parsed = ParsedIndex::parse(raw);
        assert_eq!(parsed.pointers.len(), 2);
        assert_eq!(parsed.pointers[0].title, "Greeting");
        assert_eq!(parsed.pointers[0].file, "greeting.md");
        assert_eq!(parsed.pointers[0].hook.as_deref(), Some("how to greet"));
        assert_eq!(parsed.pointers[1].title, "Other");
        assert_eq!(parsed.pointers[1].file, "other.md");
        assert_eq!(parsed.pointers[1].hook.as_deref(), Some("hook only"));
    }

    #[test]
    fn ignores_non_bullet_lines() {
        let raw = "# Index\n\nNot a bullet\n  - [Nested](nested.md) — ok\n";
        let parsed = ParsedIndex::parse(raw);
        assert_eq!(parsed.pointers.len(), 1);
        assert_eq!(parsed.pointers[0].file, "nested.md");
    }

    #[test]
    fn empty_index_is_empty() {
        let parsed = ParsedIndex::parse("");
        assert!(parsed.is_empty());
    }
}
