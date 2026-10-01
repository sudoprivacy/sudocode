//! Provider HTTP carried by prompt/reply files under a Nexus model mount.
//! The provider codecs still own the request JSON and SSE decoding.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use runtime::FsBackend;
use serde_json::{json, Value};

use crate::ApiError;

/// Session-owned filesystem and model egress policy. Clones preserve identity.
#[derive(Clone)]
pub struct ModelAccess {
    pub fs: Arc<dyn FsBackend>,
    pub require_mount: bool,
}

#[derive(Clone)]
pub(crate) struct NexusTransport {
    fs: Arc<dyn FsBackend>,
    mount: String,
}

impl std::fmt::Debug for NexusTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NexusTransport")
            .field("mount", &self.mount)
            .finish_non_exhaustive()
    }
}

impl NexusTransport {
    pub(crate) fn new(url: &str, fs: Arc<dyn FsBackend>) -> Result<Self, ApiError> {
        let mount = url
            .strip_prefix("nexus://")
            .ok_or_else(|| configuration("expected nexus:///absolute/mount"))?;
        if !mount.starts_with('/')
            || mount.starts_with("//")
            || mount.contains(['?', '#', '\\'])
            || mount.split('/').any(|s| matches!(s, "." | ".."))
            || mount.trim_matches('/').is_empty()
        {
            return Err(configuration(
                "model baseUrl must be nexus:///absolute/mount without query or traversal",
            ));
        }
        Ok(Self {
            fs,
            mount: mount.trim_end_matches('/').to_string(),
        })
    }

    pub(crate) async fn send(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &Value,
        request_id: &str,
    ) -> Result<reqwest::Response, ApiError> {
        // Provider clients use this synthetic origin only to build endpoint paths.
        let path = url
            .strip_prefix("http://nexus.invalid/")
            .ok_or_else(|| configuration("model request escaped the Nexus provider endpoint"))?;
        let forwarded: std::collections::BTreeMap<_, _> = headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                matches!(name.as_str(), "anthropic-version" | "anthropic-beta")
                    .then(|| (name, value.clone()))
            })
            .chain(std::iter::once((
                "x-request-id".to_string(),
                request_id.to_string(),
            )))
            .collect();
        let request = serde_json::to_vec(&json!({"nexus_http": {"version": 1, "path": path, "headers": forwarded}, "body": body}))
            .map_err(|e| configuration(e.to_string()))?;
        let stem = format!("{}/{}", self.mount, uuid::Uuid::new_v4());
        let prompt = format!("{stem}.prompt");
        let reply = format!("{stem}.reply");
        let fs = Arc::clone(&self.fs);
        tokio::task::spawn_blocking(move || fs.write(&prompt, &request))
            .await
            .map_err(|e| configuration(e.to_string()))?
            .map_err(|e| configuration(format!("Nexus model write: {e}")))?;
        let mut reader = ReplyReader {
            fs: Arc::clone(&self.fs),
            path: reply,
            cursor: 0,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        let (head, next) = read_record(reader)
            .await
            .map_err(|e| configuration(e.to_string()))?;
        reader = next;
        let head: Value = serde_json::from_slice(&head)
            .map_err(|e| configuration(format!("invalid Nexus response header: {e}")))?;
        if head["type"] != "response" || head["version"] != 1 {
            return Err(configuration(format!(
                "Nexus model: {}",
                head.get("message").unwrap_or(&head)
            )));
        }
        let status = head["status"]
            .as_u64()
            .and_then(|s| u16::try_from(s).ok())
            .ok_or_else(|| configuration("missing Nexus response status"))?;
        let mut response = http::Response::builder().status(status);
        if let Some(headers) = head["headers"].as_object() {
            for (name, value) in headers {
                if let Some(value) = value.as_str() {
                    response = response.header(name, value);
                }
            }
        }
        reader.deadline = Instant::now() + Duration::from_secs(300);
        let stream = futures_util::stream::try_unfold(reader, |reader| async move {
            let (record, mut reader) = read_record(reader).await?;
            reader.deadline = Instant::now() + Duration::from_secs(300);
            if record.first() == Some(&0) {
                return Ok(Some((record[1..].to_vec(), reader)));
            }
            let control: Value = serde_json::from_slice(&record).map_err(io::Error::other)?;
            match control["type"].as_str() {
                Some("done") => Ok(None),
                Some("error") => Err(io::Error::other(format!(
                    "Nexus model: {}",
                    control["message"]
                ))),
                _ => Err(io::Error::other("unexpected Nexus model reply record")),
            }
        });
        response
            .body(reqwest::Body::wrap_stream(stream))
            .map(reqwest::Response::from)
            .map_err(|e| configuration(e.to_string()))
    }
}

struct ReplyReader {
    fs: Arc<dyn FsBackend>,
    path: String,
    cursor: u64,
    deadline: Instant,
}

async fn read_record(reader: ReplyReader) -> io::Result<(Vec<u8>, ReplyReader)> {
    let mut reader = reader;
    loop {
        if Instant::now() >= reader.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Nexus model reply timed out",
            ));
        }
        let (result, returned) = tokio::task::spawn_blocking(move || {
            let result = reader.fs.tail_read(&reader.path, reader.cursor, 250);
            (result, reader)
        })
        .await
        .map_err(io::Error::other)?;
        reader = returned;
        match result {
            Ok((bytes, next, _)) if !bytes.is_empty() => {
                if next <= reader.cursor {
                    return Err(io::Error::other("Nexus model stream did not advance"));
                }
                reader.cursor = next;
                return Ok((bytes, reader));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound && reader.cursor == 0 => {}
            Err(e) => return Err(e),
        }
        // The reply is created asynchronously. Empty reads can also be a
        // timeout: only the explicit terminal record declares completion.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn configuration(message: impl Into<String>) -> ApiError {
    ApiError::Configuration(message.into())
}
