//! Bounded metadata from the final client request; no prompt or credential data.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRequestObservation {
    pub schema_version: u32,
    pub request_started_ms: u64,
    pub response_started_ms: u64,
    pub response_completed_ms: u64,
    pub attempts: u32,
    pub message_starts: u32,
    pub completed: bool,
    pub context_segment: u64,
    pub request_kind: String,
    pub route_key: String,
    pub settings_key: String,
    pub requested_ttls_seconds: Vec<Option<u32>>,
    pub upstream_ttl_seconds: Option<u32>,
    pub client_prefix_compatible: Option<bool>,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[must_use]
pub fn local_key(value: &str) -> String {
    let mut hash = DefaultHasher::new();
    value.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

impl CacheRequestObservation {
    pub(crate) fn from_body(body: &Value, route_key: String, segment: u64) -> Self {
        let mut ttls = Vec::new();
        let mut marker = |value: &Value| {
            if let Some(control) = value.get("cache_control") {
                ttls.push(match (control["type"].as_str(), control.get("ttl")) {
                    (Some("ephemeral"), None) => Some(300),
                    (Some("ephemeral"), Some(ttl)) if ttl == "5m" => Some(300),
                    (Some("ephemeral"), Some(ttl)) if ttl == "1h" => Some(3600),
                    _ => None,
                });
            }
        };
        marker(body);
        for key in ["tools", "system"] {
            if let Some(blocks) = body[key].as_array() {
                for block in blocks {
                    marker(block);
                }
            }
        }
        if let Some(messages) = body["messages"].as_array() {
            for message in messages {
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        marker(block);
                    }
                }
            }
        }
        // Small request settings only. Prompt fingerprints are reused from the
        // existing ledger, rather than serializing the body a second time.
        let settings = serde_json::json!({
            "thinking": body.get("thinking"), "output_config": body.get("output_config"),
            "tool_choice": body.get("tool_choice"), "speed": body.get("speed"),
        });
        Self {
            schema_version: 1,
            request_started_ms: 0,
            response_started_ms: 0,
            response_completed_ms: 0,
            attempts: 0,
            message_starts: 0,
            completed: false,
            context_segment: segment,
            request_kind: if runtime::compaction_scope::is_active() {
                "compaction"
            } else {
                "session_request"
            }
            .to_owned(),
            route_key,
            settings_key: local_key(&settings.to_string()),
            requested_ttls_seconds: ttls,
            upstream_ttl_seconds: None,
            client_prefix_compatible: None,
        }
    }
}
