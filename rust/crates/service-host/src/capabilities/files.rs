use super::required;
use crate::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};

pub(super) fn dispatch(client: &NexusVfsClient, method: &str, params: &Value) -> Result<Value> {
    match method {
        "files.read" => Ok(json!(
            STANDARD.encode(client.read(required(params, "path")?, "")?)
        )),
        "files.exists" => match client.stat(required(params, "path")?, "") {
            Ok(_) => Ok(json!(true)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!(false)),
            Err(error) => Err(error.into()),
        },
        "files.write" => {
            let data = STANDARD.decode(required(params, "data")?)?;
            client.write(required(params, "path")?, data, "")?;
            Ok(Value::Null)
        }
        "files.link" => Ok(json!(client.ensure_link(
            required(params, "path")?,
            required(params, "target")?,
            ""
        )?)),
        "files.stat" => {
            let entry = client.stat(required(params, "path")?, "")?;
            Ok(
                json!({"path": required(params, "path")?, "entryType": entry.entry_type,
                "isDirectory": entry.is_directory, "size": entry.size, "zone_id": entry.zone_id}),
            )
        }
        "files.list" => {
            let entries = client.readdir(required(params, "path")?, "")?;
            Ok(json!(entries
                .into_iter()
                .map(|entry| json!({"name": entry.name, "entryType": entry.entry_type}))
                .collect::<Vec<_>>()))
        }
        "files.delete" => {
            client.delete(required(params, "path")?, "")?;
            Ok(json!(true))
        }
        "files.mkdir" => {
            let path = required(params, "path")?;
            if params
                .get("parents")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                let mut parent = String::new();
                for part in path.split('/').filter(|part| !part.is_empty()) {
                    parent.push('/');
                    parent.push_str(part);
                    client.ensure_dir(&parent, "")?;
                }
            } else {
                client.ensure_dir(path, "")?;
            }
            Ok(Value::Null)
        }
        _ => Err(format!("unknown service capability: {method}").into()),
    }
}
