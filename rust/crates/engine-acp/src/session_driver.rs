//! ACP wire adaptation over one persistent engine command/event handle.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol_schema::{ClientRequest, ExtRequest};
use engine_core::engine_events::{CompletionAction, CompletionReceipt};
use engine_core::{EngineAttachment, EngineCommand, EngineEvent, EngineHandle, EngineSession};
use engine_host::SessionEngine;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::acp_sdk_server::{HostedSessionFactory, SessionSubagents};
use crate::session_ops;

type ControlWaiter = std::sync::mpsc::Sender<Result<Value, String>>;

#[derive(Default)]
struct ConnectionState {
    connection: Option<ConnectionTo<Client>>,
    transport: Option<tokio::task::JoinHandle<()>>,
    keepalive: Option<Arc<SessionDriver>>,
    pending_configuration: Option<u64>,
    tasks: bool,
}

impl ConnectionState {
    fn abort_transport(&mut self) {
        if let Some(task) = self.transport.take() {
            task.abort();
        }
    }

    fn disconnect(&mut self) {
        self.abort_transport();
        self.connection = None;
        self.keepalive = None;
        self.pending_configuration = None;
    }
}

pub(crate) struct SessionDriver {
    commands: std::sync::mpsc::Sender<EngineCommand>,
    handle: Mutex<Option<EngineHandle>>,
    attachment: EngineAttachment,
    listener: Mutex<Option<UnboundedSender<EngineEvent>>>,
    connection: Mutex<ConnectionState>,
    subagents: Mutex<Option<Weak<SessionSubagents>>>,
    hosted_factory: Mutex<Option<Arc<dyn HostedSessionFactory>>>,
    requests: Mutex<HashMap<u64, ControlWaiter>>,
    next_request: AtomicU64,
    bridge_stopped: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl SessionDriver {
    pub(crate) fn completion_retirement(&self) -> engine_core::CompletionRetirement {
        self.attachment.completion_retirement()
    }

    fn new(
        (mut handle, attachment): (EngineHandle, EngineAttachment),
        session_id: String,
    ) -> Arc<Self> {
        let (_, unused) = std::sync::mpsc::channel();
        let events = std::mem::replace(&mut handle.events, unused);
        let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
        let driver = Arc::new(Self {
            commands: handle.commands.clone(),
            handle: Mutex::new(Some(handle)),
            attachment,
            listener: Mutex::new(None),
            connection: Mutex::new(ConnectionState::default()),
            subagents: Mutex::new(None),
            hosted_factory: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            bridge_stopped: Mutex::new(stopped_rx),
        });
        let (tx, mut rx) = unbounded_channel();
        std::thread::Builder::new()
            .name("acp-engine-events".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn ACP engine event bridge");
        let weak = Arc::downgrade(&driver);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let Some(driver) = weak.upgrade() else {
                    break;
                };
                driver.on_event(&session_id, event);
            }
            // No reply can follow: wake every request still waiting for one.
            if let Some(driver) = weak.upgrade() {
                driver
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
            }
            let _ = stopped_tx.send(());
        });
        driver
    }

    fn on_event(self: &Arc<Self>, session_id: &str, event: EngineEvent) {
        // Observe the engine's ordered lifecycle even without a prompt listener
        // (direct calls and idle background delivery share this same route).
        if let EngineEvent::State(state) = &event {
            let factory = self
                .hosted_factory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(factory) = factory {
                factory.state_changed(*state);
            }
        }
        match event {
            EngineEvent::CommandResult { id, result } => {
                {
                    let mut state = self
                        .connection
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if state.pending_configuration == Some(id) {
                        state.pending_configuration = None;
                        if result.is_err() {
                            state.disconnect();
                        }
                    }
                }
                if let Some(waiter) = self
                    .requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id)
                {
                    let _ = waiter.send(result);
                }
                return;
            }
            EngineEvent::CompletionRequest {
                attempt,
                tasks,
                payload,
            } => {
                self.deliver(attempt, tasks, payload);
                return;
            }
            EngineEvent::CompletionIdle => {
                let mut state = self
                    .connection
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Previous connection/protocol events precede the configure
                // command's result. They cannot retire the current binding.
                if state.pending_configuration.is_none() {
                    state.abort_transport();
                    state.keepalive = None;
                }
                return;
            }
            _ => {}
        }
        let route = self
            .subagents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        let turn_event = matches!(
            &event,
            EngineEvent::TextDelta { .. }
                | EngineEvent::State(_)
                | EngineEvent::PermissionRequest { .. }
                | EngineEvent::QuestionRequest { .. }
                | EngineEvent::ContextCommitted(_)
                | EngineEvent::TurnComplete(_)
                | EngineEvent::TurnFailed { .. }
                | EngineEvent::PromptRejected { .. }
                | EngineEvent::Error { .. }
        )
        .then(|| event.clone());
        if let Some(notification) =
            session_ops::engine_event_to_session_update(session_id, event, route.is_some())
        {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connection
                .clone();
            if let Some(connection) = connection {
                let _ = connection.send_notification(notification);
            }
        }
        if let Some(event) = turn_event {
            if let Some(listener) = self
                .listener
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                let _ = listener.send(event);
            }
        }
    }

    pub(crate) fn open(
        session: &runtime::Session,
        cwd: &Path,
        create: bool,
    ) -> Result<Arc<Self>, String> {
        Ok(Self::new(
            engine_host::session::completion_engine(session, cwd, create)?,
            session.session_id.clone(),
        ))
    }

    pub(crate) fn stored(session_id: &str, cwd: &Path) -> Result<Arc<Self>, String> {
        Ok(Self::new(
            engine_host::session::stored_completion_engine(session_id, cwd)?,
            session_id.into(),
        ))
    }

    pub(crate) fn start(
        engine: Arc<SessionEngine>,
        subagents: Option<&Arc<SessionSubagents>>,
        session_id: String,
        existing: Option<Arc<Self>>,
        hosted_factory: Option<Arc<dyn HostedSessionFactory>>,
    ) -> Result<Arc<Self>, String> {
        let driver = subagents
            .and_then(|route| route.completion_delivery())
            .or(existing)
            .unwrap_or_else(|| Self::new(EngineSession::deferred(Arc::default()), session_id));
        *driver
            .hosted_factory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hosted_factory;
        driver.attachment.attach(engine)?;
        *driver
            .subagents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = subagents.map(Arc::downgrade);
        Ok(driver)
    }

    pub(crate) fn attach_session(
        &self,
        session: &runtime::Session,
        cwd: &Path,
    ) -> Result<(), String> {
        self.attachment.attach_session(session, cwd)
    }

    pub(crate) fn control(&self, action: CompletionAction) -> Result<Value, String> {
        self.request(|id| EngineCommand::Completion {
            id: Some(id),
            action,
        })
    }

    fn request(&self, command: impl FnOnce(u64) -> EngineCommand) -> Result<Value, String> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        if let Err(error) = self.send(command(id)) {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(error);
        }
        let result = rx
            .recv_timeout(Duration::from_secs(30))
            .map_err(|e| e.to_string());
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        result?
    }

    pub(crate) async fn control_async(
        self: &Arc<Self>,
        action: CompletionAction,
    ) -> Result<Value, String> {
        let driver = Arc::clone(self);
        tokio::task::spawn_blocking(move || driver.control(action))
            .await
            .map_err(|e| e.to_string())?
    }

    pub(crate) async fn runtime_status(self: &Arc<Self>) -> Result<Value, String> {
        let driver = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            driver.request(|request_id| EngineCommand::RuntimeStatus { request_id })
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(crate) async fn cancel_subagent(
        self: &Arc<Self>,
        agent_id: String,
    ) -> Result<bool, String> {
        let driver = Arc::clone(self);
        let value = tokio::task::spawn_blocking(move || {
            driver.request(|request_id| EngineCommand::CancelSubagent {
                request_id,
                agent_id,
            })
        })
        .await
        .map_err(|e| e.to_string())??;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// Run one tool for the client outside any turn and wait for its reply for
    /// as long as it takes. The call can wait on a permission prompt, so a
    /// fixed timeout would abandon a call that may still run. Only a command
    /// the engine never received is a refusal; once sent, a lost reply leaves
    /// the outcome unknown.
    pub(crate) async fn direct_tool(
        self: &Arc<Self>,
        name: &str,
        input: String,
    ) -> Result<Value, engine_core::DirectToolError> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::channel();
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        let sent = self.send(EngineCommand::DirectTool {
            request_id: id,
            name: name.to_string(),
            input,
        });
        let result = match sent {
            Ok(()) => tokio::task::spawn_blocking(move || rx.recv())
                .await
                .map_err(|e| e.to_string())
                .and_then(|reply| {
                    reply.map_err(|_| "the engine session closed before replying".to_string())
                })
                .and_then(|reply| reply)
                .map_err(engine_core::DirectToolError::Unknown),
            Err(error) => Err(engine_core::DirectToolError::Refused(error)),
        };
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        result
    }

    /// Publish connection/protocol state and its causal event barrier together.
    fn configure_connection(&self, state: &mut ConnectionState) {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        state.abort_transport();
        state.pending_configuration = Some(id);
        if self
            .send(EngineCommand::Completion {
                id: Some(id),
                action: CompletionAction::Configure {
                    tasks: state.tasks,
                    connected: state.connection.is_some(),
                },
            })
            .is_err()
        {
            state.disconnect();
        }
    }

    pub(crate) fn set_task_protocol(&self, tasks: bool) {
        let mut state = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.tasks = tasks;
        self.configure_connection(&mut state);
    }

    pub(crate) fn bind(self: &Arc<Self>, connection: ConnectionTo<Client>) {
        let mut state = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.connection = Some(connection);
        state.keepalive = Some(Arc::clone(self));
        self.configure_connection(&mut state);
    }

    pub(crate) fn unbind(&self) {
        let mut state = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.disconnect();
        self.configure_connection(&mut state);
    }

    fn deliver(
        self: &Arc<Self>,
        attempt: u64,
        tasks: bool,
        payload: engine_core::engine_events::TaskCompletion,
    ) {
        let mut state = self
            .connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending_configuration.is_some() || state.connection.is_none() {
            return;
        }
        state.abort_transport();
        state.keepalive = Some(Arc::clone(self));
        let connection = state.connection.clone();
        let driver = Arc::clone(self);
        let task = tokio::spawn(async move {
            let receipt = match (
                connection,
                payload.wire_value(tasks).and_then(|value| {
                    serde_json::value::to_raw_value(&value).map_err(|e| e.to_string())
                }),
            ) {
                (Some(connection), Ok(raw)) => {
                    let result = connection
                        .send_request(ClientRequest::ExtMethodRequest(ExtRequest::new(
                            if tasks {
                                "_sudocode/task/completed"
                            } else {
                                "_sudocode/subagent/completed"
                            },
                            Arc::from(raw),
                        )))
                        .block_task()
                        .await;
                    match result {
                        Ok(value) => CompletionReceipt::Ack {
                            completion_id: value
                                .get("completionId")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            disposition: value
                                .get("disposition")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            reason: value
                                .get("reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        },
                        Err(error) => match error.code {
                            agent_client_protocol_schema::ErrorCode::InvalidParams
                            | agent_client_protocol_schema::ErrorCode::InvalidRequest => {
                                CompletionReceipt::ProtocolRejected
                            }
                            agent_client_protocol_schema::ErrorCode::MethodNotFound
                            | agent_client_protocol_schema::ErrorCode::AuthRequired => {
                                CompletionReceipt::Unavailable
                            }
                            _ => CompletionReceipt::Retry,
                        },
                    }
                }
                _ => CompletionReceipt::Retry,
            };
            let _ = driver.send(EngineCommand::Completion {
                id: None,
                action: CompletionAction::Receipt { attempt, receipt },
            });
        });
        state.transport = Some(task);
    }

    pub(crate) fn listen(
        self: &Arc<Self>,
        connection: ConnectionTo<Client>,
    ) -> (TurnListener, UnboundedReceiver<EngineEvent>) {
        // Ordinary turn traffic must not opt a client into durable delivery.
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .connection = Some(connection);
        let (tx, rx) = unbounded_channel();
        let mut listener = self
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            listener.is_none(),
            "session lane must serialize turn listeners"
        );
        *listener = Some(tx);
        (TurnListener(Arc::clone(self)), rx)
    }

    pub(crate) fn send(&self, command: EngineCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "engine session closed".into())
    }

    pub(crate) fn shutdown(&self) -> bool {
        self.unbind();
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let reaped = handle.is_none_or(|handle| handle.shutdown(Duration::from_secs(30)));
        self.attachment.release();
        let stopped = self
            .bridge_stopped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(30))
            .is_ok();
        reaped && stopped
    }
}

impl Drop for SessionDriver {
    fn drop(&mut self) {
        self.connection
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort_transport();
    }
}

pub(crate) struct TurnListener(Arc<SessionDriver>);
impl Drop for TurnListener {
    fn drop(&mut self) {
        self.0
            .listener
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}
