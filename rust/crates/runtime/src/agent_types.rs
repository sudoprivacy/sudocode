//! Agent TYPES — the catalog of what `agent_spawn` can be asked to be.
//!
//! One registry for the built-in presets, merged with the custom `.md`
//! definitions [`crate::custom_agents`] parses off disk. Everything that needs
//! to know "which agent types exist" reads from here:
//!
//! | consumer | what it takes |
//! |---|---|
//! | `tools::allowed_tools_for_subagent` | the per-preset tool pool |
//! | `tools::is_builtin_subagent` | the set of names a `.md` file may not shadow |
//! | `engine_host::runtime_build` | [`render_agent_types_prompt_section`] |
//!
//! Before this module those three carried their own copy of the preset list —
//! names in one place, descriptions in a second, tool pools in a third — so a
//! new preset had to be added three times and a reworded description drifted
//! silently. The catalog is data here and derived everywhere else.
//!
//! ## Why the catalog is a prompt section and not a tool
//!
//! A tool that lists agent types (scode once had `agent_list`) is the wrong
//! shape twice over. Claude Code has no such tool; it puts the list in the
//! conversation and keeps the *tool description* static, because the list is
//! volatile — loading a plugin, reconnecting an MCP server, or changing
//! permission mode mutates it. CC measured the cost of letting that volatility
//! sit in a tool description at **~10.2% of fleet `cache_creation` tokens**: the
//! description changes, so the whole tools-block prompt cache busts
//! (`claude-code/src/tools/AgentTool/prompt.ts:53-56`).
//!
//! `agent_spawn` is a CORE tool, so its description rides in that same cached
//! tools block. Hence the division of labour this module enforces:
//!
//! - `agent_spawn`'s description and schema **never mention a specific agent
//!   type** — they stay byte-identical no matter what is installed.
//! - The volatile list lives in the `<available-agent-types>` dynamic system
//!   prompt section, which may change freely.
//!
//! A test in `tools` pins the static half; treat a failure there as the cache
//! property breaking, not as a stale assertion to update.
//!
//! ## Line format
//!
//! `- <name>: <when_to_use> (Tools: <pool>)` — CC's `formatAgentLine`
//! (`prompt.ts:43`) verbatim, so a model trained on the CC tool set reads a
//! familiar shape.
//!
//! `when_to_use` says *when to pick this type* and deliberately does not
//! restate the tool list, which the `(Tools: …)` column already carries.
//!
//! ## The `*` pool
//!
//! `*` marks the inherited general-purpose pool ([`GENERAL_PURPOSE_TOOLS`]),
//! matching how a custom `.md` agent spells "no restriction" in its
//! frontmatter. It is a named default, NOT "every tool in the process" — the
//! pool deliberately withholds the delegation surface (`agent_spawn`, `send`,
//! `AskUserQuestion`), which is why `fork` has to name those explicitly rather
//! than inherit.
//!
//! ## Why `fork` is in the registry but not in the listing
//!
//! `fork` is a real `subagent_type` with its own tool pool, so it belongs in
//! the SSOT — `is_builtin_subagent("fork")` must stay true or a `.md` file
//! could shadow it. But it is a *mode* of spawning (inherit the parent's
//! context and prompt cache), not a specialization to choose between, and it
//! carries preconditions that need prose. CC draws the same line: its fork
//! semantics live in the Agent tool's static description while the agent-types
//! list stays a list of specializations. So `fork` sets [`BuiltinAgentType::listed`]
//! `= false`, and the one place that decides this is the registry row.

use std::collections::BTreeSet;
use std::path::Path;

/// Which tools a subagent of a given type may invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentToolPool {
    /// Inherit [`GENERAL_PURPOSE_TOOLS`]. Rendered as `*`.
    GeneralPurpose,
    /// An explicit allowlist.
    Only(&'static [&'static str]),
}

/// One built-in agent type. See the module docs for how `listed` is decided.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinAgentType {
    /// Canonical `subagent_type` value.
    pub name: &'static str,
    /// CC's `whenToUse`: when a caller should pick this type.
    pub when_to_use: &'static str,
    /// The tool pool this type runs with.
    pub tools: AgentToolPool,
    /// Whether the type appears in `<available-agent-types>`.
    pub listed: bool,
}

/// The maximal pool a general-purpose subagent may invoke, and the fallback
/// for an unknown preset name or a custom `.md` agent that declines to
/// restrict itself (`tools: '*'`, an empty list, or no `tools` key).
///
/// Withholds the delegation surface on purpose — see the module docs.
pub const GENERAL_PURPOSE_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "edit_file",
    "glob_search",
    "grep_search",
    "WebFetch",
    "WebSearch",
    "TaskCreate",
    "TaskUpdate",
    "TaskList",
    "Skill",
    "ToolSearch",
    "Sleep",
    "Config",
    "StructuredOutput",
    "PowerShell",
];

/// Every built-in agent type, in the order they are listed to the model.
pub const BUILTIN_AGENT_TYPES: &[BuiltinAgentType] = &[
    BuiltinAgentType {
        name: "general-purpose",
        when_to_use: "Research, implementation, and multi-step tasks. The default when no \
                      specialized type fits.",
        tools: AgentToolPool::GeneralPurpose,
        listed: true,
    },
    BuiltinAgentType {
        name: "Explore",
        when_to_use: "Read-only research: locating code and sweeping many files to answer a \
                      question without editing anything.",
        tools: AgentToolPool::Only(&[
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
        ]),
        listed: true,
    },
    BuiltinAgentType {
        name: "Plan",
        when_to_use: "Designing an implementation strategy before any code changes — read-only \
                      exploration plus task tracking.",
        tools: AgentToolPool::Only(&[
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "StructuredOutput",
        ]),
        listed: true,
    },
    BuiltinAgentType {
        name: "Verification",
        when_to_use: "Running tests and checks to verify a change, with bash on top of the \
                      read-only set.",
        tools: AgentToolPool::Only(&[
            "bash",
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "StructuredOutput",
            "PowerShell",
        ]),
        listed: true,
    },
    BuiltinAgentType {
        name: "scode-guide",
        when_to_use: "Questions about using scode itself — its tools, config, and workflows.",
        tools: AgentToolPool::Only(&[
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
        ]),
        listed: true,
    },
    BuiltinAgentType {
        name: "statusline-setup",
        when_to_use: "Configuring the status line display.",
        tools: AgentToolPool::Only(&[
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "ToolSearch",
        ]),
        listed: true,
    },
    BuiltinAgentType {
        name: "fork",
        when_to_use: "Inherit the parent's context and prompt cache instead of starting fresh.",
        // Mirrors CC-fork's `tools: ['*']`. sudocode does not thread the
        // parent's `allowed_tools` into `prepare_agent_job`, so `*` is
        // approximated as the general-purpose pool PLUS the delegation tools a
        // fork child still needs — it may spawn NON-fork sub-agents (recursion
        // is blocked at call time by `ToolDispatchContext::is_inside_fork_child`)
        // and report back to its parent.
        tools: AgentToolPool::Only(&[
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "Skill",
            "ToolSearch",
            "Sleep",
            "Config",
            "StructuredOutput",
            "PowerShell",
            "Agent",
            "SendMessage",
        ]),
        // See the module docs: a spawn MODE, not a specialization.
        listed: false,
    },
];

/// The fallback pool, owned: [`GENERAL_PURPOSE_TOOLS`] as a set.
#[must_use]
pub fn general_purpose_tools() -> BTreeSet<String> {
    GENERAL_PURPOSE_TOOLS
        .iter()
        .map(|t| (*t).to_string())
        .collect()
}

/// Look up a built-in type by its exact canonical name.
#[must_use]
pub fn builtin_agent_type(name: &str) -> Option<&'static BuiltinAgentType> {
    BUILTIN_AGENT_TYPES.iter().find(|a| a.name == name)
}

/// Whether `name` is a built-in preset. Built-ins win over a same-named custom
/// `.md` file, so this is also the shadowing guard.
#[must_use]
pub fn is_builtin_agent_type(name: &str) -> bool {
    builtin_agent_type(name).is_some()
}

/// The tool allowlist for a built-in type, resolved through
/// [`AgentToolPool::GeneralPurpose`]. `None` when `name` is not a built-in.
#[must_use]
pub fn builtin_allowed_tools(name: &str) -> Option<BTreeSet<String>> {
    builtin_agent_type(name).map(|a| match a.tools {
        AgentToolPool::GeneralPurpose => general_purpose_tools(),
        AgentToolPool::Only(tools) => tools.iter().map(|t| (*t).to_string()).collect(),
    })
}

/// One row of the catalog shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTypeListing {
    /// Canonical `subagent_type` value.
    pub name: String,
    /// When a caller should pick this type.
    pub when_to_use: String,
    /// The rendered `(Tools: …)` column: `*` for the inherited
    /// general-purpose pool, else a comma-joined allowlist.
    pub tools: String,
}

/// `*` — the inherited general-purpose pool. See the module docs.
const INHERITED_POOL: &str = "*";

fn render_pool(pool: AgentToolPool) -> String {
    match pool {
        AgentToolPool::GeneralPurpose => INHERITED_POOL.to_string(),
        AgentToolPool::Only(tools) => tools.join(", "),
    }
}

/// Every agent type a caller may pass to `agent_spawn`, built-ins first and
/// then the custom `.md` definitions under
/// [`crate::custom_agents::standard_custom_agent_dirs`].
///
/// Built-ins are never shadowed (a same-named `.md` file is skipped, matching
/// [`is_builtin_agent_type`]), and a custom name is listed once even when the
/// same `name` appears in both the user and project search path — first hit
/// wins, the precedence [`crate::custom_agents::find_custom_agent`] already
/// applies for resolution.
#[must_use]
pub fn available_agent_types(cwd: &Path) -> Vec<AgentTypeListing> {
    let mut listings: Vec<AgentTypeListing> = BUILTIN_AGENT_TYPES
        .iter()
        .filter(|a| a.listed)
        .map(|a| AgentTypeListing {
            name: a.name.to_string(),
            when_to_use: a.when_to_use.to_string(),
            tools: render_pool(a.tools),
        })
        .collect();

    let mut seen: BTreeSet<String> = BUILTIN_AGENT_TYPES
        .iter()
        .map(|a| a.name.to_string())
        .collect();

    for dir in crate::custom_agents::standard_custom_agent_dirs(cwd) {
        for def in crate::custom_agents::load_md_agents(&dir) {
            if !seen.insert(def.name.clone()) {
                continue;
            }
            // `Some(non-empty)` is a real restriction; `Some(empty)` (from
            // `tools: '*'`) and `None` both mean "inherit" — the same three-way
            // split `builtin_allowed_tools`'s caller applies.
            let tools = match def.tools.as_deref() {
                Some(list) if !list.is_empty() => list.join(", "),
                _ => INHERITED_POOL.to_string(),
            };
            listings.push(AgentTypeListing {
                name: def.name,
                when_to_use: def.description,
                tools,
            });
        }
    }

    listings
}

/// Format one catalog row. CC's `formatAgentLine` (`prompt.ts:43`).
#[must_use]
pub fn format_agent_line(listing: &AgentTypeListing) -> String {
    format!(
        "- {}: {} (Tools: {})",
        listing.name, listing.when_to_use, listing.tools
    )
}

/// The XML tag wrapping the catalog in the system prompt.
pub const AGENT_TYPES_SECTION_TAG: &str = "available-agent-types";

/// Render the `<available-agent-types>` dynamic system-prompt section.
///
/// Injected next to the skills and deferred-tool sections in
/// `engine_host::runtime_build`, so the REPL, `--print`, and ACP sessions all
/// get it. Never empty: the built-ins always exist, and a model that can call
/// `agent_spawn` always needs to know what to pass.
#[must_use]
pub fn render_agent_types_prompt_section(cwd: &Path) -> String {
    let listings = available_agent_types(cwd);
    let mut lines = vec![
        format!("<{AGENT_TYPES_SECTION_TAG}>"),
        "Agent types available to `agent_spawn`, passed as its `agent` argument. `Tools: *` means \
         the agent inherits the default general-purpose tool set."
            .to_string(),
    ];
    lines.extend(listings.iter().map(format_agent_line));
    lines.push(format!("</{AGENT_TYPES_SECTION_TAG}>"));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is the SSOT: every name `tools` treats as a built-in must
    /// resolve to a pool here, or `allowed_tools_for_subagent` silently falls
    /// back to general-purpose and a restricted preset quietly gains tools.
    #[test]
    fn every_builtin_resolves_to_a_non_empty_pool() {
        for agent in BUILTIN_AGENT_TYPES {
            let pool = builtin_allowed_tools(agent.name)
                .unwrap_or_else(|| panic!("`{}` must resolve to a pool", agent.name));
            assert!(
                !pool.is_empty(),
                "`{}` resolved to an empty tool pool",
                agent.name
            );
            assert!(
                pool.contains("read_file"),
                "`{}` must be able to read files",
                agent.name
            );
        }
    }

    #[test]
    fn general_purpose_inherits_the_default_pool() {
        assert_eq!(
            builtin_allowed_tools("general-purpose").unwrap(),
            general_purpose_tools()
        );
    }

    /// `fork` is a spawn mode, not a specialization — it must stay resolvable
    /// (so a `.md` file cannot shadow it) while staying out of the catalog the
    /// model chooses from.
    #[test]
    fn fork_is_registered_but_not_listed() {
        assert!(is_builtin_agent_type("fork"));
        let listed = available_agent_types(Path::new("/nonexistent-workspace"));
        assert!(
            !listed.iter().any(|l| l.name == "fork"),
            "`fork` must not appear in the agent-type catalog"
        );
    }

    #[test]
    fn unknown_name_is_not_a_builtin() {
        assert!(!is_builtin_agent_type("some-unknown-name"));
        assert!(builtin_allowed_tools("some-unknown-name").is_none());
    }

    #[test]
    fn agent_line_matches_cc_format() {
        let line = format_agent_line(&AgentTypeListing {
            name: "Explore".to_string(),
            when_to_use: "Read-only research.".to_string(),
            tools: "read_file, glob_search".to_string(),
        });
        assert_eq!(
            line,
            "- Explore: Read-only research. (Tools: read_file, glob_search)"
        );
    }

    #[test]
    fn section_lists_every_listed_builtin_in_cc_format() {
        let section = render_agent_types_prompt_section(Path::new("/nonexistent-workspace"));
        assert!(section.starts_with("<available-agent-types>"));
        assert!(section.ends_with("</available-agent-types>"));
        for agent in BUILTIN_AGENT_TYPES.iter().filter(|a| a.listed) {
            assert!(
                section.contains(&format!("- {}: ", agent.name)),
                "section must list `{}`:\n{section}",
                agent.name
            );
        }
        // The `*` legend has to be present whenever a `*` row is, or the model
        // reads it as a literal tool name.
        assert!(section.contains("(Tools: *)"));
        assert!(section.contains("inherits the default general-purpose tool set"));
    }

    fn temp_workspace(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "sudocode-agent-types-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join(".sudocode").join("agents")).expect("create agents dir");
        root
    }

    fn write_agent(workspace: &Path, file: &str, contents: &str) {
        std::fs::write(
            workspace.join(".sudocode").join("agents").join(file),
            contents,
        )
        .expect("write agent file");
    }

    #[test]
    fn custom_md_agents_join_the_catalog_with_their_tool_column() {
        let ws = temp_workspace("custom");
        write_agent(
            &ws,
            "narrow.md",
            "---\nname: narrow\ndescription: Only reads.\ntools: [read_file, grep_search]\n---\nbody",
        );
        write_agent(
            &ws,
            "wide.md",
            "---\nname: wide\ndescription: Unrestricted.\n---\nbody",
        );

        let listings = available_agent_types(&ws);
        let narrow = listings
            .iter()
            .find(|l| l.name == "narrow")
            .expect("custom agent must be listed");
        assert_eq!(narrow.when_to_use, "Only reads.");
        // Sorted, not frontmatter order: `parse_tools_list` collects into a
        // `BTreeSet`, so the same tool set always renders to the same string
        // and an unchanged catalog never perturbs the prompt.
        assert_eq!(narrow.tools, "grep_search, read_file");

        let wide = listings
            .iter()
            .find(|l| l.name == "wide")
            .expect("unrestricted custom agent must be listed");
        // No `tools:` key ⇒ inherit, same as `tools: '*'`.
        assert_eq!(wide.tools, INHERITED_POOL);

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A built-in must win over a same-named `.md` file, and must not be
    /// listed twice — the catalog and [`is_builtin_agent_type`] have to agree
    /// or the model is offered a type that resolves to different tools than
    /// advertised.
    #[test]
    fn custom_md_agent_cannot_shadow_or_duplicate_a_builtin() {
        let ws = temp_workspace("shadow");
        write_agent(
            &ws,
            "explore.md",
            "---\nname: Explore\ndescription: Impostor.\ntools: [bash]\n---\nbody",
        );

        let listings = available_agent_types(&ws);
        let explore: Vec<_> = listings.iter().filter(|l| l.name == "Explore").collect();
        assert_eq!(explore.len(), 1, "`Explore` must be listed exactly once");
        assert_ne!(explore[0].when_to_use, "Impostor.");
        assert!(!explore[0].tools.contains("bash"));

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// A `when_to_use` that restates the tool list would print it twice, since
    /// `(Tools: …)` already carries it.
    #[test]
    fn when_to_use_does_not_restate_the_tool_list() {
        for agent in BUILTIN_AGENT_TYPES {
            for needle in ["read_file", "glob_search", "grep_search", "WebFetch"] {
                assert!(
                    !agent.when_to_use.contains(needle),
                    "`{}`'s when_to_use restates the tool column: {needle}",
                    agent.name
                );
            }
        }
    }
}
