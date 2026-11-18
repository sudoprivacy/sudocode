//! Wire serialization of the engine's outstanding-request projection.
use agent_client_protocol_schema::{AgentNotification, ExtNotification};
use engine_core::engine_events::AttentionState;
use serde_json::json;

pub(crate) const METHOD: &str = "_sudocode/session/attention";

pub(crate) fn notification(session_id: &str, state: &AttentionState) -> AgentNotification {
    let payload =
        serde_json::value::to_raw_value(&json!({"sessionId": session_id, "update": state}))
            .expect("attention projection is serializable");
    AgentNotification::ExtNotification(ExtNotification::new(
        "_sudocode/session/attention_update",
        payload.into(),
    ))
}
