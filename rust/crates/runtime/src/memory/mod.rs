//! File-based persistent memory.
//!
//! Layout under each global/project configuration root's `memory/` directory
//! (or the isolated `SUDOCODE_MEMORY_DIR` override):
//!
//! - `MEMORY.md` — flat markdown index of pointers to entries.
//! - `<slug>.md` — one file per remembered fact, with YAML-ish frontmatter
//!   (`name`, `description`, `metadata.type`) plus a body.
//!
//! The runtime reads memory at prompt-build time and appends a rendered
//! section to the [`SystemPromptBuilder`]. The model maintains the files through
//! its existing file tools, using the source paths and scope instructions.

pub mod entry;
pub mod index;
pub mod loader;
pub mod provider;

/// One process-wide lock for every test in this module tree that mutates
/// `HOME` or `SUDOCODE_MEMORY_DIR`.
///
/// `loader`, `provider` and this module's tests all resolve paths from the
/// same process globals, so they have to serialize against the *same* mutex.
/// Two module-local mutexes serialize nothing, which shows up as directories
/// resolved against another test's `HOME`.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::path::{Path, PathBuf};

pub use entry::{MemoryEntry, MemoryParseError, MemoryType};
pub use index::{IndexPointer, ParsedIndex};
pub use loader::{
    agent_memory_dir_for, agent_memory_dir_under, default_memory_dir, default_memory_dir_for,
    MEMORY_DIR_ENV, MEMORY_INDEX_FILE,
};
pub use provider::{
    DisabledMemoryProvider, FileMemoryProvider, MemoryContext, MemoryMode, MemoryProvider,
};

use crate::prompt::SystemPromptBuilder;

/// Cap individual entry body at 2000 chars when rendering.
pub const ENTRY_BODY_CHAR_CAP: usize = 2_000;
/// Cap total rendered output at 16000 chars; entries past the limit are dropped.
pub const RENDERED_CHAR_CAP: usize = 16_000;

/// Which edition of the auto-memory instructions to render.
///
/// The main conversation loop (and built-in sub-agents) get [`Compact`] —
/// a ~2k-char digest of the memory methodology that keeps the operational
/// core: when to save, the four types, the frontmatter shape, the
/// `MEMORY.md` index format, dedupe-before-write, and the
/// verify-before-trusting staleness rule. The full ~12.7k teaching
/// edition ([`Full`]) is reserved for custom sub-agents that declare a
/// `memory:` scope in their frontmatter — they are the ones expected to
/// curate memory deliberately, so they carry the complete methodology.
///
/// [`Compact`]: MemoryPromptVariant::Compact
/// [`Full`]: MemoryPromptVariant::Full
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryPromptVariant {
    #[default]
    Compact,
    Full,
}

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
    pub fn load(memory_dir: &Path, fs: &dyn crate::fs_backend::FsBackend) -> std::io::Result<Self> {
        let index = loader::load_index(memory_dir, fs)?;
        let entries = loader::load_entries(memory_dir, fs)?;
        Ok(Self {
            directory: memory_dir.to_path_buf(),
            index,
            entries,
        })
    }

    /// Load memory from [`default_memory_dir`], honoring `SUDOCODE_MEMORY_DIR`.
    pub fn load_default(fs: &dyn crate::fs_backend::FsBackend) -> std::io::Result<Self> {
        Self::load(&default_memory_dir(), fs)
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

    /// Render the memory store as a single prompt section using the
    /// [`Compact`](MemoryPromptVariant::Compact) instructions. The caller is
    /// expected to pass this through [`SystemPromptBuilder::append_section`].
    ///
    /// `memory_dir` is the resolved path to the memory directory, templated
    /// into the instructions so the model knows where to write.
    #[must_use]
    pub fn render_for_prompt(&self, memory_dir: &Path) -> String {
        self.render_for_prompt_with(memory_dir, MemoryPromptVariant::Compact)
    }

    /// Variant-aware sibling of [`MemoryIndex::render_for_prompt`]: the
    /// instruction preamble is chosen by `variant`, while the `MEMORY.md`
    /// index passthrough and entry rendering are identical for both.
    #[must_use]
    pub fn render_for_prompt_with(
        &self,
        memory_dir: &Path,
        variant: MemoryPromptVariant,
    ) -> String {
        self.render_scoped_for_prompt_with(memory_dir, variant, &[], None)
    }

    /// Render all scopes under one budget. Earlier scopes own duplicate filenames.
    pub(super) fn render_scoped_for_prompt_with(
        &self,
        memory_dir: &Path,
        variant: MemoryPromptVariant,
        fallbacks: &[Self],
        global_dir: Option<&Path>,
    ) -> String {
        use std::fmt::Write as _;

        let mut out = match variant {
            MemoryPromptVariant::Compact => build_compact_memory_instructions(memory_dir),
            MemoryPromptVariant::Full => build_full_memory_instructions(memory_dir),
        };
        out.push_str("\n\n");
        if let Some(global_dir) = global_dir {
            let _ = writeln!(out,
                "Project memory: `{}`. Global memory: `{}` (create it when needed). Save project-specific facts to project memory; save cross-project preferences to global memory. Same-filename precedence: project, legacy project, global. Update or forget a fact at its source path, including any shadowed copy when forgetting it. Legacy directories are compatibility sources; save new project facts in project memory.\n",
                memory_dir.display(), global_dir.display());
        }
        out = truncate_bytes(&out, RENDERED_CHAR_CAP - 160);
        let stores = || std::iter::once(self).chain(fallbacks.iter());
        for store in stores() {
            if let Some(index) = store
                .index
                .as_ref()
                .filter(|index| !index.raw.trim().is_empty())
            {
                let heading = format!("### Memory index: {}\n", store.directory.display());
                let remaining = RENDERED_CHAR_CAP.saturating_sub(out.len() + heading.len() + 160);
                if remaining > 0 {
                    out.push_str(&heading);
                    out.push_str(&truncate_bytes(index.raw.trim_end(), remaining));
                    out.push_str("\n\n");
                }
            }
        }
        out.push_str("## Loaded memory files\n");
        let mut seen = std::collections::BTreeSet::new();
        let mut dropped = 0;
        let mut loaded = 0;
        for entry in stores().flat_map(|store| &store.entries) {
            if !seen.insert(entry.path.file_name()) {
                continue;
            }
            let block = render_entry_block(entry);
            // Reserve room for the final budget notice.
            if out.len() + block.len() + 100 > RENDERED_CHAR_CAP {
                dropped += 1;
                continue;
            }
            out.push('\n');
            out.push_str(&block);
            loaded += 1;
        }
        if loaded == 0 && dropped == 0 {
            out.push_str("\n(no memory entries loaded)\n");
        }
        if dropped > 0 {
            let plural = if dropped == 1 { "y" } else { "ies" };
            let _ = write!(out,
                "\n[memory] {dropped} additional entr{plural} dropped to fit the 16000-char budget.\n");
        }

        out
    }
}

/// Append whatever `provider` contributes for `ctx`.
///
/// A provider that reports itself unavailable, or that contributes nothing,
/// leaves the builder untouched rather than emitting an empty memory
/// heading. This is the single seam every memory backend goes through.
#[inline]
#[must_use]
pub fn append_from_provider(
    builder: SystemPromptBuilder,
    provider: &dyn MemoryProvider,
    ctx: &MemoryContext,
) -> SystemPromptBuilder {
    if !provider.is_available() {
        return builder;
    }
    match provider.system_prompt_block(ctx) {
        Some(block) => builder.append_section(block),
        None => builder,
    }
}

/// Build the compact auto-memory instructions (~2k chars) carried by the
/// main conversation loop on every request. This is a digest of
/// [`build_full_memory_instructions`] that keeps the operational core —
/// when to save, the four types, the frontmatter shape, the `MEMORY.md`
/// index format, dedupe-before-write, and the staleness-verification rule —
/// while dropping the teaching material (worked examples, expanded
/// rationale, persistence-mechanism comparisons). The full edition stays
/// available for memory-scoped custom sub-agents.
///
/// The section header (`# auto memory`) is intentionally identical to the
/// full edition so downstream consumers and tests key on one name.
fn build_compact_memory_instructions(memory_dir: &Path) -> String {
    let dir_display = memory_dir.display();
    format!(
        r"# auto memory

You have a persistent memory directory at `{dir_display}` (it exists; write into it directly). One `.md` file per fact, with this frontmatter — all three fields required, `type` exactly one of `user`, `feedback`, `project`, `reference`:

```markdown
---
name: {{{{short name}}}}
description: {{{{one-line summary, specific enough to judge relevance later}}}}
type: {{{{user | feedback | project | reference}}}}
---
{{{{the fact; for feedback/project add **Why:** and **How to apply:** lines}}}}
```

A file that fails to parse is skipped silently. Bodies render up to 2,000 chars; the whole section up to 16,000.

`user` = who the user is and how they work; `feedback` = corrections and confirmed approaches, with the reason; `project` = goals, constraints, deadlines not derivable from the code (use absolute dates); `reference` = pointers to external systems.

After writing a file, add one line to `MEMORY.md`: `- [Title](file.md) — one-line hook`. That file is only an index.

Save immediately when asked to remember. When asked to forget, delete that entry's file and its `MEMORY.md` line in the same turn — the request is the confirmation. Otherwise save anything durable you learn. Update an existing memory instead of duplicating; remove ones that turn out wrong. Don't save what the repo already records (code structure, git history, fixes, AGENTS.md) — if asked to, save what was non-obvious instead. Memories are past observations: verify a file, function, or flag a memory names still exists before relying on it, and trust the current state over the memory."
    )
}

/// Build the full auto-memory instructions section, matching CC's
/// `buildMemoryLines()` from `memdir.ts`. The memory directory path
/// is templated in so the model knows where to write.
///
/// This 12.7k-char teaching edition is no longer carried by the main
/// conversation loop (which gets [`build_compact_memory_instructions`]);
/// it is routed to custom sub-agents that declare a `memory:` scope in
/// their frontmatter. Keep it in sync with the compact digest when the
/// operational rules change.
fn build_full_memory_instructions(memory_dir: &Path) -> String {
    let dir_display = memory_dir.display();
    format!(include_str!("instructions.md"), dir_display = dir_display)
}

/// Bound index bytes as well as entries, preserving UTF-8 boundaries.
fn truncate_bytes(text: &str, cap: usize) -> String {
    const SUFFIX: &str = " [truncated]";
    if text.len() <= cap {
        return text.to_string();
    }
    if cap < SUFFIX.len() {
        return SUFFIX[..cap].to_string();
    }
    let mut end = cap - SUFFIX.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &text[..end])
}

fn render_entry_block(entry: &MemoryEntry) -> String {
    let body = truncate_body(&entry.body, ENTRY_BODY_CHAR_CAP);
    format!(
        "- name: {name}  type: {ty}  description: {desc}\n  source: {path}\n  body: {body}\n",
        name = entry.name,
        path = entry.path.display(),
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
    use crate::fs_backend::StdFsBackend;
    use crate::prompt::SystemPromptBuilder;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn empty_directory_is_empty() {
        let dir = temp_dir("empty");
        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("load");
        assert!(idx.is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_directory_is_empty() {
        let dir = std::env::temp_dir().join("runtime-mem-mod-missing-xyz");
        fs::remove_dir_all(&dir).ok();
        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("missing dir is empty");
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

        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("load");
        let rendered = idx.render_for_prompt(&dir);
        assert!(rendered.starts_with("# auto memory"));
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
        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("load");
        let rendered = idx.render_for_prompt(&dir);
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
        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("load");
        let rendered = idx.render_for_prompt(&dir);
        assert!(rendered.len() <= RENDERED_CHAR_CAP);
        assert!(rendered.contains("dropped"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn append_from_provider_injects_instructions_even_when_empty() {
        let dir = temp_dir("empty-instructions");
        let ctx = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Compact);
        let appended = append_from_provider(
            SystemPromptBuilder::new().with_os("linux", "test"),
            &FileMemoryProvider::new(&StdFsBackend),
            &ctx,
        )
        .render();
        assert!(appended.contains("# auto memory"));
        assert!(appended.contains(&dir.display().to_string()));
        assert!(appended.contains("(no memory entries loaded)"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn append_from_provider_injects_section() {
        let dir = temp_dir("inject");
        fs::write(
            dir.join("MEMORY.md"),
            "# Key Learnings\n\n- [Role](role.md) — who the user is\n",
        )
        .unwrap();
        write_entry(&dir, "role", "user", "Senior Rust engineer.");
        write_entry(&dir, "habit", "feedback", "Prefer terse responses.");

        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("load");
        assert!(!idx.is_empty());

        let ctx = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Compact);
        let prompt = append_from_provider(
            SystemPromptBuilder::new().with_os("linux", "test"),
            &FileMemoryProvider::new(&StdFsBackend),
            &ctx,
        )
        .render();

        assert!(prompt.contains("# auto memory"));
        assert!(prompt.contains("role"));
        assert!(prompt.contains("habit"));
        assert!(prompt.contains("Key Learnings"));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn compact_instructions_stay_small_and_keep_operational_core() {
        let dir = PathBuf::from("/tmp/mem");
        let compact = build_compact_memory_instructions(&dir);
        // The whole point of the compact edition: an order-of-magnitude
        // smaller always-present footprint than the ~12.7k full edition.
        assert!(
            compact.chars().count() < 3_000,
            "compact edition should stay well under 3k chars, got {}",
            compact.chars().count()
        );
        // Operational core that must survive compression.
        assert!(compact.starts_with("# auto memory"));
        assert!(compact.contains("/tmp/mem"), "memory dir templated in");
        assert!(compact.contains("name:"), "frontmatter shape");
        assert!(compact.contains("type:"), "frontmatter type field");
        for ty in ["user", "feedback", "project", "reference"] {
            assert!(compact.contains(ty), "missing memory type {ty}");
        }
        assert!(compact.contains("MEMORY.md"), "index instructions");
        assert!(
            compact.contains("- [Title](file.md)"),
            "one-line index format"
        );
        assert!(
            compact.contains("Update an existing memory instead of duplicating"),
            "dedupe-before-write rule"
        );
        assert!(
            compact.contains("Memories are past observations"),
            "staleness-verification rule"
        );
        // Parser-derived warnings and the real budget constants.
        assert!(
            compact.contains("skipped silently"),
            "must warn that malformed files are silently dropped (loader.rs)"
        );
        assert!(
            compact.contains("2,000") && compact.contains("16,000"),
            "must cite the real ENTRY_BODY_CHAR_CAP / RENDERED_CHAR_CAP values"
        );
        assert!(
            !compact.contains("after 200"),
            "the 200-line index truncation claim has no code behind it"
        );
        // Teaching material must NOT survive compression.
        assert!(
            !compact.contains("<types>"),
            "worked-example XML belongs to the full edition only"
        );
    }

    #[test]
    fn taught_format_round_trips_through_the_parsers() {
        // The instructions teach the exact shapes below. entry.rs /
        // index.rs must accept them: the loader SILENTLY skips files
        // that fail to parse (loader.rs `load_entries`), so a prompt
        // that drifts from the parsers would lose memories without
        // any visible error.
        let raw = "---\n\
                   name: sample-fact\n\
                   description: one-line summary\n\
                   type: feedback\n\
                   ---\n\n\
                   The rule.\n\n\
                   **Why:** reason.\n\
                   **How to apply:** always.\n";
        let entry = MemoryEntry::parse(raw, Path::new("/tmp/sample-fact.md"))
            .expect("frontmatter shape taught by the prompt must parse");
        assert_eq!(entry.name, "sample-fact");
        assert_eq!(entry.description, "one-line summary");
        assert_eq!(entry.memory_type, MemoryType::Feedback);
        assert!(entry.body.starts_with("The rule."));

        // All four type values named by the prompt are accepted.
        for ty in ["user", "feedback", "project", "reference"] {
            let raw = format!("---\nname: n\ndescription: d\ntype: {ty}\n---\nbody\n");
            assert!(
                MemoryEntry::parse(&raw, Path::new("/t.md")).is_ok(),
                "type `{ty}` must parse"
            );
        }

        // The taught index line shape yields a pointer.
        let index = ParsedIndex::parse("- [Sample](sample-fact.md) — one-line hook\n");
        assert_eq!(index.pointers.len(), 1);
        assert_eq!(index.pointers[0].title, "Sample");
        assert_eq!(index.pointers[0].file, "sample-fact.md");
        assert_eq!(index.pointers[0].hook.as_deref(), Some("one-line hook"));
    }

    #[test]
    fn full_instructions_render_only_for_full_variant() {
        let dir = temp_dir("variant");
        let idx = MemoryIndex::load(&dir, &StdFsBackend).expect("load");
        let compact = idx.render_for_prompt_with(&dir, MemoryPromptVariant::Compact);
        let full = idx.render_for_prompt_with(&dir, MemoryPromptVariant::Full);
        assert!(compact.starts_with("# auto memory"));
        assert!(full.starts_with("# auto memory"));
        assert!(!compact.contains("<types>"));
        assert!(full.contains("<types>"));
        assert!(full.len() > compact.len() * 3);
        // Entry/index rendering is variant-independent.
        assert!(compact.contains("(no memory entries loaded)"));
        assert!(full.contains("(no memory entries loaded)"));
        // The default render path is the compact one.
        assert_eq!(idx.render_for_prompt(&dir), compact);
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
        let result = MemoryIndex::load_default(&StdFsBackend);
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
