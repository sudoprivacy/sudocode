//! Wire adaptation of the shared engine interaction contract.
use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use agent_client_protocol_schema::{AgentNotification, ExtNotification};
use engine_core::engine_events::{InteractionAction, InteractionSnapshot};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/session/interactions", response = InteractionResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InteractionRequest {
    pub session_id: String,
    #[serde(flatten)]
    pub action: InteractionAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
#[serde(transparent)]
pub(crate) struct InteractionResponse(pub Value);

pub(crate) fn notification(session_id: &str, snapshot: &InteractionSnapshot) -> AgentNotification {
    let payload = serde_json::value::to_raw_value(&json!({
        "sessionId": session_id, "snapshot": snapshot,
    }))
    .expect("interaction projection is serializable");
    AgentNotification::ExtNotification(ExtNotification::new(
        "_sudocode/session/interactions_update",
        payload.into(),
    ))
}

pub(crate) fn error_notification(session_id: &str, message: &str) -> AgentNotification {
    let payload = serde_json::value::to_raw_value(&json!({
        "sessionId": session_id, "error": message,
    }))
    .expect("interaction error is serializable");
    AgentNotification::ExtNotification(ExtNotification::new(
        "_sudocode/session/interactions_update",
        payload.into(),
    ))
}
