//! File-based persistent memory, modeled on Claude Code's
//! `~/.claude/projects/<slug>/memory/` system.
//!
//! Layout under the memory directory (default `~/.scode/memory/`,
//! overridable via the `SUDOCODE_MEMORY_DIR` env var):
//!
//! - `MEMORY.md` — flat markdown index of pointers to entries.
//! - `<slug>.md` — one file per remembered fact, with YAML-ish frontmatter
//!   (`name`, `description`, `metadata.type`) plus a body.
//!
//! The runtime reads memory at prompt-build time and appends a rendered
//! section to the [`SystemPromptBuilder`]. Writing is out of scope here —
//! the model is instructed to *propose* additions in its output, and a
//! follow-up PR will persist them.

pub mod entry;
pub mod index;
pub mod loader;

use std::path::{Path, PathBuf};

pub use entry::{MemoryEntry, MemoryParseError, MemoryType};
pub use index::{IndexPointer, ParsedIndex};
pub use loader::{default_memory_dir, MEMORY_DIR_ENV, MEMORY_INDEX_FILE};

use crate::prompt::SystemPromptBuilder;

/// Cap individual entry body at 2000 chars when rendering.
pub const ENTRY_BODY_CHAR_CAP: usize = 2_000;
/// Cap total rendered output at 16000 chars; entries past the limit are dropped.
pub const RENDERED_CHAR_CAP: usize = 16_000;

/// Loaded memory store. Combines an optional `MEMORY.md` index with the
/// parsed entry files.
#[derive(Debug, Clone, Default)]
pub struct MemoryIndex {
    pub directory: PathBuf,
    pub index: Option<ParsedIndex>,
    pub entries: Vec<MemoryEntry>,
}

impl MemoryIndex {
    /// Load memory from the given directory. Missing directory is treated
    /// as "no memory" rather than an error.
    pub fn load(memory_dir: &Path) -> std::io::Result<Self> {
        let index = loader::load_index(memory_dir)?;
        let entries = loader::load_entries(memory_dir)?;
        Ok(Self {
            directory: memory_dir.to_path_buf(),
            index,
            entries,
        })
    }

    /// Load memory from [`default_memory_dir`], honoring `SUDOCODE_MEMORY_DIR`.
    pub fn load_default() -> std::io::Result<Self> {
        Self::load(&default_memory_dir())
    }

    #[must_use]
    pub fn entries(&self) -> &[MemoryEntry] {
        &self.entries
    }

    #[must_use]
    pub fn index(&self) -> Option<&ParsedIndex> {
        self.index.as_ref()
    }

    /// `true` when there is nothing to inject into the prompt.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.index.as_ref().is_none_or(ParsedIndex::is_empty)
    }

    /// Render the memory store as a single prompt section. The caller is
    /// expected to pass this through [`SystemPromptBuilder::append_section`].
    #[must_use]
    pub fn render_for_prompt(&self) -> String {
        let mut out = String::new();
        out.push_str("# Persistent memory\n\n");
        out.push_str(PROMPT_PREAMBLE);
        out.push_str("\n\n");

        if let Some(index) = self.index.as_ref() {
            let trimmed = index.raw.trim_end();
            if !trimmed.is_empty() {
                out.push_str(trimmed);
                out.push_str("\n\n");
            }
        }

        out.push_str("## Loaded memory files\n");
        if self.entries.is_empty() {
            out.push_str("\n(no memory entries loaded)\n");
            return out;
        }

        let mut dropped = 0usize;
        let mut rendered_any = false;
        for entry in &self.entries {
            let block = render_entry_block(entry);
            // Reserve a little headroom for the trailing "dropped N" line.
            if out.len() + block.len() + 80 > RENDERED_CHAR_CAP {
                dropped += 1;
                continue;
            }
            out.push('\n');
            out.push_str(&block);
            rendered_any = true;
        }

        if !rendered_any && !self.entries.is_empty() {
            // We had entries but every one of them blew the budget. Note it.
            dropped = self.entries.len();
        }

        if dropped > 0 {
            use std::fmt::Write as _;
            let plural = if dropped == 1 { "y" } else { "ies" };
            let _ = write!(
                out,
                "\n[memory] {dropped} additional entr{plural} dropped to fit the 16000-char budget.\n"
            );
        }

        out
    }
}

/// Helper that appends the rendered memory section onto a
/// [`SystemPromptBuilder`]. No-op when there's nothing to render or when
/// loading fails.
#[must_use]
pub fn append_to_builder(
    builder: SystemPromptBuilder,
    memory_dir: Option<&Path>,
) -> SystemPromptBuilder {
    let owned;
    let dir = if let Some(d) = memory_dir {
        d
    } else {
        owned = default_memory_dir();
        owned.as_path()
    };
    match MemoryIndex::load(dir) {
        Ok(idx) if !idx.is_empty() => builder.append_section(idx.render_for_prompt()),
        _ => builder,
    }
}

/// Short static preamble explaining memory semantics to the model. Kept
/// under 600 chars per the task brief.
const PROMPT_PREAMBLE: &str =
    "The following has been remembered from prior sessions. Treat as background \
context, not as ground truth — verify against current code before acting.\n\n\
To add a memory, do NOT write to the filesystem yourself. Propose it in your \
reply as a markdown code block matching the entry frontmatter \
(`name`, `description`, `metadata.type` ∈ user|feedback|project|reference, \
then a body). Never edit `MEMORY.md` content directly — entries live in their \
own files; `MEMORY.md` only links to them.";

fn render_entry_block(entry: &MemoryEntry) -> String {
    let body = truncate_body(&entry.body, ENTRY_BODY_CHAR_CAP);
    format!(
        "- name: {name}  type: {ty}  description: {desc}\n  body: {body}\n",
        name = entry.name,
        ty = entry.memory_type,
        desc = entry.description,
        body = body.replace('\n', "\n        "),
    )
}

fn truncate_body(body: &str, cap: usize) -> String {
    if body.chars().count() <= cap {
        return body.to_string();
    }
    let mut out: String = body.chars().take(cap).collect();
    out.push_str(" [truncated]");
    out
}
