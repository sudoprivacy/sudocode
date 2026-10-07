//! Test controller for live daemon workflows. Approvals are explicit fixture
//! decisions; production has no automatic approval policy in its transport.

use a2a::session::{SessionCodec, SessionEndpoint, SessionPayload, SessionSide};
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct Controller {
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
    client: Arc<NexusVfsClient>,
    pid: Value,
    auth: String,
}

impl Controller {
    pub fn attach(client: &Arc<NexusVfsClient>, started: &Value, auth: &str) -> Self {
        let endpoint: SessionEndpoint =
            serde_json::from_value(started["session_endpoint"].clone()).unwrap();
        let mut codec = SessionCodec::new(endpoint.clone(), SessionSide::Controller).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let connection = Arc::clone(client);
        let token = auth.to_string();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || -> Result<(), String> {
            let send = |codec: &mut SessionCodec, message| -> Result<(), String> {
                let bytes = codec.encode(SessionPayload::Rpc { message })?;
                connection
                    .stream_write(&endpoint.transcript, bytes, &token)
                    .map_err(|e| e.to_string())?;
                Ok(())
            };
            send(
                &mut codec,
                json!({"jsonrpc":"2.0","id":"test-init","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
            )?;
            let mut offset = 0;
            let mut ready_tx = Some(ready_tx);
            while !stop.load(Ordering::Acquire) {
                let (bytes, next, _) = connection
                    .stream_read_at(&endpoint.transcript, offset, true, 200, &token)
                    .map_err(|e| e.to_string())?;
                offset = next;
                if bytes.is_empty() {
                    continue;
                }
                match codec.decode(&bytes)? {
                    Some(SessionPayload::Rpc { message }) => {
                        if message.get("error").is_some() {
                            return Err(message.to_string());
                        }
                        if message["id"] == "test-init" {
                            send(
                                &mut codec,
                                json!({"jsonrpc":"2.0","id":"test-open","method":"session/new","params":{"cwd":"/","mcpServers":[]}}),
                            )?;
                        } else if message["id"] == "test-open" {
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(());
                            }
                        } else if message["method"] == "session/request_permission" {
                            let option = message["params"]["options"]
                                .as_array()
                                .and_then(|options| {
                                    options.iter().find(|option| option["kind"] == "allow_once")
                                })
                                .ok_or("fixture requires an allow_once choice")?;
                            send(
                                &mut codec,
                                json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":option["optionId"]}}}),
                            )?;
                        } else if message.get("method").is_some() && message.get("id").is_some() {
                            return Err(format!("unexpected client request: {message}"));
                        }
                    }
                    Some(SessionPayload::Closed { .. }) => break,
                    None => {}
                }
            }
            Ok(())
        });
        let controller = Self {
            stopped,
            worker: Some(worker),
            client: Arc::clone(client),
            pid: started["session_id"].clone(),
            auth: auth.to_string(),
        };
        ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("controller initialize/session-new handshake");
        controller
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let _ = self.client.call(
            "managed_agent.cancel_v1",
            json!({"session_id":self.pid,"mode":"session"})
                .to_string()
                .as_bytes(),
            &self.auth,
        );
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result
                    .expect("controller thread")
                    .expect("controller channel");
            }
        }
    }
}
