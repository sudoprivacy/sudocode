//! ACP wire adaptation over one persistent engine command/event handle.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol::role::acp::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol_schema::{ClientRequest, ExtRequest};
use engine_core::engine_events::{CompletionAction, CompletionReceipt};
use engine_core::{
    EngineAttachment, EngineCommand, EngineDelegate, EngineEvent, EngineHandle, EngineSession,
};
use engine_host::SessionEngine;
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::acp_sdk_server::SessionSubagents;
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
    requests: Mutex<HashMap<u64, ControlWaiter>>,
    next_request: AtomicU64,
    bridge_stopped: Mutex<std::sync::mpsc::Receiver<()>>,
    abort: Mutex<runtime::HookAbortSignal>,
    interactive: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    input_turn: AtomicU64,
    native_input: AtomicBool,
    input_state: tokio::sync::watch::Sender<Option<engine_core::engine_events::InputState>>,
    peer_events: tokio::sync::broadcast::Sender<EngineEvent>,
    hosted_factory: Mutex<Option<Weak<dyn crate::acp_sdk_server::HostedSessionFactory>>>,
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
            requests: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            bridge_stopped: Mutex::new(stopped_rx),
            abort: Mutex::new(runtime::HookAbortSignal::default()),
            interactive: Mutex::new(Vec::new()),
            input_turn: AtomicU64::new(0),
            native_input: AtomicBool::new(false),
            input_state: tokio::sync::watch::channel(None).0,
            peer_events: tokio::sync::broadcast::channel(256).0,
            hosted_factory: Mutex::new(None),
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
        if let EngineEvent::State(state) = &event {
            if let Some(factory) = self
                .hosted_factory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(Weak::upgrade)
            {
                factory.state_changed(*state);
            }
        }
        if self.peer_events.receiver_count() > 0 {
            let _ = self.peer_events.send(event.clone());
        }
        if self.answer_interaction(session_id, &event) {
            return;
        }
        if matches!(
            &event,
            EngineEvent::TurnComplete(_)
                | EngineEvent::TurnFailed { .. }
                | EngineEvent::PromptRejected { .. }
        ) {
            for task in self
                .interactive
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .drain(..)
            {
                task.abort();
            }
        }
        let input_update = self.input_update(&event);
        if let Some(update) = input_update {
            if let Some(connection) = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connection
                .clone()
            {
                let _ = connection
                    .send_notification(crate::input_control::notification(session_id, &update));
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
        self.route_event(session_id, event);
    }

    fn answer_interaction(self: &Arc<Self>, session_id: &str, event: &EngineEvent) -> bool {
        // Requests belong to the session, including automatically resumed queued turns.
        // A request/response listener must never own permission delivery.
        if matches!(
            &event,
            EngineEvent::PermissionRequest { .. } | EngineEvent::QuestionRequest { .. }
        ) {
            let connection = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .connection
                .clone();
            let abort = self
                .abort
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(connection) = connection {
                let answer = match &event {
                    EngineEvent::PermissionRequest { id, request } => {
                        crate::acp_sdk_server::answer_permission(
                            session_id, *id, request, self, abort, connection,
                        )
                    }
                    EngineEvent::QuestionRequest { id, request } => {
                        crate::acp_sdk_server::answer_question(
                            session_id, *id, request, self, abort, connection,
                        )
                    }
                    _ => unreachable!(),
                };
                let mut interactive = self
                    .interactive
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                interactive.retain(|task| !task.is_finished());
                interactive.push(tokio::spawn(answer));
            } else {
                match event {
                    EngineEvent::PermissionRequest { id, .. } => {
                        let _ = self.send(EngineCommand::PermissionAnswer {
                            id: *id,
                            decision: runtime::PermissionPromptDecision::Deny {
                                reason: "No connected client can approve this request".into(),
                            },
                        });
                    }
                    EngineEvent::QuestionRequest { id, .. } => {
                        let _ = self.send(EngineCommand::QuestionAnswer {
                            id: *id,
                            answers: Err("No connected client can answer this request".into()),
                        });
                    }
                    _ => unreachable!(),
                }
            }
            return true;
        }
        false
    }

    fn input_update(&self, event: &EngineEvent) -> Option<Value> {
        match event {
            EngineEvent::InputState(state) => {
                self.input_state.send_replace(Some(state.clone()));
                self.input_turn
                    .store(state.active_turn_id.unwrap_or(0), Ordering::Relaxed);
                Some(serde_json::json!({"kind":"state", "state":state}))
            }
            EngineEvent::InputCommitted(input) => {
                self.input_state.borrow().as_ref().and_then(|state| state.pending.iter().find(|pending| pending.id == input.id)).map(|pending| {
                    serde_json::json!({"kind":"committed", "clientId":pending.client_id, "display":pending.display, "source":pending.source})
                })
            }
            EngineEvent::TurnComplete(complete) if self.input_turn.load(Ordering::Relaxed) != 0 => {
                Some(serde_json::json!({
                    "kind":"turn_complete", "turnId":self.input_turn.load(Ordering::Relaxed),
                    "stopReason": if complete.cancelled { "cancelled" } else { "end_turn" },
                    "usage":crate::input_control::usage(complete.turn_usage),
                    "cumulativeUsage":crate::input_control::usage(complete.session_usage),
                }))
            }
            EngineEvent::TurnFailed { message, .. } | EngineEvent::PromptRejected { message }
                if self.input_turn.load(Ordering::Relaxed) != 0 =>
            {
                Some(serde_json::json!({"kind":"error", "message":message}))
            }
            _ => None,
        }
    }

    fn route_event(&self, session_id: &str, event: EngineEvent) {
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
    ) -> Result<Arc<Self>, String> {
        let driver = subagents
            .and_then(|route| route.completion_delivery())
            .or(existing)
            .unwrap_or_else(|| Self::new(EngineSession::deferred(Arc::default()), session_id));
        *driver
            .abort
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = engine.abort_signal();
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

    pub(crate) fn bind_hosted_factory(
        &self,
        factory: Option<&Arc<dyn crate::acp_sdk_server::HostedSessionFactory>>,
    ) {
        *self
            .hosted_factory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = factory.map(Arc::downgrade);
    }

    pub(crate) fn input_changes(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<engine_core::engine_events::InputState>> {
        self.input_state.subscribe()
    }

    pub(crate) fn peer_events(&self) -> tokio::sync::broadcast::Receiver<EngineEvent> {
        self.peer_events.subscribe()
    }

    /// Atomically fence automatic input before host lifecycle operations.
    pub(crate) async fn lifecycle(
        self: &Arc<Self>,
        allow_inputs: bool,
    ) -> Result<LifecycleGuard, String> {
        let driver = Arc::clone(self);
        let token = self.next_request.fetch_add(1, Ordering::Relaxed);
        let guard = LifecycleGuard {
            driver: Arc::clone(self),
            token,
        };
        tokio::task::spawn_blocking(move || {
            driver.request(|id| EngineCommand::Input {
                id,
                action: engine_core::engine_events::InputAction::AcquireLifecycle {
                    token,
                    allow_inputs,
                },
            })?;
            Ok::<_, String>(guard)
        })
        .await
        .map_err(|error| error.to_string())?
    }

    pub(crate) async fn input(
        self: &Arc<Self>,
        action: engine_core::engine_events::InputAction,
        connection: ConnectionTo<Client>,
    ) -> Result<Value, String> {
        {
            let mut state = self
                .connection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.connection = Some(connection);
            if !self.native_input.swap(true, Ordering::Relaxed) {
                // Native hosts consume completions through the same engine input queue.
                state.tasks = true;
                self.configure_connection(&mut state);
            }
        }
        let driver = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            driver.request(|id| EngineCommand::Input { id, action })
        })
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
        let is_native = self.native_input.load(Ordering::Relaxed);
        let task = tokio::spawn(async move {
            let receipt = if is_native {
                driver.admit_completion(payload).await
            } else {
                match (
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
                }
            };
            let _ = driver.send(EngineCommand::Completion {
                id: None,
                action: CompletionAction::Receipt { attempt, receipt },
            });
        });
        state.transport = Some(task);
    }

    /// A completion is acknowledged only after its input enters durable history.
    async fn admit_completion(
        self: &Arc<Self>,
        payload: engine_core::engine_events::TaskCompletion,
    ) -> CompletionReceipt {
        use engine_core::engine_events::{
            InputAction, InputDelivery, InputSource, TaskCompletion, TurnInput,
        };
        let completion_id = payload.id().to_owned();
        let (source, text, display) = match payload {
            TaskCompletion::Subagent(value) => (
                InputSource::Subagent,
                value.message,
                format!("Sub-agent {}: {}", value.agent_id, value.status),
            ),
            TaskCompletion::Terminal(value) => (
                InputSource::Background,
                value.message,
                format!("Background task {}: {}", value.task_id, value.status),
            ),
        };
        let action = InputAction::Submit {
            client_id: format!("completion:{completion_id}"),
            draft: None,
            input: TurnInput {
                id: 0,
                text,
                display,
                source,
                attachments: Vec::new(),
            },
            delivery: InputDelivery::Queue,
            context: None,
        };
        let mut changes = self.input_changes();
        let driver = Arc::clone(self);
        let result = tokio::task::spawn_blocking(move || {
            driver.request(|id| EngineCommand::Input { id, action })
        })
        .await;
        let Ok(Ok(receipt)) = result else {
            return CompletionReceipt::Retry;
        };
        let Some(id) = receipt["id"].as_u64() else {
            return CompletionReceipt::Retry;
        };
        loop {
            let committed = changes
                .borrow_and_update()
                .as_ref()
                .is_some_and(|state| !state.pending.iter().any(|item| item.id == id));
            if committed {
                return CompletionReceipt::Ack {
                    completion_id: Some(completion_id),
                    disposition: Some("stored".into()),
                    reason: None,
                };
            }
            if changes.changed().await.is_err() {
                return CompletionReceipt::Retry;
            }
        }
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

    /// Release this host's owner and await the engine's final storage lifetime.
    /// Aborting a wire task schedules its drop; it does not release its owner yet.
    pub(crate) fn shutdown(self: Arc<Self>) -> bool {
        let retirement = self.completion_retirement();
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
        drop(self);
        reaped && stopped && retirement.wait(Duration::from_secs(30))
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

/// Dropping the host's lease resumes the engine pump without an additional task.
pub(crate) struct LifecycleGuard {
    driver: Arc<SessionDriver>,
    token: u64,
}
impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        let _ = self.driver.send(EngineCommand::Input {
            id: 0,
            action: engine_core::engine_events::InputAction::ReleaseLifecycle { token: self.token },
        });
    }
}
