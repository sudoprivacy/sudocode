//! Subagent infrastructure for the runtime.
//!
//! Owns the catalog of subagent types ([`SubagentRegistry`]) and the dispatch
//! entry point that validates [`SubagentRequest`]s against that catalog.
//! Actual sub-runtime execution is deferred (see [`dispatch`]).
//!
//! The prompt section advertising available subagents is injected via
//! [`append_to_builder`] so callers can attach the section to an existing
//! `SystemPromptBuilder` without taking a hard dependency on its internals.

mod dispatch;
mod registry;

pub use dispatch::{DispatchStatus, SubagentRequest, SubagentResponse};
pub use registry::{SubagentRegistry, SubagentType, ToolFilter};

use crate::prompt::SystemPromptBuilder;

/// Append the subagent-catalog section to `builder` if the registry is
/// non-empty. Returns the builder unchanged when there are no subagents.
#[must_use]
pub fn append_to_builder(
    builder: SystemPromptBuilder,
    registry: &SubagentRegistry,
) -> SystemPromptBuilder {
    if registry.list().is_empty() {
        return builder;
    }
    builder.append_section(registry.render_for_prompt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_registry_has_expected_types() {
        let reg = SubagentRegistry::builtin();

        let explore = reg.get("explore").expect("explore registered");
        match &explore.tools_allowed {
            ToolFilter::Allow(list) => {
                assert!(list.contains(&"Read".to_string()));
                assert!(list.contains(&"Grep".to_string()));
                assert!(list.contains(&"Glob".to_string()));
                assert!(!list.contains(&"Edit".to_string()));
                assert!(!list.contains(&"Write".to_string()));
                assert!(!list.contains(&"Bash".to_string()));
            }
            other => panic!("explore should be Allow(...), got {other:?}"),
        }

        let general = reg
            .get("general-purpose")
            .expect("general-purpose registered");
        assert!(matches!(general.tools_allowed, ToolFilter::All));

        let plan = reg.get("plan").expect("plan registered");
        assert!(!plan.tools_allowed.allows("Edit"));
        assert!(!plan.tools_allowed.allows("Write"));
        assert!(plan.tools_allowed.allows("Read"));

        let reviewer = reg.get("code-reviewer").expect("code-reviewer registered");
        assert!(!reviewer.tools_allowed.allows("Edit"));
        assert!(!reviewer.tools_allowed.allows("Write"));
        assert!(reviewer.tools_allowed.allows("Read"));
    }

    #[test]
    fn test_dispatch_validates_unknown_type() {
        let reg = SubagentRegistry::builtin();
        let resp = reg.dispatch(SubagentRequest {
            subagent_type: "no-such-agent".into(),
            description: "x".into(),
            prompt: "y".into(),
        });
        match resp.status {
            DispatchStatus::Rejected(reason) => {
                assert!(reason.contains("no-such-agent"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_validates_known_type() {
        let reg = SubagentRegistry::builtin();
        let resp = reg.dispatch(SubagentRequest {
            subagent_type: "explore".into(),
            description: "find login".into(),
            prompt: "Locate the login handler.".into(),
        });
        assert_eq!(resp.status, DispatchStatus::Validated);
    }

    #[test]
    fn test_render_for_prompt_contains_all_builtins() {
        let rendered = SubagentRegistry::builtin().render_for_prompt();
        assert!(rendered.contains("# Subagents"));
        assert!(rendered.contains("explore"));
        assert!(rendered.contains("general-purpose"));
        assert!(rendered.contains("plan"));
        assert!(rendered.contains("code-reviewer"));
    }

    #[test]
    fn test_append_to_builder_injects_section() {
        let reg = SubagentRegistry::builtin();
        let builder = append_to_builder(SystemPromptBuilder::new(), &reg);
        let rendered = builder.build().render();
        assert!(rendered.contains("# Subagents"));
        assert!(rendered.contains("explore"));
    }

    #[test]
    fn test_append_to_builder_skips_when_empty() {
        let reg = SubagentRegistry::empty();
        let rendered_with = append_to_builder(SystemPromptBuilder::new(), &reg)
            .build()
            .render();
        let rendered_without = SystemPromptBuilder::new().build().render();
        assert_eq!(rendered_with, rendered_without);
    }
}
