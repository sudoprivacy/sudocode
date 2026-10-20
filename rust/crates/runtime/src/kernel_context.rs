//! Operation contexts for in-process Nexus callers.

use kernel::kernel::OperationContext;

/// Attribute work to its owner while authorizing the acting agent.
/// An owner identity carries attribution, not inherited user grants.
#[must_use]
pub fn agent_operation_context(owner: &str, zone: &str, agent: &str) -> OperationContext {
    let mut ctx = OperationContext::new(owner, zone, false, Some(agent), false);
    ctx.subject_type = "agent".to_string();
    ctx.subject_id = Some(agent.to_string());
    ctx
}
