//! Pluggable memory providers.
//!
//! Memory reaches the model through exactly one seam: a block of text
//! appended to the system prompt at build time. This module turns that seam
//! into a trait so backends other than the on-disk files in [`super::loader`]
//! can supply it.
//!
//! Two methods, split by *when* they run:
//!
//! - [`MemoryProvider::system_prompt_block`] is synchronous, because prompt
//!   building is. It runs once per prompt build.
//! - [`MemoryProvider::prefetch`] is async and query-scoped, meant to run in
//!   the turn loop *before* the prompt is rendered so a provider can recall
//!   against what the user just asked. Nothing calls it yet — it is declared
//!   here so the shape is fixed before backends start arriving.
//!
//! Note the asymmetry with MCP. An MCP server can expose memory as tools the
//! model chooses to call, but nothing on the MCP path writes into the system
//! prompt, so it cannot recall on the model's behalf before the model knows
//! to ask. Closing that gap is why this trait exists rather than leaving
//! memory to a well-known MCP server.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::loader::{
    agent_memory_dir_for, default_memory_dir, default_memory_dir_for, ensure_memory_dir_exists,
};
use super::{MemoryIndex, MemoryPromptVariant};

/// Where a memory lookup is happening, resolved once at the call site.
///
/// Directory resolution reproduces, in order, what callers used to do by
/// hand: an explicit directory wins, then per-agent scoping, then the
/// project-scoped default, then the process-wide default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryContext {
    /// Working directory the session was started in, when known.
    pub cwd: Option<PathBuf>,
    /// Sub-agent type this lookup is scoped to, when the caller is a spawn.
    /// Previously this was smuggled in as a pre-computed directory; keeping
    /// it here lets a provider scope by agent without parsing a path.
    pub agent_type: Option<String>,
    /// Which edition of the auto-memory instructions the call site wants.
    pub variant: MemoryPromptVariant,
    /// Directory the fields above resolved to.
    pub memory_dir: PathBuf,
}

impl MemoryContext {
    /// Resolve a context from the pieces a call site actually has.
    #[must_use]
    pub fn resolve(
        memory_dir: Option<&Path>,
        cwd: Option<&Path>,
        agent_type: Option<&str>,
        variant: MemoryPromptVariant,
    ) -> Self {
        let dir = match (memory_dir, cwd, agent_type) {
            (Some(dir), _, _) => dir.to_path_buf(),
            (None, Some(cwd), Some(agent)) => agent_memory_dir_for(cwd, agent),
            (None, Some(cwd), None) => default_memory_dir_for(cwd),
            (None, None, _) => default_memory_dir(),
        };
        Self {
            cwd: cwd.map(Path::to_path_buf),
            agent_type: agent_type.map(str::to_string),
            variant,
            memory_dir: dir,
        }
    }

    #[must_use]
    pub fn memory_dir(&self) -> &Path {
        &self.memory_dir
    }
}

/// A source of remembered context for the model.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Stable identifier, used in diagnostics and configuration.
    fn name(&self) -> &str;

    /// Whether this provider can run right now. A provider that needs a
    /// network service or a missing binary reports `false` and is skipped,
    /// rather than failing the prompt build.
    fn is_available(&self) -> bool {
        true
    }

    /// Text to append to the system prompt. `None` means "contribute
    /// nothing" and must not be used to signal an error.
    fn system_prompt_block(&self, ctx: &MemoryContext) -> Option<String>;

    /// Recall scoped to a query, for the turn loop to await before the
    /// prompt is rendered. Providers that only inject static context leave
    /// this at the default.
    async fn prefetch(&self, _query: &str, _ctx: &MemoryContext) -> Option<String> {
        None
    }
}

/// The on-disk provider: `MEMORY.md` plus one file per entry, rendered whole.
///
/// This is the behaviour every session has today, moved behind the trait
/// without changing a byte of what it emits.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileMemoryProvider;

impl FileMemoryProvider {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MemoryProvider for FileMemoryProvider {
    // The trait deliberately returns `&str` rather than `&'static str`: a
    // provider fronting an MCP server takes its name from configuration and
    // cannot hand back a literal. This impl happens to have one.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "file"
    }

    fn system_prompt_block(&self, ctx: &MemoryContext) -> Option<String> {
        let dir = ctx.memory_dir();
        ensure_memory_dir_exists(dir);
        // Always render, even with zero entries: the block carries the
        // auto-memory instructions telling the model where to write.
        let index = MemoryIndex::load(dir).unwrap_or_else(|_| MemoryIndex {
            directory: dir.to_path_buf(),
            ..Default::default()
        });
        Some(index.render_for_prompt_with(dir, ctx.variant))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::ENV_LOCK;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Same hand-rolled helper the sibling `loader` tests use; the crate has
    // no `tempfile` dev-dependency and this change does not add one.
    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("runtime-memory-provider-{prefix}-{nanos}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn explicit_dir_wins_over_cwd_and_agent() {
        let dir = temp_dir("explicit");
        let ctx = MemoryContext::resolve(
            Some(&dir),
            Some(Path::new("/some/cwd")),
            Some("x"),
            MemoryPromptVariant::Compact,
        );
        assert_eq!(ctx.memory_dir(), dir.as_path());
        assert_eq!(ctx.agent_type.as_deref(), Some("x"));
    }

    #[test]
    fn agent_type_scopes_under_agent_memory() {
        // HOME is process-global and other tests move it; hold the lock
        // across both the resolve and the expectation.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cwd = temp_dir("agent-cwd");
        let ctx =
            MemoryContext::resolve(None, Some(&cwd), Some("explore"), MemoryPromptVariant::Full);
        assert_eq!(ctx.memory_dir(), agent_memory_dir_for(&cwd, "explore"));
    }

    #[test]
    fn no_agent_type_uses_project_scoped_dir() {
        // HOME is process-global and other tests move it; hold the lock
        // across both the resolve and the expectation.
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cwd = temp_dir("project-cwd");
        let ctx = MemoryContext::resolve(None, Some(&cwd), None, MemoryPromptVariant::Compact);
        assert_eq!(ctx.memory_dir(), default_memory_dir_for(&cwd));
    }

    #[test]
    fn file_provider_renders_instructions_when_empty() {
        let dir = temp_dir("empty");
        let ctx = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Compact);
        let block = FileMemoryProvider::new()
            .system_prompt_block(&ctx)
            .expect("file provider always contributes a block");
        assert!(block.contains("# auto memory"), "block was: {block}");
    }

    #[test]
    fn file_provider_honors_the_variant() {
        let dir = temp_dir("variant");
        let compact = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Compact);
        let full = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Full);
        let provider = FileMemoryProvider::new();
        let compact_block = provider.system_prompt_block(&compact).expect("compact");
        let full_block = provider.system_prompt_block(&full).expect("full");
        assert!(
            full_block.len() > compact_block.len(),
            "full edition should be longer: {} vs {}",
            full_block.len(),
            compact_block.len()
        );
    }

    #[test]
    fn file_provider_renders_entries() {
        let dir = temp_dir("entries");
        fs::write(
            dir.join("thing.md"),
            "---\nname: thing\ndescription: a thing\nmetadata:\n  type: project\n---\n\nbody text\n",
        )
        .expect("write entry");
        let ctx = MemoryContext::resolve(Some(&dir), None, None, MemoryPromptVariant::Compact);
        let block = FileMemoryProvider::new()
            .system_prompt_block(&ctx)
            .expect("file provider always contributes a block");
        assert!(block.contains("body text"), "block was: {block}");
    }
}
