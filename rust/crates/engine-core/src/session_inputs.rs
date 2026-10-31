//! Session-owned input delivery behind the existing command/event boundary.
use std::collections::HashMap;
use std::sync::Mutex;

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
    lifecycle_held: Option<(u64, bool)>,
    active_turn_id: Option<u64>,
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
        }
    }
}

#[derive(Default)]
pub(crate) struct SessionInputs(Mutex<State>, tokio::sync::Notify);

impl State {
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
            let id = self
                .queue
                .enqueue(QueuedInput {
                    input: TurnInput {
                        id: 0,
                        text,
                        display,
                        source,
                        attachments: Vec::new(),
                    },
                    receipt: Some(Receipt {
                        client_id: client_id.clone(),
                        draft: None,
                        context: None,
                    }),
                })
                .id;
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
        if state
            .lifecycle_held
            .is_some_and(|(_, allow_inputs)| !allow_inputs)
            && matches!(
                action,
                InputAction::Submit { .. } | InputAction::Flush | InputAction::FlushOrCancel
            )
        {
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
                if state.queue.has_pending_input() {
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
                let items = state.queue.recall_human();
                let mut recalled = Vec::with_capacity(items.len());
                for item in items {
                    let meta = item.receipt.expect("session input receipt");
                    state.accepted.remove(&meta.client_id);
                    let mut content = vec![json!({"type":"text", "text": item.input.text})];
                    content.extend(item.input.attachments.into_iter().filter_map(
                        |block| match block {
                            ContentBlock::Image { data, mime_type } => {
                                Some(json!({"type":"image", "data": data, "mimeType": mime_type}))
                            }
                            _ => None,
                        },
                    ));
                    recalled.push(
                        json!({"clientId":meta.client_id, "content":content, "draft":meta.draft}),
                    );
                }
                receipt["recalled"] = json!(recalled);
            }
            InputAction::Snapshot => {}
            InputAction::AcquireLifecycle {
                token,
                allow_inputs,
            } => {
                if state.lifecycle_held.is_some() || state.queue.is_running() {
                    return Err(
                        "Session is busy; wait for the current turn or cancel it first".into(),
                    );
                }
                state.lifecycle_held = Some((token, allow_inputs));
            }
            InputAction::ReleaseLifecycle { token } => {
                if state.lifecycle_held.is_some_and(|(held, _)| held == token) {
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
            EngineEvent::InputDeferred(inputs) => {
                for input in inputs {
                    state.queue.defer(input.id);
                }
            }
            EngineEvent::TurnComplete(complete) => {
                state.active_turn_id = None;
                if complete.cancelled {
                    state.queue.cancel_turn();
                }
                state.queue.finish_turn();
            }
            EngineEvent::TurnFailed { .. } | EngineEvent::PromptRejected { .. } => {
                state.active_turn_id = None;
                state.queue.cancel_turn();
                state.queue.finish_turn();
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
            || !state.queue.can_start()
            || !state.queue.has_idle_queue()
        {
            return None;
        }
        let inputs = state.queue.flush();
        state.active_turn_id = inputs.first().map(|input| input.id);
        let mut context = state
            .queue
            .entries()
            .next()
            .and_then(|(item, _)| item.receipt.as_ref())
            .map(|receipt| {
                receipt.context.clone().unwrap_or_else(|| PromptContext {
                    trace_id: receipt.client_id.clone(),
                    ..PromptContext::default()
                })
            })
            .unwrap_or_default();
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
            },
            state.snapshot(),
        ))
    }
}
