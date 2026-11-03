//! Test controller for live daemon workflows. Approvals are explicit fixture
//! decisions; production has no automatic approval policy in its transport.

use a2a::session::{SessionCodec, SessionEndpoint, SessionPayload, SessionSide};
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct Controller {
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<(), String>>>,
    client: Arc<NexusVfsClient>,
    pid: Value,
    auth: String,
    requests: mpsc::Sender<ControlRequest>,
    session_id: String,
    notifications: Mutex<mpsc::Receiver<Value>>,
}

struct ControlRequest {
    method: String,
    params: Value,
    reply: Option<mpsc::SyncSender<Value>>,
}

fn open_session_request(cwd: &str, resume: Option<&str>) -> Value {
    let (method, params) = match resume {
        Some(id) => (
            "session/load",
            json!({"sessionId":id,"cwd":cwd,"mcpServers":[]}),
        ),
        None => ("session/new", json!({"cwd":cwd,"mcpServers":[]})),
    };
    json!({"jsonrpc":"2.0","id":"test-open","method":method,"params":params})
}

fn permission_reply(message: &Value) -> Result<Value, String> {
    let option = message["params"]["options"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["kind"] == "allow_once"))
        .ok_or("fixture requires an allow_once choice")?;
    Ok(
        json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{
        "outcome":"selected","optionId":option["optionId"]}}}),
    )
}

impl Controller {
    pub fn attach(client: &Arc<NexusVfsClient>, started: &Value, auth: &str) -> Self {
        Self::attach_session(client, started, auth, "/", None)
    }

    pub fn attach_session(
        client: &Arc<NexusVfsClient>,
        started: &Value,
        auth: &str,
        cwd: &str,
        resume: Option<&str>,
    ) -> Self {
        let endpoint: SessionEndpoint =
            serde_json::from_value(started["session_endpoint"].clone()).unwrap();
        let mut codec = SessionCodec::new(endpoint.clone(), SessionSide::Controller).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let connection = Arc::clone(client);
        let token = auth.to_string();
        let cwd = cwd.to_string();
        let resume = resume.map(str::to_owned);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (requests, incoming) = mpsc::channel::<ControlRequest>();
        let (notifications, received_notifications) = mpsc::channel();
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
            let mut pending = BTreeMap::new();
            let mut next_request = 0u64;
            while !stop.load(Ordering::Acquire) {
                while let Ok(request) = incoming.try_recv() {
                    let mut message =
                        json!({"jsonrpc":"2.0","method":request.method,"params":request.params});
                    if let Some(reply) = request.reply {
                        let id = format!("fixture-request-{next_request}");
                        next_request += 1;
                        message["id"] = json!(id);
                        pending.insert(id, reply);
                    }
                    send(&mut codec, message)?;
                }
                let result = connection
                    .stream_read_at(&endpoint.transcript, offset, true, 200, &token)
                    .map_err(|e| e.to_string())?;
                let bytes = result.data;
                offset = result.next_offset;
                if bytes.is_empty() {
                    continue;
                }
                match codec.decode(&bytes)? {
                    Some(SessionPayload::Rpc { message }) => {
                        if let Some(reply) =
                            message["id"].as_str().and_then(|id| pending.remove(id))
                        {
                            let _ = reply.send(message);
                            continue;
                        }
                        if message.get("error").is_some() {
                            return Err(message.to_string());
                        }
                        if message["id"] == "test-init" {
                            send(&mut codec, open_session_request(&cwd, resume.as_deref()))?;
                        } else if message["id"] == "test-open" {
                            if let Some(tx) = ready_tx.take() {
                                // ACP load returns no ID; the requested durable ID is authoritative.
                                let id = resume
                                    .clone()
                                    .or_else(|| {
                                        message["result"]["sessionId"].as_str().map(str::to_owned)
                                    })
                                    .ok_or("session/new did not return a sessionId")?;
                                let _ = tx.send(id);
                            }
                        } else if message["method"] == "session/request_permission" {
                            send(&mut codec, permission_reply(&message)?)?;
                        } else if message.get("method").is_some() && message.get("id").is_some() {
                            return Err(format!("unexpected client request: {message}"));
                        } else if message.get("method").is_some() {
                            let _ = notifications.send(message);
                        }
                    }
                    Some(SessionPayload::Closed { .. }) => break,
                    None => {}
                }
            }
            Ok(())
        });
        let mut controller = Self {
            stopped,
            worker: Some(worker),
            client: Arc::clone(client),
            pid: started["session_id"].clone(),
            auth: auth.to_string(),
            requests,
            session_id: String::new(),
            notifications: Mutex::new(received_notifications),
        };
        controller.session_id = ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("controller initialize/session-open handshake");
        controller
    }

    pub fn request(&self, method: &str, params: Value) -> Value {
        self.request_async(method, params)
            .recv_timeout(Duration::from_secs(30))
            .expect("controller RPC response")
    }

    pub fn request_async(&self, method: &str, params: Value) -> mpsc::Receiver<Value> {
        let (reply, response) = mpsc::sync_channel(1);
        self.requests
            .send(ControlRequest {
                method: method.into(),
                params,
                reply: Some(reply),
            })
            .expect("controller request channel");
        response
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.requests
            .send(ControlRequest {
                method: method.into(),
                params,
                reply: None,
            })
            .expect("controller notification channel");
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn drain_notifications(&self) -> Vec<Value> {
        self.notifications.lock().unwrap().try_iter().collect()
    }

    pub fn wait_closed(mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !self.worker.as_ref().unwrap().is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "managed runtime did not close its transport"
            );
            thread::sleep(Duration::from_millis(20));
        }
        self.worker
            .take()
            .unwrap()
            .join()
            .expect("controller thread")
            .expect("controller channel");
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
