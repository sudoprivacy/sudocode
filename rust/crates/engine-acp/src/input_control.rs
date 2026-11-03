//! Wire vocabulary for session-owned input. Scheduling remains in engine-core.
use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use agent_client_protocol_schema::{AgentNotification, ContentBlock, ExtNotification};
use engine_core::engine_events::{
    InputAction, InputDelivery, InputSource, PromptContext, TurnInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::AcpError;

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/session/input", response = InputResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InputRequest {
    pub session_id: String,
    #[serde(flatten)]
    pub action: WireAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum WireAction {
    Submit {
        #[serde(rename = "clientId")]
        client_id: String,
        #[serde(default)]
        display: Option<String>,
        #[serde(default)]
        draft: Option<Value>,
        content: Vec<ContentBlock>,
        #[serde(default)]
        delivery: InputDelivery,
    },
    Flush,
    FlushOrCancel,
    Recall,
    Snapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
#[serde(transparent)]
pub(crate) struct InputResponse(pub Value);

impl WireAction {
    pub(crate) fn into_engine(self) -> Result<InputAction, AcpError> {
        Ok(match self {
            Self::Submit {
                client_id,
                display,
                draft,
                content,
                delivery,
            } => {
                let (text, images) = crate::acp_sdk_server::extract_content_from_blocks(&content)?;
                let input =
                    TurnInput {
                        id: 0,
                        display: display.unwrap_or_else(|| text.clone()),
                        text,
                        source: InputSource::Human,
                        attachments: images
                            .into_iter()
                            .map(|(data, mime_type)| {
                                engine_core::engine_events::ContentBlock::Image { data, mime_type }
                            })
                            .collect(),
                    };
                let context = Some(PromptContext {
                    trace_id: client_id.clone(),
                    image_policy: engine_core::engine_events::ImageInputPolicy::DescribeUnsupported,
                    ..PromptContext::default()
                });
                InputAction::Submit {
                    client_id,
                    draft,
                    input,
                    delivery,
                    context: context.map(Box::new),
                }
            }
            Self::Flush => InputAction::Flush,
            Self::FlushOrCancel => InputAction::FlushOrCancel,
            Self::Recall => InputAction::Recall,
            Self::Snapshot => InputAction::Snapshot,
        })
    }
}

pub(crate) fn notification(session_id: &str, update: &Value) -> AgentNotification {
    let payload =
        serde_json::value::to_raw_value(&json!({ "sessionId": session_id, "update": update }))
            .expect("input projection is serializable");
    AgentNotification::ExtNotification(ExtNotification::new(
        "_sudocode/session/input_update",
        payload.into(),
    ))
}

pub(crate) fn usage(usage: runtime::TokenUsage) -> Value {
    json!({
        "totalTokens": usage.total_tokens(),
        "inputTokens": usage.input_tokens,
        "outputTokens": usage.output_tokens,
        "cachedReadTokens": usage.cache_read_input_tokens,
        "cachedWriteTokens": usage.cache_creation_input_tokens,
        "costUnits": usage.cost_units,
        "costCurrency": usage.cost_currency.map(runtime::UsageCostCurrency::as_str),
    })
}
