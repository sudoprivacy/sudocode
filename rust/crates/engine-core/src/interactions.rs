//! The shared interaction control path. Renderers project the snapshot and send
//! explicit replies; model turns never own the lifetime of a pending request.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use engine_events::{
    EngineEvent, InputSource, Interaction, InteractionAction, InteractionDeliveryOwner,
    InteractionReceipt, InteractionRequest, InteractionStatus,
};
use runtime::session_interactions::{InteractionJournal, InteractionOrigin, InteractionReply};
use runtime::QuestionPromptRequest;

use crate::subagent_completion::EventSink;
use crate::EngineDelegate;

#[derive(Default)]
pub(crate) struct SessionInteractions {
    journal: Mutex<JournalBinding>,
    delivery: Mutex<Delivery>,
    workers: Mutex<HashMap<String, WorkerWaiter>>,
    enabled: AtomicBool,
    needs_refresh: AtomicBool,
    closing: AtomicBool,
}

/// Configuration failure must not select the legacy blocking prompt path.
/// Retain the previous binding only for worker cleanup after recovery or close.
#[derive(Default)]
enum JournalBinding {
    #[default]
    Disabled,
    Ready(Arc<InteractionJournal>),
    Unavailable {
        previous: Option<Arc<InteractionJournal>>,
        message: String,
    },
}

impl JournalBinding {
    fn previous(&self) -> Option<&Arc<InteractionJournal>> {
        match self {
            Self::Disabled => None,
            Self::Ready(journal) => Some(journal),
            Self::Unavailable { previous, .. } => previous.as_ref(),
        }
    }
}

struct WorkerWaiter {
    journal: Arc<InteractionJournal>,
    reply: tokio::sync::oneshot::Sender<InteractionReply>,
}

/// Dropping the real worker's prompt retires it, including when its operation
/// is cancelled. Finishing the parent's turn does not drop this guard.
struct WorkerPromptLease {
    controller: std::sync::Weak<SessionInteractions>,
    id: String,
    sink: EventSink,
}

impl Drop for WorkerPromptLease {
    fn drop(&mut self) {
        if let Some(controller) = self.controller.upgrade() {
            controller.cancel_worker(
                &self.id,
                "worker stopped waiting for human input",
                &self.sink,
            );
        }
    }
}

#[derive(Default)]
struct Delivery {
    owner: InteractionDeliveryOwner,
    emitted: HashSet<InteractionReceipt>,
}

impl SessionInteractions {
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn needs_refresh(&self) -> bool {
        self.needs_refresh.load(Ordering::Acquire)
    }

    pub(crate) fn invalidate_binding(&self) -> bool {
        let enabled = self.enabled.load(Ordering::Acquire);
        if enabled {
            self.needs_refresh.store(true, Ordering::Release);
        }
        enabled
    }

    pub(crate) fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
    }

    /// Called while idle or from the turn worker before it owns the runtime.
    /// The delegate reuses the same journal for configuration-only rebuilds.
    pub(crate) fn refresh(
        &self,
        delegate: &dyn EngineDelegate,
        sink: &EventSink,
    ) -> Result<(), String> {
        if matches!(
            *self
                .journal
                .lock()
                .map_err(|_| "interaction controller poisoned")?,
            JournalBinding::Disabled
        ) {
            return Ok(());
        }
        let next = match delegate.interaction_journal().and_then(|journal| {
            journal.ok_or_else(|| "this engine does not support durable interactions".into())
        }) {
            Ok(journal) => journal,
            Err(message) => {
                self.needs_refresh.store(true, Ordering::Release);
                let mut current = self
                    .journal
                    .lock()
                    .map_err(|_| "interaction controller poisoned")?;
                *current = JournalBinding::Unavailable {
                    previous: current.previous().cloned(),
                    message: message.clone(),
                };
                return Err(message);
            }
        };
        let mut delivery = self
            .delivery
            .lock()
            .map_err(|_| "interaction delivery poisoned")?;
        let mut current = self
            .journal
            .lock()
            .map_err(|_| "interaction controller poisoned")?;
        let previous = current.previous().cloned();
        *current = JournalBinding::Ready(next.clone());
        self.needs_refresh.store(false, Ordering::Release);
        if previous
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &next))
        {
            return Ok(());
        }
        delivery.emitted.clear();
        drop(current);
        drop(delivery);
        self.cancel_workers(previous.as_ref(), "parent selected another session", sink);
        let _ = sink.send(EngineEvent::Interactions(next.snapshot()?));
        self.deliver(delegate, sink, &next)
    }

    pub(crate) fn validate_inputs(
        &self,
        inputs: &[engine_events::InputEcho],
    ) -> Result<(), String> {
        for input in inputs {
            if let InputSource::Interaction(receipt) = input.source {
                self.journal()?
                    .ok_or("durable interactions have not been configured")?
                    .validate_receipt(receipt)?;
            }
        }
        Ok(())
    }

    pub(crate) fn check_plan_gate(
        &self,
        tool_name: &str,
        required: runtime::PermissionMode,
    ) -> Result<(), String> {
        if self.closing.load(Ordering::Acquire) {
            return Err("parent session closed".into());
        }
        match self.journal()? {
            Some(journal) => journal.check_plan_gate(tool_name, required),
            None => Ok(()),
        }
    }

    pub(crate) fn input_context_action(
        &self,
        inputs: &[engine_events::InputEcho],
    ) -> Result<Option<runtime::image_input::ToolContextAction>, String> {
        match self.journal()? {
            Some(journal) => journal.input_context_action(inputs),
            None => Ok(None),
        }
    }

    pub(crate) fn release(&self, sink: &EventSink) {
        self.enabled.store(false, Ordering::Release);
        self.needs_refresh.store(false, Ordering::Release);
        let mut delivery = self
            .delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::mem::take(
            &mut *self
                .journal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .previous()
        .cloned();
        delivery.emitted.clear();
        drop(delivery);
        self.cancel_workers(previous.as_ref(), "parent session closed", sink);
    }

    pub(crate) fn command(
        &self,
        delegate: &dyn EngineDelegate,
        action: InteractionAction,
        sink: &EventSink,
        running: bool,
    ) -> Result<serde_json::Value, String> {
        if !running {
            self.refresh(delegate, sink)?;
        }
        let should_deliver = !matches!(
            action,
            InteractionAction::Snapshot | InteractionAction::Result { .. }
        );
        if let InteractionAction::Configure { enabled, delivery } = action {
            self.configure(delegate, enabled, delivery, sink, running)?;
        } else {
            let journal = self
                .journal()?
                .ok_or("durable interactions have not been configured")?;
            match action {
                InteractionAction::Snapshot => {}
                InteractionAction::Result { interaction_id } => {
                    let record = journal.record(&interaction_id)?;
                    if !matches!(
                        record.origin,
                        runtime::session_interactions::InteractionOrigin::Direct { .. }
                    ) {
                        return Err("only direct host operations have a separate result".into());
                    }
                    let saved = journal.stored_result(&interaction_id)?;
                    return Ok(serde_json::json!({
                        "request_id": interaction_id,
                        "status": record.status,
                        "execution": record.execution,
                        "result": saved.and_then(|saved| saved.get("direct").cloned()),
                    }));
                }
                InteractionAction::Reply {
                    interaction_id,
                    revision,
                    reply,
                } => {
                    let record = journal.record(&interaction_id)?;
                    if matches!(record.origin, InteractionOrigin::Worker { .. })
                        && record.status == InteractionStatus::Pending
                        && !matches!(reply, InteractionReply::Cancelled { .. })
                        && !self
                            .workers
                            .lock()
                            .map_err(|_| "worker prompts poisoned")?
                            .contains_key(&interaction_id)
                    {
                        return Err("the originating worker is not running in this engine; reply through its owning engine or cancel this request".into());
                    }
                    let resolved = journal.resolve(&interaction_id, revision, reply)?;
                    if matches!(record.origin, InteractionOrigin::Direct { .. }) {
                        // An explicit idempotent reply can retry delivery after
                        // a transport/storage failure. Claim admission still
                        // prevents executing the operation twice.
                        self.return_delivery(InteractionReceipt {
                            id: interaction_id
                                .parse()
                                .map_err(|_| "invalid interaction identity")?,
                            revision: resolved.revision,
                        });
                    }
                }
                InteractionAction::Configure { .. } => unreachable!(),
            }
        }
        if let Some(journal) = self.journal()? {
            if should_deliver {
                self.deliver(delegate, sink, &journal)?;
            }
            let snapshot = journal.snapshot()?;
            let value = serde_json::to_value(&snapshot).map_err(|e| e.to_string())?;
            let _ = sink.send(EngineEvent::Interactions(snapshot));
            Ok(value)
        } else {
            Ok(serde_json::json!({"enabled": false}))
        }
    }

    fn configure(
        &self,
        delegate: &dyn EngineDelegate,
        enabled: bool,
        delivery: InteractionDeliveryOwner,
        sink: &EventSink,
        running: bool,
    ) -> Result<(), String> {
        if enabled
            && !runtime::experiments::is_enabled(
                runtime::experiments::Experiment::NonblockingInteractions,
            )
        {
            return Err("nonblockingInteractions is experimental and disabled".into());
        }
        if running {
            return Err("configure interactions before starting a turn".into());
        }
        if self.journal()?.is_some()
            && self
                .delivery
                .lock()
                .map_err(|_| "interaction delivery poisoned")?
                .owner
                != delivery
        {
            return Err("interaction delivery ownership cannot change on a loaded session".into());
        }
        self.delivery
            .lock()
            .map_err(|_| "interaction delivery poisoned")?
            .owner = delivery;
        if enabled {
            self.enabled.store(true, Ordering::Release);
            if self.journal()?.is_none() {
                *self
                    .journal
                    .lock()
                    .map_err(|_| "interaction controller poisoned")? =
                    JournalBinding::Unavailable {
                        previous: None,
                        message: "interaction journal has not been opened".into(),
                    };
            }
            // Record the required binding before fallible storage access.
            // A later command retries opening it; disable also requires a
            // readable journal proving that no decisions remain pending.
            self.refresh(delegate, sink)?;
        } else {
            // Hold the binding while checking storage so a local worker
            // cannot register between the check and disabling delivery.
            let mut current = self
                .journal
                .lock()
                .map_err(|_| "interaction controller poisoned")?;
            match &*current {
                JournalBinding::Ready(journal) => journal.ensure_can_disable()?,
                JournalBinding::Unavailable { message, .. } => return Err(message.clone()),
                JournalBinding::Disabled => {}
            }
            *current = JournalBinding::Disabled;
            self.enabled.store(false, Ordering::Release);
            self.needs_refresh.store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Replies use the same input queue as ordinary conversation. Persisting a
    /// reply is not delivery; only the runtime's committed-input event retires it.
    pub(crate) fn retry(&self, delegate: &dyn EngineDelegate, sink: &EventSink) {
        let result = self.journal().and_then(|journal| match journal {
            Some(journal) => self.deliver(delegate, sink, &journal),
            None => Ok(()),
        });
        if let Err(message) = result {
            let _ = sink.send(EngineEvent::InteractionError { message });
        }
    }

    pub(crate) fn next_direct(&self) -> Result<Option<InteractionReceipt>, String> {
        if self.closing.load(Ordering::Acquire) {
            return Ok(None);
        }
        let Some(journal) = self.journal()? else {
            return Ok(None);
        };
        let mut delivery = self
            .delivery
            .lock()
            .map_err(|_| "interaction delivery poisoned")?;
        for request in journal.snapshot()?.requests {
            if request.status != InteractionStatus::Resolved
                || !matches!(
                    request.origin,
                    runtime::session_interactions::InteractionOrigin::Direct { .. }
                )
            {
                continue;
            }
            let receipt = InteractionReceipt {
                id: request
                    .id
                    .parse()
                    .map_err(|_| "invalid interaction identity")?,
                revision: request.revision,
            };
            if journal.delivery_block_reason(receipt)?.is_some() {
                continue;
            }
            if delivery.emitted.insert(receipt) {
                return Ok(Some(receipt));
            }
        }
        Ok(None)
    }

    fn deliver(
        &self,
        delegate: &dyn EngineDelegate,
        sink: &EventSink,
        journal: &InteractionJournal,
    ) -> Result<(), String> {
        // Saving an answer remains available while the user owns tool control.
        // Resuming that same control wakes the engine to retry admission.
        if self.closing.load(Ordering::Acquire) || delegate.tool_control().1 {
            return Ok(());
        }
        let mut delivery = self
            .delivery
            .lock()
            .map_err(|_| "interaction delivery poisoned")?;
        if !self
            .journal()?
            .as_ref()
            .is_some_and(|current| std::ptr::eq(current.as_ref(), journal))
        {
            return Err("the active interaction session changed before delivery".into());
        }
        let mut updates = Vec::new();
        for interaction in journal.snapshot()?.requests {
            if interaction.status != InteractionStatus::Resolved {
                continue;
            }
            if matches!(interaction.origin, InteractionOrigin::Worker { .. }) {
                self.deliver_worker(journal, &interaction)?;
                continue;
            }
            if !matches!(
                interaction.origin,
                runtime::session_interactions::InteractionOrigin::Conversation
            ) {
                continue;
            }
            let receipt = InteractionReceipt {
                id: interaction
                    .id
                    .parse()
                    .map_err(|_| "invalid interaction identity")?,
                revision: interaction.revision,
            };
            if delivery.emitted.contains(&receipt) {
                continue;
            }
            if journal.delivery_block_reason(receipt)?.is_some() {
                continue;
            }
            let text = serde_json::to_string(&serde_json::json!({
                "interaction_reply": {
                    "request_id": interaction.id,
                    "source": interaction.source,
                    "revision": interaction.revision,
                    "request": interaction.request,
                    "reply": interaction.reply,
                }
            }))
            .map_err(|error| error.to_string())?;
            let input = engine_events::TurnInput {
                id: 0,
                text,
                display: format!("Answered request {}", interaction.id),
                source: InputSource::Interaction(receipt),
                pasted: Vec::new(),
                attachments: Vec::new(),
            };
            if delivery.owner == InteractionDeliveryOwner::Host {
                sink.send(EngineEvent::InteractionInput(input))
                    .map_err(|_| "interaction host disconnected")?;
                delivery.emitted.insert(receipt);
                continue;
            }
            let action = engine_events::InputAction::Submit {
                client_id: format!("interaction:{}:{}", interaction.id, interaction.revision),
                draft: None,
                input,
                delivery: engine_events::InputDelivery::Steer,
                context: None,
            };
            // Hold the receipt lock until admission finishes: a running turn
            // can commit a steering input immediately.
            let state = {
                let (_, state, _) = crate::session::admit_input(delegate, action, sink)?;
                delivery.emitted.insert(receipt);
                state
            };
            if let Some(state) = state {
                updates.push(state);
            }
        }
        drop(delivery);
        for state in updates {
            let _ = sink.send(EngineEvent::InputState(state));
        }
        Ok(())
    }

    pub(crate) fn return_delivery(&self, receipt: InteractionReceipt) {
        self.delivery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .emitted
            .remove(&receipt);
    }

    pub(crate) fn committed(
        &self,
        source: InputSource,
    ) -> Result<Option<engine_events::InteractionSnapshot>, String> {
        let InputSource::Interaction(receipt) = source else {
            return Ok(None);
        };
        let mut delivery = self
            .delivery
            .lock()
            .map_err(|_| "interaction delivery poisoned")?;
        let journal = self
            .journal()?
            .ok_or("interaction controller is no longer configured")?;
        journal.acknowledge(&receipt.id.to_string(), receipt.revision)?;
        delivery.emitted.remove(&receipt);
        journal.snapshot().map(Some)
    }

    pub(crate) fn journal(&self) -> Result<Option<Arc<InteractionJournal>>, String> {
        match &*self
            .journal
            .lock()
            .map_err(|_| "interaction controller poisoned")?
        {
            JournalBinding::Disabled => Ok(None),
            JournalBinding::Ready(journal) => Ok(Some(journal.clone())),
            JournalBinding::Unavailable { message, .. } => Err(message.clone()),
        }
    }

    pub(crate) fn worker_sink(
        self: &Arc<Self>,
        sink: EventSink,
    ) -> Result<Option<runtime::PermissionPromptSink>, String> {
        let Some(journal) = self.journal()? else {
            return Ok(None);
        };
        let controller = Arc::downgrade(self);
        let gate_controller = controller.clone();
        let gate_journal = journal.clone();
        Ok(Some(runtime::PermissionPromptSink::session_owned(
            move |agent_id, request| {
                let controller = controller.clone();
                let journal = journal.clone();
                let sink = sink.clone();
                Box::pin(async move {
                    let Some(active) = controller.upgrade() else {
                        return InteractionReply::Cancelled {
                            reason: "parent session closed".into(),
                        };
                    };
                    let registered = active.register_worker(&journal, agent_id, request, &sink);
                    let (id, reply) = match registered {
                        Ok(registered) => registered,
                        Err(reason) => return InteractionReply::Cancelled { reason },
                    };
                    let _lease = WorkerPromptLease {
                        controller,
                        id,
                        sink,
                    };
                    drop(active);
                    reply.await.unwrap_or_else(|_| InteractionReply::Cancelled {
                        reason: "parent session closed before the worker received a reply".into(),
                    })
                })
            },
            move |tool_name, required| {
                let controller = gate_controller.upgrade().ok_or("parent session closed")?;
                if controller.closing.load(Ordering::Acquire) {
                    return Err("parent session closed".into());
                }
                if !controller
                    .journal()?
                    .is_some_and(|active| Arc::ptr_eq(&active, &gate_journal))
                {
                    return Err("parent interaction session changed".into());
                }
                gate_journal.check_plan_gate(tool_name, required)
            },
        )))
    }

    fn register_worker(
        &self,
        journal: &Arc<InteractionJournal>,
        agent_id: String,
        request: InteractionRequest,
        sink: &EventSink,
    ) -> Result<(String, tokio::sync::oneshot::Receiver<InteractionReply>), String> {
        if self.closing.load(Ordering::Acquire) {
            return Err("parent session closed".into());
        }
        let mut workers = self.workers.lock().map_err(|_| "worker prompts poisoned")?;
        let current = self
            .journal
            .lock()
            .map_err(|_| "interaction controller poisoned")?;
        if !matches!(&*current, JournalBinding::Ready(active) if Arc::ptr_eq(active, journal)) {
            return Err(
                "parent interaction session changed before the worker requested input".into(),
            );
        }
        let interaction = journal.register_worker(agent_id, request)?;
        let (reply, receiver) = tokio::sync::oneshot::channel();
        workers.insert(
            interaction.id.clone(),
            WorkerWaiter {
                journal: journal.clone(),
                reply,
            },
        );
        drop(current);
        drop(workers);
        let snapshot = match journal.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.cancel_worker(&interaction.id, &error, sink);
                return Err(error);
            }
        };
        let _ = sink.send(EngineEvent::Interactions(snapshot));
        Ok((interaction.id, receiver))
    }

    fn deliver_worker(
        &self,
        journal: &InteractionJournal,
        interaction: &Interaction,
    ) -> Result<(), String> {
        let mut workers = self.workers.lock().map_err(|_| "worker prompts poisoned")?;
        let reply = interaction
            .reply
            .clone()
            .ok_or("worker interaction has no reply")?;
        let Some(waiter) = workers.get(&interaction.id) else {
            // A sibling context's missing local waiter does not prove that
            // the original worker stopped. Only explicit cancellation may
            // retire its request here; approvals stay with their live owner.
            if matches!(reply, InteractionReply::Cancelled { .. }) {
                journal.acknowledge(&interaction.id, interaction.revision)?;
                return Ok(());
            }
            return Err(format!("Request {} belongs to a worker outside this engine; reply through its owning engine or cancel it", interaction.id));
        };
        if !std::ptr::eq(waiter.journal.as_ref(), journal) {
            return Err("worker reply belongs to another session".into());
        }
        let admission = (|| {
            if matches!(reply, InteractionReply::Permission { approved: true, .. }) {
                journal.claim_permission(InteractionReceipt {
                    id: interaction
                        .id
                        .parse()
                        .map_err(|_| "invalid worker interaction identity")?,
                    revision: interaction.revision,
                })?;
            }
            // Persist consumption before waking the only authorized executor.
            // A crash may lose a response, but cannot replay an action.
            journal.acknowledge(&interaction.id, interaction.revision)
        })();
        if let Err(reason) = admission {
            if let Some(waiter) = workers.remove(&interaction.id) {
                let _ = waiter.reply.send(InteractionReply::Cancelled {
                    reason: reason.clone(),
                });
            }
            return Err(reason);
        }
        if let Some(waiter) = workers.remove(&interaction.id) {
            let _ = waiter.reply.send(reply);
        }
        Ok(())
    }

    fn cancel_worker(&self, id: &str, reason: &str, sink: &EventSink) {
        let waiter = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if let Some(waiter) = waiter {
            let journal = waiter.journal.clone();
            retire_worker(waiter, id, reason, sink);
            if self
                .journal()
                .ok()
                .flatten()
                .is_some_and(|active| Arc::ptr_eq(&active, &journal))
            {
                if let Ok(snapshot) = journal.snapshot() {
                    let _ = sink.send(EngineEvent::Interactions(snapshot));
                }
            }
        }
    }

    fn cancel_workers(
        &self,
        journal: Option<&Arc<InteractionJournal>>,
        reason: &str,
        sink: &EventSink,
    ) {
        let mut current = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<_> = current
            .iter()
            .filter(|(_, waiter)| {
                journal.is_some_and(|journal| Arc::ptr_eq(journal, &waiter.journal))
            })
            .map(|(id, _)| id.clone())
            .collect();
        let workers: Vec<_> = ids
            .into_iter()
            .filter_map(|id| current.remove(&id).map(|waiter| (id, waiter)))
            .collect();
        drop(current);
        for (id, waiter) in workers {
            retire_worker(waiter, &id, reason, sink);
        }
    }

    pub(crate) fn submit_question(
        &self,
        source: &str,
        request: &QuestionPromptRequest,
        sink: &EventSink,
    ) -> Result<Option<Interaction>, String> {
        self.submit(source, InteractionRequest::Question(request.clone()), sink)
    }

    pub(crate) fn submit(
        &self,
        source: &str,
        request: InteractionRequest,
        sink: &EventSink,
    ) -> Result<Option<Interaction>, String> {
        self.submit_from(
            source,
            request,
            runtime::session_interactions::InteractionOrigin::Conversation,
            sink,
        )
    }

    pub(crate) fn submit_from(
        &self,
        source: &str,
        request: InteractionRequest,
        origin: runtime::session_interactions::InteractionOrigin,
        sink: &EventSink,
    ) -> Result<Option<Interaction>, String> {
        let Some(journal) = self.journal()? else {
            return Ok(None);
        };
        let interaction = journal.register_from(source.to_owned(), request, origin)?;
        let _ = sink.send(EngineEvent::Interactions(journal.snapshot()?));
        Ok(Some(interaction))
    }
}

fn retire_worker(waiter: WorkerWaiter, id: &str, reason: &str, sink: &EventSink) {
    let result = (|| {
        let mut record = waiter.journal.record(id)?;
        if record.status == InteractionStatus::Pending {
            record = waiter.journal.resolve(
                id,
                record.revision,
                InteractionReply::Cancelled {
                    reason: reason.into(),
                },
            )?;
        }
        if record.status == InteractionStatus::Resolved {
            waiter.journal.acknowledge(id, record.revision)?;
        }
        Ok::<_, String>(())
    })();
    let _ = waiter.reply.send(InteractionReply::Cancelled {
        reason: reason.into(),
    });
    if let Err(message) = result {
        let _ = sink.send(EngineEvent::InteractionError { message });
    }
}
