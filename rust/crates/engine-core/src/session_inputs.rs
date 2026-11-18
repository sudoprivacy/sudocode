//! Session-owned input delivery behind the existing command/event boundary.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use engine_events::{
    ContentBlock, EngineCommand, EngineEvent, InputAction, InputState, PendingInput, PromptContext,
    TurnInput,
};
use runtime::turn_input::TurnInputInbox;
use serde_json::{json, Value};

use crate::input_queue::{QueuedInput, TurnInputCoordinator};

#[derive(Debug, Clone)]
struct Receipt {
    client_id: String,
    draft: Option<Value>,
    context: Option<PromptContext>,
}

struct State {
    queue: TurnInputCoordinator<Receipt>,
    accepted: HashMap<String, u64>,
    revision: u64,
    native: bool,
    background_watched: bool,
    background: crate::background_completion::Completions,
    lifecycle_held: Option<Lifecycle>,
    active_turn_id: Option<u64>,
    /// The session's turn inbox, for steering a completion into the running turn.
    inbox: Option<TurnInputInbox>,
    interaction_controls: usize,
    interaction_bindings: usize,
}

#[derive(Clone, Copy)]
enum Lifecycle {
    Host { token: u64, allow_inputs: bool },
    Direct,
}

impl Default for State {
    fn default() -> Self {
        Self {
            queue: TurnInputCoordinator::new(),
            accepted: HashMap::new(),
            revision: 0,
            native: false,
            background_watched: false,
            background: crate::background_completion::Completions::default(),
            lifecycle_held: None,
            active_turn_id: None,
            interaction_controls: 0,
            interaction_bindings: 0,
            inbox: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct SessionInputs(Mutex<State>, tokio::sync::Notify);

pub(crate) struct InteractionControlLease {
    inputs: Arc<SessionInputs>,
    binding: bool,
}

impl Drop for InteractionControlLease {
    fn drop(&mut self) {
        let mut state = self.inputs.lock();
        state.interaction_controls -= 1;
        state.interaction_bindings -= usize::from(self.binding);
        self.inputs.1.notify_one();
    }
}

impl State {
    fn recall_human(&mut self) -> Value {
        let items = self.queue.recall_human();
        let mut recalled = Vec::with_capacity(items.len());
        for item in items {
            let meta = item.receipt.expect("session input receipt");
            self.accepted.remove(&meta.client_id);
            let mut content = vec![json!({"type":"text", "text": item.input.text})];
            content.extend(
                item.input
                    .attachments
                    .into_iter()
                    .filter_map(|block| match block {
                        ContentBlock::Image { data, mime_type } => {
                            Some(json!({"type":"image", "data": data, "mimeType": mime_type}))
                        }
                        _ => None,
                    }),
            );
            recalled
                .push(json!({"clientId":meta.client_id, "content":content, "draft":meta.draft}));
        }
        json!(recalled)
    }

    fn snapshot(&self) -> InputState {
        let mut pending: Vec<_> = self
            .queue
            .entries()
            .filter_map(|(item, delivery)| {
                item.receipt.as_ref().map(|receipt| PendingInput {
                    id: item.input.id,
                    client_id: receipt.client_id.clone(),
                    display: item.input.display.clone(),
                    source: item.input.source,
                    delivery,
                })
            })
            .collect();
        pending.sort_unstable_by_key(|item| item.id);
        InputState {
            revision: self.revision,
            active_turn_id: self.active_turn_id,
            running: self.queue.is_running(),
            paused: self.queue.is_paused(),
            pending,
        }
    }

    fn submit(
        &mut self,
        client_id: String,
        draft: Option<Value>,
        mut input: TurnInput,
        delivery: engine_events::InputDelivery,
        context: Option<PromptContext>,
        inbox: Option<&TurnInputInbox>,
    ) -> Result<u64, String> {
        if client_id.is_empty() {
            return Err("clientId must not be empty".into());
        }
        if input.text.trim().is_empty() && input.attachments.is_empty() {
            return Err("input must contain text or an attachment".into());
        }
        if let Some(id) = self.accepted.get(&client_id) {
            Ok(*id)
        } else {
            // Identity is allocated by the session, never trusted from the wire.
            input.id = 0;
            let item = QueuedInput {
                input,
                receipt: Some(Receipt {
                    client_id: client_id.clone(),
                    draft,
                    context,
                }),
                is_command: false,
            };
            if item.input.source == engine_events::InputSource::Human && delivery.is_steer() {
                self.queue.resume();
            }
            let id = if delivery.is_steer() && self.queue.can_deliver() {
                let input = self.queue.steer(item);
                let id = input.id;
                self.admit(vec![input], inbox);
                id
            } else {
                self.queue.enqueue(item).id
            };
            self.accepted.insert(client_id, id);
            Ok(id)
        }
    }

    fn observe_notification(&mut self, event: &EngineEvent) -> bool {
        let notification = match event {
            EngineEvent::BackgroundTask(event) if self.native => {
                self.background.observe(event).map(|finished| {
                    (
                        format!("background:{}", finished.id),
                        finished.prompt,
                        finished.display,
                        engine_events::InputSource::Background,
                    )
                })
            }
            EngineEvent::SubagentCompletionReady(finished) if self.native => {
                finished.completion_notification.clone().map(|prompt| {
                    (
                        format!("subagent:{}", finished.source_key()),
                        prompt,
                        format!(
                            "Sub-agent {}: {}",
                            finished.agent.description,
                            finished.status.as_deref().unwrap_or("finished")
                        ),
                        engine_events::InputSource::Subagent,
                    )
                })
            }
            _ => None,
        };
        if let Some((client_id, text, display, source)) = notification {
            if self.accepted.contains_key(&client_id) {
                return false;
            }
            let item = QueuedInput {
                input: TurnInput {
                    id: 0,
                    text,
                    display,
                    source,
                    pasted: Vec::new(),
                    attachments: Vec::new(),
                },
                receipt: Some(Receipt {
                    client_id: client_id.clone(),
                    draft: None,
                    context: None,
                }),
                is_command: false,
            };
            // A completion is steered like a human steer: into the running
            // turn at its next model boundary, or queued when no turn can take
            // it. The REPL decides the same way. A turn that closes first
            // refuses the admit, and the input goes back to the queue.
            let id = if self.queue.can_deliver() {
                let input = self.queue.steer(item);
                let id = input.id;
                let inbox = self.inbox.clone();
                self.admit(vec![input], inbox.as_ref());
                id
            } else {
                self.queue.enqueue(item).id
            };
            self.accepted.insert(client_id, id);
            return true;
        }
        false
    }

    fn admit(&mut self, inputs: Vec<TurnInput>, inbox: Option<&TurnInputInbox>) {
        let refused = match inbox {
            Some(inbox) => inbox.admit(inputs).err(),
            None => Some(inputs),
        };
        if let Some(inputs) = refused {
            for input in inputs {
                self.queue.defer(input.id);
            }
        }
    }
}

impl SessionInputs {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An action returns a receipt and, only for an empty flush-or-cancel, cancellation.
    pub(crate) fn action(
        &self,
        action: InputAction,
        inbox: Option<&TurnInputInbox>,
    ) -> Result<(Value, Option<InputState>, bool), String> {
        let mut state = self.lock();
        if state.lifecycle_held.is_some_and(|owner| {
            matches!(
                owner,
                Lifecycle::Host {
                    allow_inputs: false,
                    ..
                }
            )
        }) && matches!(
            action,
            InputAction::Submit { .. } | InputAction::Flush | InputAction::FlushOrCancel
        ) {
            return Err("Session control is changing; input was not admitted".into());
        }
        if !matches!(
            action,
            InputAction::AcquireLifecycle { .. } | InputAction::ReleaseLifecycle { .. }
        ) {
            state.native = true;
        }
        let mut cancel = false;
        let mut receipt = json!({});
        match action {
            InputAction::Submit {
                client_id,
                draft,
                input,
                delivery,
                context,
            } => {
                receipt["id"] = json!(state.submit(
                    client_id,
                    draft,
                    input,
                    delivery,
                    context.map(|context| *context),
                    inbox
                )?);
            }
            InputAction::Flush | InputAction::FlushOrCancel => {
                if state.queue.has_pending_followup() {
                    state.queue.resume();
                    if state.queue.can_deliver() {
                        let inputs = state.queue.flush();
                        state.admit(inputs, inbox);
                    }
                } else if matches!(action, InputAction::FlushOrCancel) {
                    cancel = state.queue.is_running();
                    if cancel {
                        state.queue.cancel_turn();
                    }
                }
                receipt["cancelled"] = json!(cancel);
            }
            InputAction::Recall => {
                receipt["recalled"] = state.recall_human();
            }
            InputAction::Snapshot => {}
            InputAction::AcquireLifecycle {
                token,
                allow_inputs,
            } => {
                // Ordinary host prompts share the same admission rule as the
                // native input queue: receipt work may overlap, binding changes
                // may not. Destructive management retains the full fence.
                let controls = if allow_inputs {
                    state.interaction_bindings
                } else {
                    state.interaction_controls
                };
                if state.lifecycle_held.is_some() || state.queue.is_running() || controls != 0 {
                    return Err(
                        "Session is busy; wait for the current turn or cancel it first".into(),
                    );
                }
                state.lifecycle_held = Some(Lifecycle::Host {
                    token,
                    allow_inputs,
                });
            }
            InputAction::ReleaseLifecycle { token } => {
                if state.lifecycle_held.is_some_and(
                    |owner| matches!(owner, Lifecycle::Host { token: held, .. } if held == token),
                ) {
                    state.lifecycle_held = None;
                }
            }
        }
        state.revision += 1;
        let snapshot = state.snapshot();
        receipt["state"] = json!(snapshot);
        Ok((receipt, state.native.then_some(snapshot), cancel))
    }

    pub(crate) async fn changed(&self) {
        self.1.notified().await;
    }

    pub(crate) fn can_start_direct(&self) -> bool {
        let state = self.lock();
        state.lifecycle_held.is_none() && state.interaction_bindings == 0 && state.queue.can_start()
    }

    /// The shared foreground phase ends at the durable terminal event, which
    /// can precede the finalizer task's return to the command pump.
    pub(crate) fn is_running(&self) -> bool {
        self.lock().queue.is_running()
    }

    pub(crate) fn has_interaction_controls(&self) -> bool {
        self.lock().interaction_controls != 0
    }

    pub(crate) fn interaction_binding_pending(&self) -> bool {
        self.lock().interaction_bindings != 0
    }

    /// Reserve foreground ownership before fallible turn admission touches
    /// storage. The real `TurnStarted` event still opens steering afterward.
    pub(crate) fn reserve_turn(&self) -> Option<InputState> {
        let mut state = self.lock();
        if state.queue.can_start() {
            state.queue.start_turn();
            state.revision += 1;
        }
        state.native.then(|| state.snapshot())
    }

    pub(crate) fn begin_interaction_control(
        self: &Arc<Self>,
        binding: bool,
    ) -> Result<InteractionControlLease, String> {
        let mut state = self.lock();
        if matches!(
            state.lifecycle_held,
            Some(Lifecycle::Host {
                allow_inputs: false,
                ..
            })
        ) || (binding && (state.lifecycle_held.is_some() || state.queue.is_running()))
        {
            return Err("Session control is changing; retry the interaction request".into());
        }
        state.interaction_controls += 1;
        state.interaction_bindings += usize::from(binding);
        Ok(InteractionControlLease {
            inputs: self.clone(),
            binding,
        })
    }

    /// Completion controls refer to this engine's immutable completion binding.
    /// A current host lifecycle may configure its connection; later lifecycle
    /// operations still wait for the admitted work to finish.
    pub(crate) fn begin_completion_control(
        self: &Arc<Self>,
        binding: bool,
    ) -> InteractionControlLease {
        let mut state = self.lock();
        state.interaction_controls += 1;
        state.interaction_bindings += usize::from(binding);
        InteractionControlLease {
            inputs: self.clone(),
            binding,
        }
    }

    /// Share the lifecycle guard with host management. Input can still queue,
    /// but no model turn or session replacement starts until this call ends.
    pub(crate) fn begin_direct(&self) -> Result<(), String> {
        let mut state = self.lock();
        if state.lifecycle_held.is_some()
            || state.interaction_bindings != 0
            || !state.queue.can_start()
        {
            return Err("Session is busy; the direct call was not started".into());
        }
        state.lifecycle_held = Some(Lifecycle::Direct);
        Ok(())
    }

    pub(crate) fn finish_direct(&self) {
        let mut state = self.lock();
        if matches!(state.lifecycle_held, Some(Lifecycle::Direct)) {
            state.lifecycle_held = None;
            self.1.notify_one();
        }
    }

    /// The session's turn inbox. A completion that arrives while a turn is
    /// running is steered into it.
    pub(crate) fn attach_inbox(&self, inbox: Option<TurnInputInbox>) {
        self.lock().inbox = inbox;
    }

    pub(crate) fn watch_background(&self) -> bool {
        let mut state = self.lock();
        if !state.native || state.background_watched {
            return false;
        }
        state.background_watched = true;
        true
    }

    pub(crate) fn cancel(&self) -> Option<InputState> {
        let mut state = self.lock();
        if state.queue.is_running() {
            state.queue.cancel_turn();
        } else if state.queue.has_pending_input() {
            // Cancel can arrive after admission but before the first worker starts.
            state.queue.cancel_turn();
            state.queue.finish_turn();
        }
        state.revision += 1;
        state.native.then(|| state.snapshot())
    }

    pub(crate) fn observe(&self, event: &EngineEvent) -> Option<InputState> {
        if !matches!(
            event,
            EngineEvent::TurnStarted { .. }
                | EngineEvent::TurnComplete(_)
                | EngineEvent::TurnFailed { .. }
                | EngineEvent::PromptRejected { .. }
                | EngineEvent::InputCommitted(_)
                | EngineEvent::InputDeferred(_)
                | EngineEvent::InteractionDeliveryReturned(_)
                | EngineEvent::BackgroundTask(_)
                | EngineEvent::SubagentCompletionReady(_)
        ) {
            return None;
        }
        let mut state = self.lock();
        if matches!(
            event,
            EngineEvent::BackgroundTask(_) | EngineEvent::SubagentCompletionReady(_)
        ) {
            // Output chunks are not queue changes. Publish only a newly admitted completion.
            if !state.observe_notification(event) {
                return None;
            }
            self.1.notify_one();
        }
        match event {
            EngineEvent::BackgroundTask(_) | EngineEvent::SubagentCompletionReady(_) => {}
            EngineEvent::TurnStarted { .. } => {
                if state.queue.can_start() {
                    state.queue.start_turn();
                }
                let _ = state.queue.turn_started();
            }
            EngineEvent::InputCommitted(input) => {
                state.queue.committed(input.id);
            }
            EngineEvent::InteractionDeliveryReturned(receipt) => {
                for (_, receipt) in state.queue.return_interaction(*receipt) {
                    if let Some(receipt) = receipt {
                        state.accepted.remove(&receipt.client_id);
                    }
                }
            }
            EngineEvent::InputDeferred(inputs) => {
                for input in inputs {
                    state.queue.defer(input.id);
                }
            }
            EngineEvent::TurnComplete(complete) => {
                state.active_turn_id = None;
                if complete.cancelled {
                    state.queue.finish_failed_turn();
                } else {
                    state.queue.finish_turn();
                }
            }
            EngineEvent::TurnFailed { .. } | EngineEvent::PromptRejected { .. } => {
                state.active_turn_id = None;
                state.queue.finish_failed_turn();
            }
            _ => unreachable!(),
        }
        state.revision += 1;
        state.native.then(|| state.snapshot())
    }

    /// Lease one FIFO batch; keep ownership until the runtime's durable callback.
    pub(crate) fn next_prompt(&self) -> Option<(EngineCommand, InputState)> {
        let mut state = self.lock();
        if state.lifecycle_held.is_some()
            || state.interaction_bindings != 0
            || !state.queue.can_start()
            || !state.queue.has_idle_queue()
        {
            return None;
        }
        let inputs = state.queue.drain_next()?.inputs;
        state.active_turn_id = inputs.first().map(|input| input.id);
        let mut context = state
            .queue
            .entries()
            .next()
            .and_then(|(item, _)| item.receipt.as_ref())
            .map_or_default(|receipt| {
                receipt.context.clone().unwrap_or_else(|| PromptContext {
                    trace_id: receipt.client_id.clone(),
                    ..PromptContext::default()
                })
            });
        context.input_views = inputs.iter().map(TurnInput::view).collect();
        let mut blocks = Vec::new();
        for input in inputs {
            blocks.push(ContentBlock::Text { text: input.text });
            blocks.extend(input.attachments);
        }
        state.queue.start_turn();
        state.revision += 1;
        Some((
            EngineCommand::Prompt {
                blocks,
                context: Some(context),
                shown: Vec::new(),
            },
            state.snapshot(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_events::{BackgroundTaskEvent, InputDelivery};
    use runtime::background_tasks::{BackgroundTask, BackgroundTaskKind, BackgroundTaskStatus};

    fn finished_shell(id: &str) -> EngineEvent {
        let mut task = BackgroundTask::new(id.into(), BackgroundTaskKind::Terminal, "sleep".into());
        task.status = BackgroundTaskStatus::Completed;
        task.exit_code = Some(0);
        EngineEvent::BackgroundTask(BackgroundTaskEvent::Updated(task))
    }

    fn native_session() -> SessionInputs {
        let inputs = SessionInputs::default();
        inputs.action(InputAction::Snapshot, None).unwrap();
        inputs
    }

    fn running(inputs: &SessionInputs, inbox: &TurnInputInbox) {
        inputs.attach_inbox(Some(inbox.clone()));
        inbox.begin();
        inputs.observe(&EngineEvent::TurnStarted {
            label: "turn".into(),
        });
    }

    fn delivery(state: &InputState) -> Vec<InputDelivery> {
        state.pending.iter().map(|item| item.delivery).collect()
    }

    /// While a turn runs, a finished background shell reaches it at the next
    /// model boundary instead of waiting for the turn to end.
    #[test]
    fn a_completion_during_a_turn_is_steered_into_it() {
        let inputs = native_session();
        let inbox = TurnInputInbox::default();
        running(&inputs, &inbox);
        let state = inputs.observe(&finished_shell("shell-1")).unwrap();
        assert!(inbox.has_pending(), "the running turn's inbox holds it");
        assert_eq!(delivery(&state), [InputDelivery::Steer], "{state:?}");
    }

    /// With no turn running there is nothing to steer into: it is queued, and
    /// starts the next turn as before.
    #[test]
    fn a_completion_while_idle_is_queued() {
        let inputs = native_session();
        let inbox = TurnInputInbox::default();
        inputs.attach_inbox(Some(inbox.clone()));
        let state = inputs.observe(&finished_shell("shell-1")).unwrap();
        assert!(!inbox.has_pending());
        assert_eq!(delivery(&state), [InputDelivery::Queue], "{state:?}");
    }

    /// A turn that closed before the completion reached it refuses the admit;
    /// the completion goes back to the queue rather than being lost.
    #[test]
    fn a_completion_refused_by_a_closing_turn_is_queued() {
        let inputs = native_session();
        let inbox = TurnInputInbox::default();
        running(&inputs, &inbox);
        let _ = inbox.finish();
        let state = inputs.observe(&finished_shell("shell-1")).unwrap();
        assert!(!inbox.has_pending());
        assert_eq!(delivery(&state), [InputDelivery::Queue], "{state:?}");
    }
}
