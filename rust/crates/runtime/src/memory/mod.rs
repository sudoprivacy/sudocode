//! File-based persistent memory.
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
    agent_memory_dir_for, default_memory_dir, default_memory_dir_for, MEMORY_DIR_ENV,
    MEMORY_INDEX_FILE,
};
pub use provider::{FileMemoryProvider, MemoryContext, MemoryProvider};

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
        let mut out = String::new();
        match variant {
            MemoryPromptVariant::Compact => {
                out.push_str(&build_compact_memory_instructions(memory_dir));
            }
            MemoryPromptVariant::Full => {
                out.push_str(&build_full_memory_instructions(memory_dir));
            }
        }
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
        r#"# auto memory

You have a persistent, file-based memory system at `{dir_display}`. The directory already exists — write files into it directly with the Write tool (do not run mkdir or check for its existence). Use it to carry durable context across conversations: who the user is, how they want you to work, and the background behind the work they give you.

Each memory is one `.md` file holding one fact, using this frontmatter format:

```markdown
---
name: {{{{short name}}}}
description: {{{{one-line summary — used to decide relevance in future conversations, so be specific}}}}
type: {{{{user, feedback, project, reference}}}}
---

{{{{memory content — for feedback/project types: the rule/fact first, then **Why:** and **How to apply:** lines}}}}
```

The file must begin with the `---` line. All three fields are required (single line each) and `type` must be exactly one of the four lowercase values — a file that fails to parse is skipped silently, so keep the format exact. Bodies render up to 2,000 chars; the whole section caps at 16,000.

Types: `user` — the user's role, expertise, and preferences. `feedback` — guidance on how to approach work: corrections AND confirmed approaches, with the reason. `project` — ongoing work, goals, deadlines, and constraints not derivable from the code or git history (convert relative dates to absolute when saving). `reference` — pointers to information in external systems (dashboards, trackers, channels).

After writing a memory file, add a pointer line to `MEMORY.md` (exact uppercase name): `- [Title](file.md) — one-line hook`. `MEMORY.md` is an index, not a memory — one bullet line per entry, under ~150 characters, no frontmatter. Never write memory content directly into it.

When to save: immediately when the user explicitly asks you to remember something (and remove the entry when asked to forget). Otherwise, save when you learn something durable — a preference, a correction, a validated approach, project context, or an external resource. Before writing, check whether an existing memory already covers it and update that file instead of duplicating. Update or remove memories that turn out to be wrong or outdated.

Do NOT save what the current project state already records: code structure and conventions, git history, debugging fixes (the fix is in the code), anything documented in AGENTS.md, or ephemeral task details. This applies even when the user explicitly asks to save — ask what was surprising or non-obvious, and save that instead.

Memories are observations from the past, not guarantees about the present. Before answering from memory or recommending something a memory names (a file, function, or flag), verify it against the current state of the code or resource — "the memory says X exists" is not the same as "X exists now". If memory conflicts with what you observe, trust the current state and update or remove the stale entry. If the user says to ignore memory, proceed as if it were empty."#
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
    format!(
        r#"# auto memory

You have a persistent, file-based memory system at `{dir_display}`. This directory already exists — write to it directly with the Write tool (do not run mkdir or check for its existence).

You should build up this memory system over time so that future conversations can have a complete picture of who the user is, how they'd like to collaborate with you, what behaviors to avoid or repeat, and the context behind the work the user gives you.

If the user explicitly asks you to remember something, save it immediately as whichever type fits best. If they ask you to forget something, find and remove the relevant entry.

## Types of memory

There are several discrete types of memory that you can store in your memory system:

<types>
<type>
    <name>user</name>
    <description>Contain information about the user's role, goals, responsibilities, and knowledge. Great user memories help you tailor your future behavior to the user's preferences and perspective. Your goal in reading and writing these memories is to build up an understanding of who the user is and how you can be most helpful to them specifically. For example, you should collaborate with a senior software engineer differently than a student who is coding for the very first time. Keep in mind, that the aim here is to be helpful to the user. Avoid writing memories about the user that could be viewed as a negative judgement or that are not relevant to the work you're trying to accomplish together.</description>
    <when_to_save>When you learn any details about the user's role, preferences, responsibilities, or knowledge</when_to_save>
    <how_to_use>When your work should be informed by the user's profile or perspective. For example, if the user is asking you to explain a part of the code, you should answer that question in a way that is tailored to the specific details that they will find most valuable or that helps them build their mental model in relation to domain knowledge they already have.</how_to_use>
    <examples>
    user: I'm a data scientist investigating what logging we have in place
    assistant: [saves user memory: user is a data scientist, currently focused on observability/logging]

    user: I've been writing Go for ten years but this is my first time touching the React side of this repo
    assistant: [saves user memory: deep Go expertise, new to React and this project's frontend — frame frontend explanations in terms of backend analogues]
    </examples>
</type>
<type>
    <name>feedback</name>
    <description>Guidance the user has given you about how to approach work — both what to avoid and what to keep doing. These are a very important type of memory to read and write as they allow you to remain coherent and responsive to the way you should approach work in the project. Record from failure AND success: if you only save corrections, you will avoid past mistakes but drift away from approaches the user has already validated, and may grow overly cautious.</description>
    <when_to_save>Any time the user corrects your approach ("no not that", "don't", "stop doing X") OR confirms a non-obvious approach worked ("yes exactly", "perfect, keep doing that", accepting an unusual choice without pushback). Corrections are easy to notice; confirmations are quieter — watch for them. In both cases, save what is applicable to future conversations, especially if surprising or not obvious from the code. Include *why* so you can judge edge cases later.</when_to_save>
    <how_to_use>Let these memories guide your behavior so that the user does not need to offer the same guidance twice.</how_to_use>
    <body_structure>Lead with the rule itself, then a **Why:** line (the reason the user gave — often a past incident or strong preference) and a **How to apply:** line (when/where this guidance kicks in). Knowing *why* lets you judge edge cases instead of blindly following the rule.</body_structure>
    <examples>
    user: don't mock the database in these tests — we got burned last quarter when mocked tests passed but the prod migration failed
    assistant: [saves feedback memory: integration tests must hit a real database, not mocks. Reason: prior incident where mock/prod divergence masked a broken migration]

    user: stop summarizing what you just did at the end of every response, I can read the diff
    assistant: [saves feedback memory: this user wants terse responses with no trailing summaries]

    user: yeah the single bundled PR was the right call here, splitting this one would've just been churn
    assistant: [saves feedback memory: for refactors in this area, user prefers one bundled PR over many small ones. Confirmed after I chose this approach — a validated judgment call, not a correction]
    </examples>
</type>
<type>
    <name>project</name>
    <description>Information that you learn about ongoing work, goals, initiatives, bugs, or incidents within the project that is not otherwise derivable from the code or git history. Project memories help you understand the broader context and motivation behind the work the user is doing within this working directory.</description>
    <when_to_save>When you learn who is doing what, why, or by when. These states change relatively quickly so try to keep your understanding of this up to date. Always convert relative dates in user messages to absolute dates when saving (e.g., "Thursday" → "2026-03-05"), so the memory remains interpretable after time passes.</when_to_save>
    <how_to_use>Use these memories to more fully understand the details and nuance behind the user's request and make better informed suggestions.</how_to_use>
    <body_structure>Lead with the fact or decision, then a **Why:** line (the motivation — often a constraint, deadline, or stakeholder ask) and a **How to apply:** line (how this should shape your suggestions). Project memories decay fast, so the why helps future-you judge whether the memory is still load-bearing.</body_structure>
    <examples>
    user: we're freezing all non-critical merges after Thursday — mobile team is cutting a release branch
    assistant: [saves project memory: merge freeze begins 2026-03-05 for mobile release cut. Flag any non-critical PR work scheduled after that date]

    user: the reason we're ripping out the old auth middleware is that legal flagged it for storing session tokens in a way that doesn't meet the new compliance requirements
    assistant: [saves project memory: auth middleware rewrite is driven by legal/compliance requirements around session token storage, not tech-debt cleanup — scope decisions should favor compliance over ergonomics]
    </examples>
</type>
<type>
    <name>reference</name>
    <description>Stores pointers to where information can be found in external systems. These memories allow you to remember where to look to find up-to-date information outside of the project directory.</description>
    <when_to_save>When you learn about resources in external systems and their purpose. For example, that bugs are tracked in a specific project in Linear or that feedback can be found in a specific Slack channel.</when_to_save>
    <how_to_use>When the user references an external system or information that may be in an external system.</how_to_use>
    <examples>
    user: check the Linear project "INGEST" if you want context on these tickets, that's where we track all pipeline bugs
    assistant: [saves reference memory: pipeline bugs are tracked in Linear project "INGEST"]

    user: the Grafana board at grafana.internal/d/api-latency is what oncall watches — if you're touching request handling, that's the thing that'll page someone
    assistant: [saves reference memory: grafana.internal/d/api-latency is the oncall latency dashboard — check it when editing request-path code]
    </examples>
</type>
</types>

## What NOT to save in memory

- Code patterns, conventions, architecture, file paths, or project structure — these can be derived by reading the current project state.
- Git history, recent changes, or who-changed-what — `git log` / `git blame` are authoritative.
- Debugging solutions or fix recipes — the fix is in the code; the commit message has the context.
- Anything already documented in AGENTS.md files.
- Ephemeral task details: in-progress work, temporary state, current conversation context.

These exclusions apply even when the user explicitly asks you to save. If they ask you to save a PR list or activity summary, ask what was *surprising* or *non-obvious* about it — that is the part worth keeping.

## How to save memories

Saving a memory is a two-step process:

**Step 1** — write the memory to its own file (e.g., `user_role.md`, `feedback_testing.md`) using this frontmatter format:

```markdown
---
name: {{{{memory name}}}}
description: {{{{one-line description — used to decide relevance in future conversations, so be specific}}}}
type: {{{{user, feedback, project, reference}}}}
---

{{{{memory content — for feedback/project types, structure as: rule/fact, then **Why:** and **How to apply:** lines}}}}
```

**Step 2** — add a pointer to that file in `MEMORY.md`. `MEMORY.md` is an index, not a memory — each entry should be one line, under ~150 characters: `- [Title](file.md) — one-line hook`. It has no frontmatter. Never write memory content directly into `MEMORY.md`.

- `MEMORY.md` is always loaded into your conversation context, so keep the index concise
- Keep the name, description, and type fields in memory files up-to-date with the content
- Organize memory semantically by topic, not chronologically
- Update or remove memories that turn out to be wrong or outdated
- Do not write duplicate memories. First check if there is an existing memory you can update before writing a new one.

## When to access memories
- When memories seem relevant, or the user references prior-conversation work.
- You MUST access memory when the user explicitly asks you to check, recall, or remember.
- If the user says to *ignore* or *not use* memory: proceed as if MEMORY.md were empty. Do not apply remembered facts, cite, compare against, or mention memory content.
- Memory records can become stale over time. Use memory as context for what was true at a given point in time. Before answering the user or building assumptions based solely on information in memory records, verify that the memory is still correct and up-to-date by reading the current state of the files or resources. If a recalled memory conflicts with current information, trust what you observe now — and update or remove the stale memory rather than acting on it.

## Before recommending from memory

A memory that names a specific function, file, or flag is a claim that it existed *when the memory was written*. It may have been renamed, removed, or never merged. Before recommending it:

- If the memory names a file path: check the file exists.
- If the memory names a function or flag: grep for it.
- If the user is about to act on your recommendation (not just asking about history), verify first.

"The memory says X exists" is not the same as "X exists now."

A memory that summarizes repo state (activity logs, architecture snapshots) is frozen in time. If the user asks about *recent* or *current* state, prefer `git log` or reading the code over recalling the snapshot.

## Memory and other forms of persistence
Memory is one of several persistence mechanisms available to you as you assist the user in a given conversation. The distinction is often that memory can be recalled in future conversations and should not be used for persisting information that is only useful within the scope of the current conversation.
- When to use or update a plan instead of memory: If you are about to start a non-trivial implementation task and would like to reach alignment with the user on your approach you should use a Plan rather than saving this information to memory. Similarly, if you already have a plan within the conversation and you have changed your approach persist that change by updating the plan rather than saving a memory.
- When to use or update tasks instead of memory: When you need to break your work in current conversation into discrete steps or keep track of your progress use tasks instead of saving to memory. Tasks are great for persisting information about the work that needs to be done in the current conversation, but memory should be reserved for information that will be useful in future conversations."#
    )
}

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
        let idx = MemoryIndex::load(&dir).expect("load");
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
        let idx = MemoryIndex::load(&dir).expect("load");
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
            &FileMemoryProvider::new(),
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

        let idx = MemoryIndex::load(&dir).expect("load");
        assert!(!idx.is_empty());

        let ctx = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Compact);
        let prompt = append_from_provider(
            SystemPromptBuilder::new().with_os("linux", "test"),
            &FileMemoryProvider::new(),
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
            compact.contains("update that file instead of duplicating"),
            "dedupe-before-write rule"
        );
        assert!(
            compact.contains("not guarantees about the present"),
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
        let idx = MemoryIndex::load(&dir).expect("load");
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
