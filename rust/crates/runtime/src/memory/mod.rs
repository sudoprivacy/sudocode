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
pub use loader::{default_memory_dir, default_memory_dir_for, MEMORY_DIR_ENV, MEMORY_INDEX_FILE};

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
///
/// When `memory_dir` is `None`, the directory is derived from `cwd`
/// (project-scoped path under `~/.scode/projects/<slug>/memory/`).
/// When `cwd` is also `None`, falls back to `default_memory_dir()`.
#[must_use]
pub fn append_to_builder(
    builder: SystemPromptBuilder,
    memory_dir: Option<&Path>,
    cwd: Option<&Path>,
) -> SystemPromptBuilder {
    let owned;
    let dir = if let Some(d) = memory_dir {
        d
    } else if let Some(cwd) = cwd {
        owned = default_memory_dir_for(cwd);
        owned.as_path()
    } else {
        owned = default_memory_dir();
        owned.as_path()
    };
    loader::ensure_memory_dir_exists(dir);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::SystemPromptBuilder;
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("runtime-mem-mod-{prefix}-{nanos}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn write_entry(dir: &Path, slug: &str, ty: &str, body: &str) {
        let raw = format!(
            "---\nname: {slug}\ndescription: desc for {slug}\nmetadata:\n  type: {ty}\n---\n\n{body}\n"
        );
        fs::write(dir.join(format!("{slug}.md")), raw).unwrap();
    }

    #[test]
    fn preamble_stays_under_600_chars() {
        assert!(
            PROMPT_PREAMBLE.len() < 600,
            "preamble is {} chars",
            PROMPT_PREAMBLE.len()
        );
    }

    #[test]
    fn empty_directory_is_empty() {
        let dir = temp_dir("empty");
        let idx = MemoryIndex::load(&dir).expect("load");
        assert!(idx.is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_directory_is_empty() {
        let dir = std::env::temp_dir().join("runtime-mem-mod-missing-xyz");
        fs::remove_dir_all(&dir).ok();
        let idx = MemoryIndex::load(&dir).expect("missing dir is empty");
        assert!(idx.is_empty());
    }

    #[test]
    fn renders_index_and_entries() {
        let dir = temp_dir("renders");
        fs::write(
            dir.join("MEMORY.md"),
            "# Key Learnings\n\n## Habits\n- [Greet](greet.md) — say hi\n",
        )
        .unwrap();
        write_entry(&dir, "greet", "feedback", "Always greet warmly.");
        write_entry(&dir, "role", "user", "Senior Rust engineer.");

        let idx = MemoryIndex::load(&dir).expect("load");
        let rendered = idx.render_for_prompt();
        assert!(rendered.starts_with("# Persistent memory"));
        assert!(rendered.contains("Key Learnings"));
        assert!(rendered.contains("- name: greet"));
        assert!(rendered.contains("- name: role"));
        assert!(rendered.contains("type: feedback"));
        assert!(rendered.contains("Always greet warmly."));
        assert!(rendered.len() <= RENDERED_CHAR_CAP);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn truncates_oversize_body() {
        let dir = temp_dir("truncate");
        let big_body = "x".repeat(ENTRY_BODY_CHAR_CAP * 2);
        write_entry(&dir, "big", "project", &big_body);
        let idx = MemoryIndex::load(&dir).expect("load");
        let rendered = idx.render_for_prompt();
        assert!(rendered.contains("[truncated]"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn drops_entries_past_budget() {
        let dir = temp_dir("budget");
        // Each entry body is ~1900 chars; with 16000 cap and ~10 entries we
        // should exceed the budget and drop some.
        let body = "y".repeat(1_900);
        for i in 0..12 {
            write_entry(&dir, &format!("e{i:02}"), "project", &body);
        }
        let idx = MemoryIndex::load(&dir).expect("load");
        let rendered = idx.render_for_prompt();
        assert!(rendered.len() <= RENDERED_CHAR_CAP);
        assert!(rendered.contains("dropped"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn append_to_builder_skips_when_empty() {
        let dir = temp_dir("skip");
        let rendered_no_mem = SystemPromptBuilder::new().with_os("linux", "test").render();
        let appended = append_to_builder(
            SystemPromptBuilder::new().with_os("linux", "test"),
            Some(&dir),
            None,
        )
        .render();
        assert!(!appended.contains("# Persistent memory"));
        assert_eq!(rendered_no_mem, appended);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn append_to_builder_injects_section() {
        let dir = temp_dir("inject");
        fs::write(
            dir.join("MEMORY.md"),
            "# Key Learnings\n\n- [Role](role.md) — who the user is\n",
        )
        .unwrap();
        write_entry(&dir, "role", "user", "Senior Rust engineer.");
        write_entry(&dir, "habit", "feedback", "Prefer terse responses.");

        let idx = MemoryIndex::load(&dir).expect("load");
        assert!(!idx.is_empty());

        let prompt = append_to_builder(
            SystemPromptBuilder::new().with_os("linux", "test"),
            Some(&dir),
            None,
        )
        .render();

        assert!(prompt.contains("# Persistent memory"));
        assert!(prompt.contains("role"));
        assert!(prompt.contains("habit"));
        assert!(prompt.contains("Key Learnings"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn load_default_honors_env_var() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_dir("default-env");
        write_entry(&dir, "via-env", "reference", "Look here for X.");
        let prior = std::env::var_os(MEMORY_DIR_ENV);
        std::env::set_var(MEMORY_DIR_ENV, &dir);
        let result = MemoryIndex::load_default();
        if let Some(value) = prior {
            std::env::set_var(MEMORY_DIR_ENV, value);
        } else {
            std::env::remove_var(MEMORY_DIR_ENV);
        }
        let idx = result.expect("load default");
        assert_eq!(idx.entries().len(), 1);
        assert_eq!(idx.entries()[0].name, "via-env");
        fs::remove_dir_all(dir).ok();
    }
}
