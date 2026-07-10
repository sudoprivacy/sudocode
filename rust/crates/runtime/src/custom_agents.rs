//! Custom sub-agent definitions parsed from `~/.claude/agents/*.md`
//! (and sibling directories). Ports the CC-fork behaviour documented
//! in `sudoprivacy/claude-code/src/tools/AgentTool/loadAgentsDir.ts`
//! for the parity target on `.md`-with-YAML-frontmatter agent files.
//!
//! ## Layout expected on disk
//!
//! ```markdown
//! ---
//! name: my-researcher
//! description: Only responds with candidate names.
//! model: opus
//! tools: [read_file, glob_search, grep_search]
//! permissionMode: read-only
//! memory: project
//! omitClaudeMd: true
//! ---
//! You are a naming committee.  Reply with names, one per line.  Do
//! not explain or elaborate.
//! ```
//!
//! Files without frontmatter — or with frontmatter missing the
//! required `name` and `description` fields — are silently skipped
//! (they're typically co-located reference docs, not agent
//! definitions). This mirrors CC-fork's
//! `parseAgentFromMarkdown → return null` fall-through at
//! `loadAgentsDir.ts:554-561`.
//!
//! ## Search paths
//!
//! `standard_custom_agent_dirs(cwd)` returns, in priority order:
//! 1. `~/.claude/agents/`
//! 2. `~/.nexus/sudocode/agents/`
//! 3. `<cwd>/.claude/agents/`
//! 4. `<cwd>/.sudocode/agents/`
//!
//! First hit wins per-name — mirrors CC-fork's user/project/managed
//! precedence.  The order is intentionally user-scope before
//! project-scope so a user override for a shared team name takes
//! effect.
//!
//! ## Scope vs. sudocode's TOML-agents inventory
//!
//! Sudocode's `commands` crate reads `~/.claude/agents/*.toml` and
//! `~/.codex/agents/*.toml` for its `/agents` slash-command
//! inventory. This module reads `*.md` files from overlapping
//! directories — no collision because the file extensions differ.
//! A future contributor adding an agent should pick ONE format:
//! `.toml` for the slash-command inventory (sudocode-native), or
//! `.md` for the CC-fork-compatible sub-agent parity path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Parsed `.md`-with-YAML-frontmatter custom sub-agent definition.
///
/// Field mapping to CC-fork's `BaseAgentDefinition` (see
/// `loadAgentsDir.ts:106-133`):
///
/// | Sudocode field       | CC field                | Notes                                             |
/// |----------------------|-------------------------|---------------------------------------------------|
/// | `name`               | `agentType`             | Required. Becomes the `subagent_type` alias.      |
/// | `description`        | `whenToUse`             | Required. Free-form guidance for the caller.      |
/// | `tools`              | `tools`                 | `None` → inherit general-purpose set.             |
/// | `model`              | `model`                 | `Some("inherit")` means fall back to parent model.|
/// | `permission_mode`    | `permissionMode`        | Threaded to `PermissionMode::from_str` at spawn.  |
/// | `memory`             | `memory`                | `user` / `project` / `local`, or `None`.          |
/// | `omit_claude_md`     | `omitClaudeMd`          | Slim-subagent kill-switch; default `false`.       |
/// | `system_prompt`      | agent body              | Everything after the closing `---`.               |
/// | `source_path`        | `filename` + `baseDir`  | Reconstructed on-demand from this path.           |
///
/// Not (yet) ported: `disallowedTools`, `skills`, `mcpServers`,
/// `hooks`, `color`, `effort`, `maxTurns`,
/// `criticalSystemReminder_EXPERIMENTAL`, `requiredMcpServers`,
/// `background`, `initialPrompt`, `isolation`. When a downstream commit
/// needs one of them, the parser can be extended without touching
/// callers because [`load_md_agent_from_str`] is the sole entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomAgentDefinition {
    /// Agent identifier (the frontmatter `name` field). Becomes the
    /// canonical `subagent_type` when the Agent tool is invoked.
    pub name: String,
    /// Free-form guidance shown to the caller — CC's `whenToUse`.
    pub description: String,
    /// Explicit tool allowlist. `None` means "inherit the
    /// general-purpose default set." An empty vec is a legitimate
    /// (though useless) restriction — the agent gets NO tools.
    pub tools: Option<Vec<String>>,
    /// Model override.  `Some("inherit")` (case-insensitive) is
    /// preserved so downstream logic can decide to fall back to the
    /// parent model.
    pub model: Option<String>,
    /// Permission mode override, one of `read-only`,
    /// `workspace-write`, `plan`, `bypass`, `danger-full-access`.
    /// Kept as a string here so runtime keeps its choice of enum.
    pub permission_mode: Option<String>,
    /// Memory scope: `user`, `project`, `local`, or `None`.
    pub memory: Option<String>,
    /// When `true`, the parent CLAUDE.md hierarchy is omitted from the
    /// child's system prompt — matches CC's cost-saving flag.
    pub omit_claude_md: bool,
    /// Everything after the closing `---` delimiter, verbatim.  Used
    /// as the child's system-prompt body.
    pub system_prompt: String,
    /// Absolute path to the .md file on disk. Handy for diagnostics.
    pub source_path: PathBuf,
}

/// Search paths for custom `.md` agent definitions, in precedence
/// order.  See module docs for the semantics.
#[must_use]
pub fn standard_custom_agent_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home_dir() {
        dirs.push(home.join(".claude").join("agents"));
        dirs.push(home.join(".nexus").join("sudocode").join("agents"));
    }
    dirs.push(cwd.join(".claude").join("agents"));
    dirs.push(cwd.join(".sudocode").join("agents"));
    dirs
}

/// Look up a custom agent by name across the standard search paths.
/// Returns `None` when no `.md` file matches.  Multiple `.md` files
/// with the same frontmatter `name` in different search paths → the
/// first-hit wins (user scope before project scope).
#[must_use]
pub fn find_custom_agent(name: &str, cwd: &Path) -> Option<CustomAgentDefinition> {
    for dir in standard_custom_agent_dirs(cwd) {
        if let Some(def) = find_named_agent_in_dir(&dir, name) {
            return Some(def);
        }
    }
    None
}

/// List every valid custom agent under `dir`. Files that fail to
/// parse (or are missing required fields) are silently skipped —
/// same tolerant behaviour as CC-fork.  Order is filesystem order.
#[must_use]
pub fn load_md_agents(dir: &Path) -> Vec<CustomAgentDefinition> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut agents = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(def) = load_md_agent_from_str(&contents, &path) {
                agents.push(def);
            }
        }
    }
    agents
}

/// Parse a single `.md` file's contents into a
/// [`CustomAgentDefinition`].  The parser is intentionally minimal
/// (see module docs for the supported frontmatter shape).  Returns
/// `None` when the file:
/// - has no `---` frontmatter fence,
/// - has frontmatter but no `name` field,
/// - has `name` but no `description` field.
///
/// Everything else falls back to sensible defaults so a partial
/// frontmatter still yields a usable agent.
#[must_use]
pub fn load_md_agent_from_str(contents: &str, source_path: &Path) -> Option<CustomAgentDefinition> {
    let (frontmatter, body) = split_frontmatter(contents)?;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;
    let mut model: Option<String> = None;
    let mut permission_mode: Option<String> = None;
    let mut memory: Option<String> = None;
    let mut omit_claude_md = false;

    for (key, raw_value) in parse_frontmatter_kv(frontmatter) {
        match key.as_str() {
            "name" => name = Some(strip_quotes(&raw_value).to_string()),
            "description" | "whenToUse" => {
                description = Some(strip_quotes(&raw_value).to_string());
            }
            "tools" => tools = Some(parse_tools_list(&raw_value)),
            "model" => model = Some(strip_quotes(&raw_value).to_string()),
            "permissionMode" | "permission_mode" => {
                permission_mode = Some(strip_quotes(&raw_value).to_string());
            }
            "memory" => memory = Some(strip_quotes(&raw_value).to_string()),
            "omitClaudeMd" | "omit_claude_md" => {
                omit_claude_md = parse_bool(&raw_value).unwrap_or(false);
            }
            _ => {
                // Unrecognised keys are silently accepted; a future
                // commit that adds e.g. `color` or `effort` support
                // will pick them up here without breaking existing
                // frontmatter authored ahead of time.
            }
        }
    }

    let name = name?;
    let description = description?;

    Some(CustomAgentDefinition {
        name,
        description,
        tools,
        model,
        permission_mode,
        memory,
        omit_claude_md,
        system_prompt: body.trim_start_matches('\n').to_string(),
        source_path: source_path.to_path_buf(),
    })
}

fn find_named_agent_in_dir(dir: &Path, name: &str) -> Option<CustomAgentDefinition> {
    for agent in load_md_agents(dir) {
        if agent.name == name {
            return Some(agent);
        }
    }
    None
}

/// Split a file's contents into `(frontmatter, body)`. Frontmatter is
/// everything BETWEEN the opening and closing `---` fences (first two
/// occurrences); body is everything AFTER the closing fence. Returns
/// `None` if the file lacks the fence pair.
///
/// Accepts both `\n` and `\r\n` line endings — Windows-authored
/// `.md` files must parse identically to Unix ones.
fn split_frontmatter(contents: &str) -> Option<(&str, &str)> {
    let normalized = contents.trim_start();
    let after_open = normalized.strip_prefix("---\n").or_else(|| {
        normalized
            .strip_prefix("---\r\n")
            .or_else(|| normalized.strip_prefix("---"))
    })?;
    // Now find the next line that is exactly `---` (± trailing \r).
    let mut acc = String::new();
    let mut rest = after_open;
    while let Some(newline) = rest.find('\n') {
        let (line_incl_nl, remainder) = rest.split_at(newline + 1);
        let line = line_incl_nl.trim_end_matches('\n').trim_end_matches('\r');
        if line == "---" {
            // Slice the frontmatter portion from the original
            // `after_open` slice so the caller receives a `&str` that
            // still lives in `contents` (avoids allocation).
            let frontmatter_len = acc.len();
            let frontmatter = &after_open[..frontmatter_len];
            return Some((frontmatter, remainder));
        }
        acc.push_str(line_incl_nl);
        rest = remainder;
    }
    None
}

/// Iterate `(key, raw_value)` pairs from a frontmatter block. Ignores
/// blank lines and lines starting with `#` (YAML comments). Splits on
/// the FIRST `:` — subsequent colons stay in the value (so URLs and
/// timestamps survive intact).
fn parse_frontmatter_kv(frontmatter: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        out.push((key.trim().to_string(), value.trim().to_string()));
    }
    out
}

/// Strip surrounding single/double quotes from a value if present.
fn strip_quotes(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// Parse `tools:` — accepts either a YAML-style flow sequence
/// `[a, b, c]`, a bare comma-separated list `a, b, c`, or the literal
/// `*` (which yields an empty vec, signalling "use the general-purpose
/// default" — matches CC-fork's `tools: ['*']` semantics).
///
/// Individual tool names are trimmed and quote-stripped so
/// `[  "read_file"  , 'glob_search' ]` parses cleanly.
fn parse_tools_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    // Strip quotes BEFORE the star-check so `tools: '*'`, `tools: "*"`,
    // and bare `tools: *` all resolve to the "inherit default" sentinel.
    let star_stripped = strip_quotes(trimmed);
    if star_stripped == "*" {
        return Vec::new();
    }
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    let mut names = BTreeSet::new();
    for part in inner.split(',') {
        let stripped = strip_quotes(part.trim()).trim();
        if stripped == "*" {
            continue;
        }
        if !stripped.is_empty() {
            names.insert(stripped.to_string());
        }
    }
    names.into_iter().collect()
}

fn parse_bool(raw: &str) -> Option<bool> {
    match strip_quotes(raw).trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// Return the current user's home dir. Delegated so tests can shim it
/// via env var without touching every call site.
fn home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return Some(PathBuf::from(home));
        }
    }
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        if !user_profile.trim().is_empty() {
            return Some(PathBuf::from(user_profile));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_agent(dir: &Path, filename: &str, contents: &str) -> PathBuf {
        let path = dir.join(filename);
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(&path, contents).expect("write");
        path
    }

    #[test]
    fn parse_minimal_agent_with_name_and_description() {
        let contents =
            "---\nname: my-researcher\ndescription: Names things.\n---\nYou are a namer.\n";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/my.md")).expect("parse");
        assert_eq!(def.name, "my-researcher");
        assert_eq!(def.description, "Names things.");
        assert!(def.tools.is_none(), "no tools -> inherit default");
        assert!(def.model.is_none());
        assert!(!def.omit_claude_md);
        assert!(def.system_prompt.contains("You are a namer."));
    }

    #[test]
    fn parse_returns_none_without_frontmatter() {
        let contents = "no fence here\njust body\n";
        assert!(load_md_agent_from_str(contents, Path::new("/tmp/x.md")).is_none());
    }

    #[test]
    fn parse_returns_none_without_name() {
        let contents = "---\ndescription: no name.\n---\nbody\n";
        assert!(load_md_agent_from_str(contents, Path::new("/tmp/x.md")).is_none());
    }

    #[test]
    fn parse_returns_none_without_description() {
        let contents = "---\nname: only-name\n---\nbody\n";
        assert!(load_md_agent_from_str(contents, Path::new("/tmp/x.md")).is_none());
    }

    #[test]
    fn parse_tools_flow_sequence() {
        let contents = "\
---
name: r
description: R.
tools: [read_file, glob_search, grep_search]
---
body
";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/r.md")).expect("parse");
        let tools = def.tools.expect("tools present");
        assert_eq!(tools, vec!["glob_search", "grep_search", "read_file"]);
    }

    #[test]
    fn parse_tools_star_means_default_inherit() {
        let contents = "---\nname: r\ndescription: R.\ntools: '*'\n---\nbody";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/r.md")).expect("parse");
        assert_eq!(def.tools.as_deref(), Some(&[] as &[String]));
    }

    #[test]
    fn parse_model_and_permission_mode_and_memory() {
        let contents = "\
---
name: r
description: R.
model: opus
permissionMode: read-only
memory: project
omitClaudeMd: true
---
body";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/r.md")).expect("parse");
        assert_eq!(def.model.as_deref(), Some("opus"));
        assert_eq!(def.permission_mode.as_deref(), Some("read-only"));
        assert_eq!(def.memory.as_deref(), Some("project"));
        assert!(def.omit_claude_md);
    }

    #[test]
    fn parse_handles_crlf_line_endings() {
        let contents = "---\r\nname: r\r\ndescription: R.\r\n---\r\nbody\r\n";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/r.md")).expect("parse");
        assert_eq!(def.name, "r");
        assert!(def.system_prompt.contains("body"));
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines_in_frontmatter() {
        let contents = "\
---
# this is a comment
name: r

description: R.
# another comment
model: opus
---
body";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/r.md")).expect("parse");
        assert_eq!(def.name, "r");
        assert_eq!(def.model.as_deref(), Some("opus"));
    }

    #[test]
    fn parse_strips_quotes_around_values() {
        let contents = "---\nname: \"quoted-name\"\ndescription: 'quoted desc'\n---\nbody";
        let def = load_md_agent_from_str(contents, Path::new("/tmp/r.md")).expect("parse");
        assert_eq!(def.name, "quoted-name");
        assert_eq!(def.description, "quoted desc");
    }

    #[test]
    fn load_md_agents_walks_directory_and_skips_non_md() {
        let dir = std::env::temp_dir().join(format!(
            "sudocode-custom-agents-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        write_temp_agent(
            &dir,
            "one.md",
            "---\nname: one\ndescription: one.\n---\nbody 1",
        );
        write_temp_agent(
            &dir,
            "two.md",
            "---\nname: two\ndescription: two.\n---\nbody 2",
        );
        // Should be silently skipped.
        write_temp_agent(&dir, "reference.md", "just prose, no frontmatter\n");
        write_temp_agent(&dir, "notes.txt", "unrelated file");

        let mut agents = load_md_agents(&dir);
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].name, "one");
        assert_eq!(agents[1].name, "two");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_named_agent_returns_first_match() {
        let dir = std::env::temp_dir().join(format!(
            "sudocode-find-named-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        write_temp_agent(
            &dir,
            "a.md",
            "---\nname: target\ndescription: hit.\n---\nprompt",
        );
        write_temp_agent(
            &dir,
            "b.md",
            "---\nname: other\ndescription: miss.\n---\nprompt",
        );

        let hit = find_named_agent_in_dir(&dir, "target").expect("found");
        assert_eq!(hit.name, "target");
        assert!(find_named_agent_in_dir(&dir, "nope").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
