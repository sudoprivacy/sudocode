//! Subagent dispatch — validation stub.
//!
//! Spawning a real child runtime (its own Tokio task, model client, and
//! tool-surface enforcement) is deferred to a follow-up PR. For now,
//! `dispatch` validates the request against the registry and reports
//! whether the subagent *would* be spawned.

use super::registry::SubagentRegistry;

/// Request to dispatch work to a subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRequest {
    /// Name of a registered [`super::SubagentType`].
    pub subagent_type: String,
    /// Short (≤5 words) human-readable description used in UI and logs.
    pub description: String,
    /// Full task brief handed to the subagent.
    pub prompt: String,
}

/// Outcome of a dispatch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchStatus {
    /// Request passed registry validation — a real runtime would spawn here.
    Validated,
    /// Request rejected before any work was attempted (e.g. unknown type).
    Rejected(String),
    /// Subagent ran and produced a result. Reserved for the post-stub
    /// implementation; the current dispatcher never returns this variant.
    Completed,
    /// Subagent began running and failed. Reserved for the post-stub
    /// implementation; the current dispatcher never returns this variant.
    Failed(String),
}

/// Structured response from [`SubagentRegistry::dispatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentResponse {
    pub status: DispatchStatus,
    /// Populated when `status == Completed`. Always `None` from the stub.
    pub result: Option<String>,
    /// Human-readable message describing what happened.
    pub message: String,
}

impl SubagentRegistry {
    /// Validate `req` against the registry. Real execution is deferred —
    /// see the module docs for scope.
    // TODO(post-PR): wire up actual sub-runtime execution (separate Tokio
    // task, separate model client, separate tool surface enforcement).
    #[must_use]
    pub fn dispatch(&self, req: SubagentRequest) -> SubagentResponse {
        let Some(sa) = self.get(&req.subagent_type) else {
            let message = format!("unknown subagent type: {}", req.subagent_type);
            return SubagentResponse {
                status: DispatchStatus::Rejected(message.clone()),
                result: None,
                message,
            };
        };

        let tool_summary = match &sa.tools_allowed {
            super::registry::ToolFilter::All => "all tools allowed".to_string(),
            super::registry::ToolFilter::Allow(list) => {
                format!("allowed tools: {}", list.join(", "))
            }
            super::registry::ToolFilter::Deny(list) => format!("denied tools: {}", list.join(", ")),
        };

        SubagentResponse {
            status: DispatchStatus::Validated,
            result: None,
            message: format!(
                "validated dispatch to `{}` ({}); {}",
                sa.name, req.description, tool_summary
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::registry::{SubagentRegistry, SubagentType, ToolFilter};
    use super::*;

    fn req(kind: &str) -> SubagentRequest {
        SubagentRequest {
            subagent_type: kind.into(),
            description: "find auth code".into(),
            prompt: "Locate the authentication entry point.".into(),
        }
    }

    #[test]
    fn dispatch_unknown_is_rejected() {
        let reg = SubagentRegistry::builtin();
        let resp = reg.dispatch(req("does-not-exist"));
        assert!(matches!(resp.status, DispatchStatus::Rejected(_)));
        assert!(resp.result.is_none());
    }

    #[test]
    fn dispatch_known_is_validated() {
        let reg = SubagentRegistry::builtin();
        let resp = reg.dispatch(req("explore"));
        assert_eq!(resp.status, DispatchStatus::Validated);
        assert!(resp.message.contains("explore"));
        assert!(resp.result.is_none());
    }

    #[test]
    fn dispatch_message_reflects_tool_filter() {
        let mut reg = SubagentRegistry::empty();
        reg.register(SubagentType::new(
            "narrow",
            "narrow agent",
            ToolFilter::Allow(vec!["Read".into()]),
        ));
        let resp = reg.dispatch(req("narrow"));
        assert!(resp.message.contains("Read"));
    }
}
