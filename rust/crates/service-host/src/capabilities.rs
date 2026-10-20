use crate::Result;
use nexus_vfs_client::NexusVfsClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod files;
mod mounts;
mod sessions;
mod vault;

/// The process channel carries capability calls, never kernel credentials.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "nullable_result")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

fn nullable_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Error {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn from_result(id: Value, result: Result<Value>) -> Self {
        match result {
            Ok(value) => Self {
                id,
                result: Some(value),
                error: None,
            },
            Err(error) => Self {
                id,
                result: None,
                error: Some(Error {
                    code: -32000,
                    message: error.to_string(),
                }),
            },
        }
    }
}

pub(super) fn required<'a>(params: &'a Value, field: &str) -> Result<&'a str> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing string field: {field}").into())
}

pub(crate) fn dispatch(client: &NexusVfsClient, method: &str, params: &Value) -> Result<Value> {
    match method.split_once('.').map(|(domain, _)| domain) {
        Some("vault") => vault::dispatch(client, method, params),
        Some("files") => files::dispatch(client, method, params),
        Some("sessions") => sessions::dispatch(client, method, params),
        Some("mounts") => mounts::dispatch(client, method, params),
        _ => Err(format!("unknown service capability: {method}").into()),
    }
}
