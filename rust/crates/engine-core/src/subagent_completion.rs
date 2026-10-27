//! The engine event adapter for the shared completion qualification reducer.
use crate::EngineEvent;
use engine_events::{SubagentLifecycle, SubagentUpdate};

#[derive(Default)]
struct Completions(runtime::subagent_completion::Completions);

impl Completions {
    fn observe(&mut self, event: &EngineEvent) -> Vec<SubagentLifecycle> {
        match event {
            EngineEvent::TurnStarted { .. } => self.0.begin(),
            EngineEvent::TurnComplete(_) | EngineEvent::TurnFailed { .. } => self.0.close(),
            EngineEvent::Subagent(event) if event.stream.is_none() => {
                if let SubagentUpdate::Lifecycle(lifecycle) = &event.update {
                    self.0.lifecycle(lifecycle);
                }
            }
            EngineEvent::ToolResult {
                assistant_message_id,
                id,
                name,
                output,
                ..
            } => self
                .0
                .tool_result(assistant_message_id.as_deref(), id, name, output),
            _ => return Vec::new(),
        }
        self.0.drain()
    }
}

mod control;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use engine_events::PromptContext;
use runtime::subagent_completion::CompletionJournal;

/// Session-owned completion state. Transports exchange frozen payloads and raw
/// receipts; qualification, admission and receipt validation live in the engine.
#[derive(Default)]
pub struct CompletionState {
    journal: Mutex<Option<Arc<CompletionJournal>>>,
    binding: Option<control::StorageBinding>,
    live: std::sync::atomic::AtomicBool,
    wake: Arc<tokio::sync::Notify>,
    delivery: Mutex<control::DeliveryState>,
    inner: Mutex<CompletionTurn>,
    // Fields drop in declaration order. Keep this last so retirement follows
    // the journal/storage fields, including a worker's final state owner.
    pub(crate) lifetime: crate::attachment::CompletionLifetime,
}

#[derive(Default)]
struct CompletionTurn {
    volatile: Completions,
    agents: HashSet<String>,
    trace: Option<String>,
    failure: Option<String>,
    terminal_retries: BTreeMap<String, SubagentLifecycle>,
}

impl CompletionState {
    pub(crate) fn journal(&self) -> Option<Arc<CompletionJournal>> {
        self.journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn begin(&self, context: Option<&PromptContext>) -> Result<(), String> {
        let mut turn = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(journal) = self.journal() {
            let context = context.ok_or("completion prompt requires traceId")?;
            journal.begin(&context.trace_id, context.expected_context.as_ref())?;
            turn.trace = Some(context.trace_id.clone());
        }
        turn.failure = None;
        Ok(())
    }

    pub(crate) fn cancel_subagent(&self, agent_id: &str) -> Result<serde_json::Value, String> {
        if !self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agents
            .contains(agent_id)
        {
            return Err(format!("agent {agent_id} was not spawned in this session"));
        }
        Ok(serde_json::Value::Bool(tools::abort_registered_agent(
            agent_id,
        )))
    }

    fn commit_locked(
        &self,
        turn: &mut CompletionTurn,
        trace: &str,
    ) -> Result<engine_events::ContextRef, String> {
        self.retry_locked(turn)?;
        if let Some(error) = turn.failure.take() {
            return Err(error);
        }
        self.journal()
            .as_ref()
            .ok_or("session has no durable completion journal")?
            .commit(trace)
    }

    /// Retry durable terminal observations without changing the turn boundary.
    pub(crate) fn retry_terminals(&self) -> Result<(), String> {
        let mut turn = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.retry_locked(&mut turn)
    }

    fn retry_locked(&self, turn: &mut CompletionTurn) -> Result<(), String> {
        if let Some(journal) = self.journal() {
            while let Some((id, event)) = turn.terminal_retries.first_key_value() {
                journal.lifecycle(event)?;
                let id = id.clone();
                turn.terminal_retries.remove(&id);
            }
        }
        Ok(())
    }

    pub(crate) fn observe(&self, mut event: EngineEvent) -> Vec<EngineEvent> {
        let mut turn = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let EngineEvent::Subagent(event) = &event {
            if let SubagentUpdate::Lifecycle(lifecycle) = &event.update {
                if lifecycle.phase == engine_events::SubagentPhase::Started {
                    turn.agents.insert(lifecycle.agent.agent_id.clone());
                }
            }
        }
        let mut output = Vec::new();
        if let Some(journal) = self.journal() {
            let observed = match &event {
                EngineEvent::Subagent(event) if event.stream.is_none() => {
                    if let SubagentUpdate::Lifecycle(lifecycle) = &event.update {
                        if let Err(error) = journal.lifecycle(lifecycle) {
                            let mut terminal = lifecycle.as_ref().clone();
                            terminal.raw_output = None;
                            turn.terminal_retries
                                .insert(lifecycle.source_key(), terminal);
                            eprintln!("[engine-completion] terminal persistence pending: {error}");
                        }
                    }
                    Ok(())
                }
                EngineEvent::ToolResult {
                    assistant_message_id,
                    id,
                    name,
                    output,
                    ..
                } => journal.tool_result(assistant_message_id.as_deref(), id, name, output),
                _ => Ok(()),
            };
            if let Err(error) = observed {
                turn.failure = Some(error);
            }
            if matches!(
                event,
                EngineEvent::TurnComplete(_) | EngineEvent::TurnFailed { .. }
            ) {
                let trace = turn.trace.take().unwrap_or_default();
                if matches!(event, EngineEvent::TurnComplete(_)) {
                    let committed = self.commit_locked(&mut turn, &trace);
                    match committed {
                        Ok(context) => output.push(EngineEvent::ContextCommitted(context)),
                        Err(error) => {
                            let failure = journal.fail_turn(&trace).err();
                            event = EngineEvent::TurnFailed {
                                message: failure.map_or(error.clone(), |failure| {
                                    format!("{error}; completion rollback failed: {failure}")
                                }),
                                context_window: false,
                            };
                        }
                    }
                } else if let Err(error) = journal.fail_turn(&trace) {
                    event = EngineEvent::TurnFailed {
                        message: error,
                        context_window: false,
                    };
                }
            }
        } else {
            for completion in turn.volatile.observe(&event) {
                output.push(EngineEvent::SubagentCompletionReady(Box::new(completion)));
            }
        }
        // A terminal event is the barrier after every state observation and
        // context commit. Frontends can respond as soon as they receive it.
        if matches!(
            event,
            EngineEvent::TurnComplete(_) | EngineEvent::TurnFailed { .. }
        ) {
            output.push(event);
        } else {
            output.insert(0, event);
        }
        self.wake.notify_one();
        output
    }
}

/// Serializes completion-affecting events before crossing the renderer seam.
/// Streaming deltas retain the direct channel path and take no state lock.
/// The pump owns the route; retained host callbacks cannot keep it alive.
#[derive(Clone)]
pub(crate) struct EventSink {
    tx: Weak<std::sync::mpsc::Sender<EngineEvent>>,
    order: Arc<Mutex<()>>,
    state: Arc<Mutex<engine_events::EngineState>>,
    pub(crate) completions: Arc<CompletionState>,
    pub(crate) inputs: Arc<crate::session_inputs::SessionInputs>,
}

impl EventSink {
    pub(crate) fn new(
        tx: &Arc<std::sync::mpsc::Sender<EngineEvent>>,
        completions: Arc<CompletionState>,
    ) -> Self {
        Self {
            tx: Arc::downgrade(tx),
            completions,
            order: Arc::default(),
            state: Arc::new(Mutex::new(engine_events::EngineState::Idle)),
            inputs: Arc::default(),
        }
    }

    pub(crate) fn reject_prompt(
        &self,
        message: String,
    ) -> Result<(), std::sync::mpsc::SendError<()>> {
        self.send(EngineEvent::PromptRejected { message })?;
        self.send(EngineEvent::State(engine_events::EngineState::Idle))
    }

    pub(crate) fn runtime_status(
        &self,
        tasks: &runtime::background_tasks::BackgroundTasks,
    ) -> engine_events::RuntimeStatus {
        let foreground = *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        engine_events::RuntimeStatus {
            foreground,
            background: tasks.summary(),
        }
    }

    pub(crate) fn send(&self, event: EngineEvent) -> Result<(), std::sync::mpsc::SendError<()>> {
        let tx = self.route()?;
        if let EngineEvent::State(state) = event {
            // Keep query observations in the same order as lifecycle events.
            // Provider output and elapsed time never mutate this projection.
            let mut current = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current = state;
            return tx
                .send(EngineEvent::State(state))
                .map_err(|_| std::sync::mpsc::SendError(()));
        }
        if matches!(
            event,
            EngineEvent::TurnStarted { .. }
                | EngineEvent::TurnComplete(_)
                | EngineEvent::TurnFailed { .. }
                | EngineEvent::PromptRejected { .. }
                | EngineEvent::InputCommitted(_)
                | EngineEvent::InputDeferred(_)
                | EngineEvent::BackgroundTask(_)
                | EngineEvent::ToolResult { .. }
        ) || matches!(&event, EngineEvent::Subagent(event) if matches!(event.update, SubagentUpdate::Lifecycle(_)))
        {
            let _order = self
                .order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let events = if matches!(event, EngineEvent::BackgroundTask(_)) {
                vec![event]
            } else {
                self.completions.observe(event)
            };
            for event in events {
                let input_state = if matches!(event, EngineEvent::BackgroundTask(_))
                    && self.completions.journal().is_some()
                {
                    None
                } else {
                    self.inputs.observe(&event)
                };
                tx.send(event).map_err(|_| std::sync::mpsc::SendError(()))?;
                if let Some(state) = input_state {
                    tx.send(EngineEvent::InputState(state))
                        .map_err(|_| std::sync::mpsc::SendError(()))?;
                }
            }
            Ok(())
        } else {
            tx.send(event).map_err(|_| std::sync::mpsc::SendError(()))
        }
    }

    fn route(
        &self,
    ) -> Result<Arc<std::sync::mpsc::Sender<EngineEvent>>, std::sync::mpsc::SendError<()>> {
        self.tx.upgrade().ok_or(std::sync::mpsc::SendError(()))
    }
}
