//! Request-derived attention shared by terminal and hosted consumers.
use crate::runtime_status::InputReason;
use crate::SubagentIdentity;

/// Unique within one live engine session, including cancelled turns.
pub type RequestId = u64;

/// Queue ownership is distinct from permission to answer the visible request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    Queued,
    Presented,
}

/// A bounded view of a real outstanding request. Details still travel through
/// the existing permission/question event and its correlated answer command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionRequest {
    pub id: RequestId,
    pub reason: InputReason,
    pub phase: RequestPhase,
    /// None means the current session, not an unidentified remote peer.
    pub agent: Option<SubagentIdentity>,
    pub summary: String,
}

/// Session-local ordered projection, not a durable request or control grant.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionState {
    pub revision: u64,
    pub requests: Vec<AttentionRequest>,
}

/// A host-bound directory read. It cannot start a turn, send a message or
/// acquire control authority. Hosts retain their own discovery deadlines.
pub type AgentDirectoryReader =
    std::sync::Arc<dyn Fn() -> Result<serde_json::Value, String> + Send + Sync>;
