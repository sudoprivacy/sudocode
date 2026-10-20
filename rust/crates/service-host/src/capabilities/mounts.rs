use super::required;
use crate::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};

pub(super) fn dispatch(client: &NexusVfsClient, method: &str, params: &Value) -> Result<Value> {
    match method {
        "mounts.call" => {
            let method = required(params, "method")?;
            if !method.starts_with("fuse.") {
                return Err("unsupported mount capability".into());
            }
            if method == "fuse.status" && !crate::artifacts::fuse_available() {
                client.stat("/", "")?;
                return Ok(json!(STANDARD.encode(b"fuse-t-missing")));
            }
            let data = STANDARD.decode(required(params, "data")?)?;
            Ok(json!(STANDARD.encode(client.call(method, &data, "")?)))
        }
        _ => Err(format!("unknown service capability: {method}").into()),
    }
}
