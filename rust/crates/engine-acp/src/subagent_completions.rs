//! ACP request/response schemas for the engine completion contract.
pub(crate) use crate::session_driver::SessionDriver as Delivery;
use agent_client_protocol::{JsonRpcRequest, JsonRpcResponse};
use engine_core::engine_events::ContextRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TASK_META_KEY: &str = "taskCompletions";

pub const META_KEY: &str = "subagentCompletions";

pub fn negotiated(meta: Option<&agent_client_protocol_schema::Meta>, key: &str) -> bool {
    meta.and_then(|m| m.get("sudocode"))
        .and_then(|s| s.get(key))
        .and_then(|c| c.get("version"))
        .and_then(Value::as_u64)
        .is_some_and(|v| v == 1)
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_sudocode/subagent/replay", response = ReplayResponse)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplayRequest {
    pub session_id: String,
    pub cwd: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResponse {
    pub watermark: u64,
    pub waiting: usize,
    pub pending: usize,
    pub unresolved: usize,
    pub settled: usize,
    pub blocked: Vec<String>,
    pub context_ref: Option<ContextRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub struct TaskResponse {
    #[serde(flatten)]
    pub fields: serde_json::Map<String, Value>,
}

macro_rules! task_request {
    ($name:ident, $method:tt $(, $field:ident: $ty:ty)*) => {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
        #[request(method = $method, response = TaskResponse)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct $name {
            pub session_id: String,
            pub cwd: std::path::PathBuf,
            pub storage_ref: Option<String>,
            $(pub $field: $ty,)*
        }
    };
}
task_request!(TaskReplayRequest, "_sudocode/task/replay", after_task_id: Option<String>);
task_request!(TaskSuppressRequest, "_sudocode/task/suppress", origin_trace_id: String);
task_request!(TaskPurgeRequest, "_sudocode/task/purge", cleanup_id: String);

pub enum TaskOperation {
    Replay(Option<String>),
    Suppress(String),
    Purge(String),
}
