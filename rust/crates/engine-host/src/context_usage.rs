//! The data behind `/context`: what the next request would carry, split into
//! the categories Claude Code's `/context` grid shows (system prompt, tools,
//! agent catalog, memory files, skills, messages).
//!
//! Every figure is read from the live runtime — the rendered [`SystemPrompt`]
//! sections and the exact `tools` array [`EngineApiClient`] would attach —
//! rather than re-derived from config, so the report cannot disagree with the
//! wire. Token counts use the same `bytes / 4 + 1` heuristic as the request
//! preflight; the headline total prefers the provider-reported occupancy of
//! the latest response when one exists.
//!
//! [`EngineApiClient`]: engine_core::EngineApiClient

use std::collections::BTreeSet;
use std::path::Path;

use runtime::agent_types::{available_agent_types, format_agent_line, AGENT_TYPES_SECTION_TAG};
use runtime::custom_agents::{load_md_agents, standard_custom_agent_dirs};
use runtime::{estimate_session_tokens, ProjectContext, SystemPrompt};

use crate::runtime_build::BuiltRuntime;

/// One named contributor inside a `/context` category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    pub name: String,
    /// Where it came from: an MCP server, a definition root, a file path.
    pub source: String,
    /// Estimated tokens; `0` for a deferred tool the API does not count yet.
    pub tokens: usize,
    /// `false` for a deferred tool whose schema is on the wire but not yet
    /// revealed (counted by the API only after `ToolSearch` discovers it).
    pub loaded: bool,
}

/// Context occupancy by category, for one live session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextUsage {
    /// Wire model id the request is built for.
    pub model: String,
    /// The model's context window (tokens).
    pub context_window: u32,
    /// Occupancy at which auto-compaction fires; everything above it is the
    /// reserved buffer the grid draws at the end.
    pub auto_compact_threshold: u32,
    /// Provider-reported context occupancy of the latest response, when a
    /// turn has completed — the same figure the status line's `ctx` segment
    /// and the auto-compaction trigger use.
    pub provider_context_tokens: Option<u32>,
    /// Built-in prompt blocks plus every dynamic section not attributed to a
    /// category below.
    pub system_prompt_tokens: usize,
    pub system_tools: Vec<ContextEntry>,
    pub mcp_tools: Vec<ContextEntry>,
    /// Tokens of the `<available-agent-types>` prompt section.
    pub agent_types_tokens: usize,
    pub agent_types: Vec<ContextEntry>,
    /// Tokens of the `# Project instructions` prompt section.
    pub memory_files_tokens: usize,
    pub memory_files: Vec<ContextEntry>,
    /// Tokens of the `# Available skills` prompt section.
    pub skills_tokens: usize,
    pub skills: Vec<ContextEntry>,
    /// Estimated tokens of the transcript the request carries.
    pub message_tokens: usize,
    pub message_count: usize,
}

impl ContextUsage {
    #[must_use]
    pub fn system_tools_tokens(&self) -> usize {
        self.system_tools.iter().map(|t| t.tokens).sum()
    }

    #[must_use]
    pub fn mcp_tools_tokens(&self) -> usize {
        self.mcp_tools.iter().map(|t| t.tokens).sum()
    }

    /// Sum of every category estimate — the fallback headline before the
    /// first response reports real occupancy.
    #[must_use]
    pub fn estimated_total_tokens(&self) -> usize {
        self.system_prompt_tokens
            + self.system_tools_tokens()
            + self.mcp_tools_tokens()
            + self.agent_types_tokens
            + self.memory_files_tokens
            + self.skills_tokens
            + self.message_tokens
    }

    /// The headline: provider-reported occupancy when available, else the
    /// category estimate.
    #[must_use]
    pub fn total_tokens(&self) -> usize {
        self.provider_context_tokens
            .filter(|tokens| *tokens > 0)
            .map_or_else(|| self.estimated_total_tokens(), |tokens| tokens as usize)
    }

    /// Tokens between the auto-compaction threshold and the window.
    #[must_use]
    pub fn reserved_tokens(&self) -> u32 {
        self.context_window
            .saturating_sub(self.auto_compact_threshold)
    }
}

/// Which `/context` category a rendered prompt section belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionCategory {
    SystemPrompt,
    MemoryFiles,
    Skills,
    AgentTypes,
}

/// Attribute a dynamic prompt section by its opening line. The three
/// cwd-derived sections each open with a fixed heading (`# Project
/// instructions`, `# Available skills`, `<available-agent-types>`); anything
/// else — environment, auto-memory instructions, deferred-tool listing, A2A
/// identity, caller-appended text — is plain system prompt.
fn classify_section(section: &str) -> SectionCategory {
    let first_line = section.lines().next().unwrap_or("").trim();
    if first_line == "# Project instructions" {
        SectionCategory::MemoryFiles
    } else if first_line == "# Available skills" {
        SectionCategory::Skills
    } else if first_line == format!("<{AGENT_TYPES_SECTION_TAG}>") {
        SectionCategory::AgentTypes
    } else {
        SectionCategory::SystemPrompt
    }
}

/// `bytes / 4 + 1`: the preflight heuristic, applied to prompt text.
fn estimate_text_tokens(text: &str) -> usize {
    text.len() / 4 + 1
}

/// The preflight heuristic applied to a serialized tool definition.
fn estimate_serialized_tokens<T: serde::Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len() / 4 + 1)
}

/// Split the system prompt into the attributed category totals.
fn prompt_section_totals(prompt: &SystemPrompt) -> (usize, usize, usize, usize) {
    let mut system = prompt
        .static_sections
        .iter()
        .map(|s| estimate_text_tokens(s))
        .sum::<usize>();
    let (mut memory, mut skills, mut agents) = (0, 0, 0);
    for section in &prompt.dynamic_sections {
        let tokens = estimate_text_tokens(section);
        match classify_section(section) {
            SectionCategory::SystemPrompt => system += tokens,
            SectionCategory::MemoryFiles => memory += tokens,
            SectionCategory::Skills => skills += tokens,
            SectionCategory::AgentTypes => agents += tokens,
        }
    }
    (system, memory, skills, agents)
}

/// Collect the `/context` figures for the session rooted at `cwd`.
///
/// Returns the default (all-zero) usage when the runtime is mid-rebuild and
/// momentarily absent, so the caller renders an empty grid rather than
/// failing the command.
#[must_use]
pub fn collect_context_usage(cwd: &Path, built: &BuiltRuntime) -> ContextUsage {
    let Some(runtime) = built.runtime_ref() else {
        return ContextUsage::default();
    };
    let client = runtime.api_client();
    let session = runtime.session();
    let model = client.model().to_string();

    let (system_prompt_tokens, memory_files_tokens, skills_tokens, agent_types_tokens) =
        prompt_section_totals(runtime.system_prompt());

    let pre_compact_discovered = session
        .compaction
        .as_ref()
        .map(|c| c.pre_compact_discovered_tools.clone())
        .unwrap_or_default();
    let mut system_tools = Vec::new();
    let mut mcp_tools = Vec::new();
    for definition in client.request_tool_definitions(&session.messages, &pre_compact_discovered) {
        let loaded = !definition.defer_loading;
        let tokens = if loaded {
            estimate_serialized_tokens(&definition)
        } else {
            0
        };
        match definition.name.strip_prefix("mcp__") {
            Some(rest) => {
                let (server, tool) = rest.split_once("__").unwrap_or(("", rest));
                mcp_tools.push(ContextEntry {
                    name: tool.to_string(),
                    source: server.to_string(),
                    tokens,
                    loaded,
                });
            }
            None => system_tools.push(ContextEntry {
                name: definition.name.clone(),
                source: String::new(),
                tokens,
                loaded,
            }),
        }
    }

    let custom_agent_names: BTreeSet<String> = standard_custom_agent_dirs(cwd)
        .iter()
        .flat_map(|dir| load_md_agents(dir))
        .map(|def| def.name)
        .collect();
    let agent_types = available_agent_types(cwd)
        .iter()
        .map(|listing| ContextEntry {
            tokens: estimate_text_tokens(&format_agent_line(listing)),
            source: if custom_agent_names.contains(&listing.name) {
                "Custom".to_string()
            } else {
                "Built-in".to_string()
            },
            name: listing.name.clone(),
            loaded: true,
        })
        .collect();

    let memory_files = ProjectContext::discover(cwd, runtime::today_local())
        .map(|context| {
            context
                .instruction_files
                .iter()
                .map(|file| ContextEntry {
                    name: file.path.display().to_string(),
                    source: String::new(),
                    tokens: estimate_text_tokens(&file.content),
                    loaded: true,
                })
                .collect()
        })
        .unwrap_or_default();

    let skills = commands::skill_prompt_entries(cwd, Some(built.plugin_load_outcome()))
        .into_iter()
        .map(|entry| ContextEntry {
            name: entry.name,
            source: entry.source.to_string(),
            tokens: entry.tokens,
            loaded: true,
        })
        .collect();

    let provider_context_tokens = Some(runtime.usage().current_context_tokens()).filter(|t| *t > 0);

    ContextUsage {
        context_window: runtime::model_capabilities::context_window_or_default(&model),
        auto_compact_threshold: runtime::auto_compact_threshold_for_model(&model),
        provider_context_tokens,
        model,
        system_prompt_tokens,
        system_tools,
        mcp_tools,
        agent_types_tokens,
        agent_types,
        memory_files_tokens,
        memory_files,
        skills_tokens,
        skills,
        message_tokens: estimate_session_tokens(session),
        message_count: session.messages.len(),
    }
}
