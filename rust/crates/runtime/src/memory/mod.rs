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
pub use loader::{
    agent_memory_dir_for, default_memory_dir, default_memory_dir_for, MEMORY_DIR_ENV,
    MEMORY_INDEX_FILE,
};

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
    ///
    /// `memory_dir` is the resolved path to the memory directory, templated
    /// into the instructions so the model knows where to write.
    #[must_use]
    pub fn render_for_prompt(&self, memory_dir: &Path) -> String {
        let mut out = String::new();
        out.push_str(&build_auto_memory_instructions(memory_dir));
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
    // Always inject the auto-memory instructions (even when empty) so the
    // model knows the memory directory path and how to save/forget entries.
    match MemoryIndex::load(dir) {
        Ok(idx) => builder.append_section(idx.render_for_prompt(dir)),
        _ => {
            let empty = MemoryIndex {
                directory: dir.to_path_buf(),
                ..Default::default()
            };
            builder.append_section(empty.render_for_prompt(dir))
        }
    }
}

/// Build the full auto-memory instructions section, matching CC's
/// `buildMemoryLines()` from `memdir.ts`. The memory directory path
/// is templated in so the model knows where to write.
fn build_auto_memory_instructions(memory_dir: &Path) -> String {
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

- `MEMORY.md` is always loaded into your conversation context — lines after 200 will be truncated, so keep the index concise
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
    fn append_to_builder_injects_instructions_even_when_empty() {
        let dir = temp_dir("empty-instructions");
        let appended = append_to_builder(
            SystemPromptBuilder::new().with_os("linux", "test"),
            Some(&dir),
            None,
        )
        .render();
        // Auto-memory instructions are always injected so the model
        // knows the memory directory path and how to save entries.
        assert!(appended.contains("# auto memory"));
        assert!(appended.contains(&dir.display().to_string()));
        assert!(appended.contains("(no memory entries loaded)"));
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

        assert!(prompt.contains("# auto memory"));
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
