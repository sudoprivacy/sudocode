//! Test controller for the production mailbox driver. Tool approvals are explicit
//! test inputs; production never installs this controller.
#![allow(dead_code)]
use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use kernel::kernel::{Kernel, OperationContext};
use managed_agent::{SpawnHandle, SpawnOptions, SpawnTask};
use runtime::spawn_task::{AgentDescriptor, AgentState};
use runtime::HookAbortSignal;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn send(mailbox: &SessionMailbox<Kernel>, message: Value) -> Result<(), String> {
    mailbox.send(SessionPayload::Rpc { message })
}
fn response(mailbox: &SessionMailbox<Kernel>, id: &str) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match mailbox.receive(200)? {
            Some(SessionPayload::Rpc { message }) if message["id"] == id => {
                if let Some(error) = message.get("error") {
                    return Err(error.to_string());
                }
                return Ok(message["result"].clone());
            }
            Some(SessionPayload::Closed { reason }) => return Err(reason),
            _ => {}
        }
    }
    Err(format!("controller timed out waiting for {id}"))
}
fn connect(
    kernel: Arc<Kernel>,
    endpoint: SessionEndpoint,
    durable: Option<&str>,
) -> Result<SessionMailbox<Kernel>, String> {
    let ctx = OperationContext::new(
        "test-owner",
        "root",
        false,
        Some(&endpoint.controller),
        false,
    );
    let mailbox = SessionMailbox::open(kernel, ctx, endpoint, SessionSide::Controller)?;
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}),
    )?;
    response(&mailbox, "init")?;
    let (method, params) = match durable {
        Some(id) => (
            "session/load",
            json!({"sessionId":id,"cwd":"/","mcpServers":[]}),
        ),
        None => ("session/new", json!({"cwd":"/","mcpServers":[]})),
    };
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"open","method":method,"params":params}),
    )?;
    response(&mailbox, "open")?;
    Ok(mailbox)
}
fn pump(mailbox: SessionMailbox<Kernel>, stop: HookAbortSignal) {
    while !stop.is_aborted() {
        match mailbox.receive(200) {
            Ok(Some(SessionPayload::Rpc { message }))
                if message["method"] == "session/request_permission" =>
            {
                let option = message["params"]["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|option| option["kind"] == "allow_once")
                    .unwrap()["optionId"]
                    .clone();
                if send(&mailbox, json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":option}}})).is_err() { break; }
            }
            Ok(Some(SessionPayload::Closed { .. })) | Err(_) => break,
            _ => {}
        }
    }
}
pub struct TestAbort {
    stop: HookAbortSignal,
    host: Arc<dyn SpawnHandle>,
}
impl TestAbort {
    pub fn abort(&self) {
        self.stop.abort();
        self.host.abort();
    }
}
pub struct TestHost {
    pub abort_signal: TestAbort,
    pub join: std::thread::JoinHandle<()>,
}

pub fn spawn_managed_agent(
    kernel: Arc<Kernel>,
    desc: AgentDescriptor,
    observer: impl Fn(AgentState, Option<String>) + Send + Sync + 'static,
) -> Result<TestHost, String> {
    let endpoint = SessionEndpoint::new(
        desc.name.clone(),
        "test-controller".into(),
        format!("{}-{:?}", desc.pid, std::time::SystemTime::now()),
    );
    crate::common::provision_stream_transcript(&kernel, &endpoint.transcript);
    let host: Arc<dyn SpawnHandle> = Arc::from(
        engine_acp::managed_agent::SudoCodeSpawnAdapter.spawn_with_options(
            Arc::clone(&kernel),
            desc,
            SpawnOptions {
                session_endpoint: Some(endpoint.clone()),
                ..SpawnOptions::default()
            },
            Arc::new(observer),
        )?,
    );
    let mailbox = match connect(kernel, endpoint, None) {
        Ok(mailbox) => mailbox,
        Err(error) => {
            host.abort();
            return Err(error);
        }
    };
    let stop = HookAbortSignal::new();
    let thread_stop = stop.clone();
    let join = std::thread::spawn(move || pump(mailbox, thread_stop));
    Ok(TestHost {
        abort_signal: TestAbort { stop, host },
        join,
    })
}
pub struct Controller {
    stop: HookAbortSignal,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.abort();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
pub fn attach_controller(
    kernel: Arc<Kernel>,
    started: &Value,
    resume: bool,
) -> Result<Controller, String> {
    let endpoint =
        serde_json::from_value(started["session_endpoint"].clone()).map_err(|e| e.to_string())?;
    let mailbox = connect(
        kernel,
        endpoint,
        if resume {
            started["durable_session_id"].as_str()
        } else {
            None
        },
    )?;
    let stop = HookAbortSignal::new();
    let thread_stop = stop.clone();
    Ok(Controller {
        stop,
        join: Some(std::thread::spawn(move || pump(mailbox, thread_stop))),
    })
}
