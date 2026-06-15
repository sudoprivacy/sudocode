//! Subagent type registry.
//!
//! A [`SubagentRegistry`] holds the catalog of available subagent types — each
//! described by a name, a human-readable purpose, and a [`ToolFilter`] that
//! constrains which tools the subagent is allowed to use. The registry is the
//! single source of truth that the prompt-injection helper and the dispatch
//! stub both consult.

/// Tool surface a subagent is permitted to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFilter {
    /// Subagent inherits the full host tool set.
    All,
    /// Subagent may only use the listed tools.
    Allow(Vec<String>),
    /// Subagent may use everything except the listed tools.
    Deny(Vec<String>),
}

impl ToolFilter {
    /// Returns `true` iff `tool` is allowed by this filter.
    #[must_use]
    pub fn allows(&self, tool: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allow(list) => list.iter().any(|t| t == tool),
            Self::Deny(list) => list.iter().all(|t| t != tool),
        }
    }
}

/// A single subagent type definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentType {
    pub name: String,
    pub description: String,
    pub tools_allowed: ToolFilter,
}

impl SubagentType {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        tools_allowed: ToolFilter,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            tools_allowed,
        }
    }
}

/// Catalog of subagent types available to the runtime.
#[derive(Debug, Clone, Default)]
pub struct SubagentRegistry {
    types: Vec<SubagentType>,
}

impl SubagentRegistry {
    /// Empty registry. Useful for tests and for callers that want to
    /// opt out of the built-in subagent set.
    #[must_use]
    pub fn empty() -> Self {
        Self { types: Vec::new() }
    }

    /// Registry pre-populated with the standard built-in subagent set:
    /// `explore`, `general-purpose`, `plan`, `code-reviewer`.
    #[must_use]
    pub fn builtin() -> Self {
        let mut registry = Self::empty();
        registry.register(SubagentType::new(
            "explore",
            "Fast read-only search agent for locating code.",
            ToolFilter::Allow(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string(),
            ]),
        ));
        registry.register(SubagentType::new(
            "general-purpose",
            "Multi-step research and execution agent.",
            ToolFilter::All,
        ));
        registry.register(SubagentType::new(
            "plan",
            "Software architect for designing implementation plans.",
            ToolFilter::Deny(vec![
                "Edit".to_string(),
                "Write".to_string(),
                "NotebookEdit".to_string(),
            ]),
        ));
        registry.register(SubagentType::new(
            "code-reviewer",
            "Reviews diffs for correctness.",
            ToolFilter::Deny(vec![
                "Edit".to_string(),
                "Write".to_string(),
                "NotebookEdit".to_string(),
            ]),
        ));
        registry
    }

    /// Add or replace a subagent type. Replacement is by `name`.
    pub fn register(&mut self, sa: SubagentType) {
        if let Some(slot) = self.types.iter_mut().find(|t| t.name == sa.name) {
            *slot = sa;
        } else {
            self.types.push(sa);
        }
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SubagentType> {
        self.types.iter().find(|t| t.name == name)
    }

    #[must_use]
    pub fn list(&self) -> &[SubagentType] {
        &self.types
    }

    /// Render the prompt section describing available subagents. Returns an
    /// empty string when the registry is empty so callers can skip injection.
    #[must_use]
    pub fn render_for_prompt(&self) -> String {
        if self.types.is_empty() {
            return String::new();
        }

        let mut out = String::from("# Subagents\n\n");
        out.push_str(
            "You can delegate work to specialized subagents via the `dispatch_subagent` tool.\n\n",
        );
        out.push_str("Available subagent types:\n");
        for sa in &self.types {
            let tools = match &sa.tools_allowed {
                ToolFilter::All => "all".to_string(),
                ToolFilter::Allow(list) => list.join(", "),
                ToolFilter::Deny(list) => format!("all except {}", list.join(", ")),
            };
            out.push_str(&format!(
                "- {}: {} Tools: {}.\n",
                sa.name, sa.description, tools
            ));
        }
        out.push('\n');
        out.push_str("When to use a subagent:\n");
        out.push_str("- Broad codebase exploration (>3 queries) → `explore`\n");
        out.push_str("- Multi-step independent research → `general-purpose`\n");
        out.push_str("- Design before implementation → `plan`\n");
        out.push_str("- Verify diff correctness → `code-reviewer`\n\n");
        out.push_str("Do NOT duplicate work a subagent is already doing.");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_filter_all_allows_anything() {
        assert!(ToolFilter::All.allows("Edit"));
        assert!(ToolFilter::All.allows("anything"));
    }

    #[test]
    fn tool_filter_allow_is_exact() {
        let f = ToolFilter::Allow(vec!["Read".into(), "Grep".into()]);
        assert!(f.allows("Read"));
        assert!(!f.allows("Edit"));
    }

    #[test]
    fn tool_filter_deny_is_exact() {
        let f = ToolFilter::Deny(vec!["Edit".into()]);
        assert!(f.allows("Read"));
        assert!(!f.allows("Edit"));
    }

    #[test]
    fn register_replaces_by_name() {
        let mut reg = SubagentRegistry::empty();
        reg.register(SubagentType::new("x", "v1", ToolFilter::All));
        reg.register(SubagentType::new("x", "v2", ToolFilter::All));
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.get("x").unwrap().description, "v2");
    }
}
