//! One reader routes stdio responses by JSON-RPC id. A cancelled waiter is
//! removed without taking another request's response or restarting the server.
use crate::{
    mcp_ndjson_transport,
    mcp_server_manager::{JsonRpcId, JsonRpcRequest, JsonRpcResponse},
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex},
};
use tokio::{
    io::BufReader,
    process::{ChildStdin, ChildStdout},
    sync::{oneshot, Mutex as AsyncMutex},
    task::AbortHandle,
};

type Reply = oneshot::Sender<io::Result<Value>>;
#[derive(Debug, Default)]
struct Replies {
    pending: HashMap<String, Reply>,
    closed: Option<(io::ErrorKind, String)>,
    initialize_id: Option<String>,
}

#[derive(Debug)]
pub(super) struct StdioRpc {
    writer: Arc<AsyncMutex<ChildStdin>>,
    replies: Arc<Mutex<Replies>>,
    reader: AbortHandle,
}

impl Drop for StdioRpc {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

struct Registration {
    replies: Arc<Mutex<Replies>>,
    key: String,
    writer: Arc<AsyncMutex<ChildStdin>>,
    id: JsonRpcId,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let pending = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(&self.key);
        if pending.is_some() {
            let writer = self.writer.clone();
            let notification = serde_json::json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":self.id, "reason":"request cancelled"}}).to_string();
            tokio::spawn(async move {
                let _ = mcp_ndjson_transport::write_msg(
                    &mut *writer.lock().await,
                    notification.as_bytes(),
                )
                .await;
            });
        }
    }
}

impl StdioRpc {
    pub(super) fn start(
        writer: Arc<AsyncMutex<ChildStdin>>,
        mut reader: BufReader<ChildStdout>,
    ) -> Self {
        let replies = Arc::new(Mutex::new(Replies::default()));
        let pending = replies.clone();
        let reader = tokio::spawn(async move {
            let error = loop {
                let bytes = match mcp_ndjson_transport::read_msg(&mut reader).await {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        break io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "MCP stdio stream closed",
                        )
                    }
                    Err(error) => break error,
                };
                let value: Value = match serde_json::from_slice(&bytes) {
                    Ok(value) => value,
                    Err(error) => break io::Error::new(io::ErrorKind::InvalidData, error),
                };
                let Some(id) = value.get("id").filter(|id| !id.is_null()) else {
                    // Notifications are not responses. Progress uses the same
                    // best-effort notification surface as other transports.
                    if value["method"] == "notifications/progress" {
                        if let Ok(progress) = serde_json::from_value(value["params"].clone()) {
                            crate::mcp_server_manager::emit_mcp_progress(progress);
                        }
                    }
                    continue;
                };
                let key = id.to_string();
                let reply = {
                    let mut replies = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if replies
                        .initialize_id
                        .as_ref()
                        .is_some_and(|expected| expected != &key)
                    {
                        break io::Error::new(
                            io::ErrorKind::InvalidData,
                            "MCP initialize response used mismatched id",
                        );
                    }
                    if replies.initialize_id.as_ref() == Some(&key) {
                        replies.initialize_id = None;
                    }
                    replies.pending.remove(&key)
                };
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(value));
                }
                // A timed-out/cancelled request can still reply; discard only
                // that id and keep the shared connection usable by siblings.
            };
            let mut pending = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.closed = Some((error.kind(), error.to_string()));
            for (_, reply) in pending.pending.drain() {
                let _ = reply.send(Err(io::Error::new(error.kind(), error.to_string())));
            }
        })
        .abort_handle();
        Self {
            writer,
            replies,
            reader,
        }
    }

    pub(super) async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        id: JsonRpcId,
        method: String,
        params: Option<P>,
    ) -> io::Result<JsonRpcResponse<R>> {
        let initializing = method == "initialize";
        let request = JsonRpcRequest::new(id.clone(), method, params);
        let bytes = serde_json::to_vec(&request).map_err(io::Error::other)?;
        let key = serde_json::to_string(&id).map_err(io::Error::other)?;
        let (tx, rx) = oneshot::channel();
        {
            let mut replies = self
                .replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((kind, message)) = &replies.closed {
                return Err(io::Error::new(*kind, message.clone()));
            }
            if replies.pending.contains_key(&key) {
                return Err(io::Error::other("duplicate MCP request id"));
            }
            if initializing {
                replies.initialize_id = Some(key.clone());
            }
            replies.pending.insert(key.clone(), tx);
        }
        let _registration = Registration {
            replies: self.replies.clone(),
            key,
            writer: self.writer.clone(),
            id: id.clone(),
        };
        // Finish a whole frame even when the requesting future is cancelled;
        // a half-written JSON line would corrupt every sibling's transport.
        // Acquire before spawning: cancellation must not overtake the frame
        // and reach the server before the request it is meant to cancel.
        let mut writer = self.writer.clone().lock_owned().await;
        tokio::spawn(async move { mcp_ndjson_transport::write_msg(&mut *writer, &bytes).await })
            .await
            .map_err(io::Error::other)??;
        let value = rx.await.map_err(|_| {
            io::Error::new(io::ErrorKind::BrokenPipe, "MCP response reader stopped")
        })??;
        let response: JsonRpcResponse<R> = serde_json::from_value(value)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if response.jsonrpc != "2.0" || response.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid MCP JSON-RPC response",
            ));
        }
        Ok(response)
    }
}
