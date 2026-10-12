use super::required;
use crate::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};

pub(super) fn dispatch(client: &NexusVfsClient, method: &str, params: &Value) -> Result<Value> {
    match method {
        "sessions.call" => {
            let method = required(params, "method")?;
            if !matches!(
                method,
                "managed_agent.start_session_v1"
                    | "managed_agent.cancel_v1"
                    | "managed_agent.get_session_v1"
                    | "managed_agent.list_sessions_v1"
            ) {
                return Err("unsupported session capability".into());
            }
            let input = params.get("params").ok_or("missing session params")?;
            let bytes = client.call(method, &serde_json::to_vec(input)?, "")?;
            let body: Value = serde_json::from_slice(&bytes)?;
            Ok(body.get("result").cloned().unwrap_or(body))
        }
        "sessions.read" => {
            let offset = required(params, "offset")?.parse::<u64>()?;
            let blocking = params
                .get("blocking")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let timeout_ms = params
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(1000)
                .min(10_000);
            let result = client.stream_read_at(
                required(params, "path")?,
                offset,
                blocking,
                timeout_ms,
                "",
            )?;
            Ok(
                json!({"data": STANDARD.encode(result.data), "nextOffset": result.next_offset.to_string(), "eof": result.eof, "timedOut": result.timed_out}),
            )
        }
        "sessions.write" => {
            let data = STANDARD.decode(required(params, "data")?)?;
            let offset = client.stream_write(required(params, "path")?, data, "")?;
            Ok(json!(offset.to_string()))
        }
        _ => Err(format!("unknown service capability: {method}").into()),
    }
}
