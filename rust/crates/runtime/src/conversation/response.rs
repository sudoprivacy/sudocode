//! Drive one provider response and its tool lifetimes together.
use super::{
    build_assistant_message, notify_tool_result, runtime_observer_mut, tool_execution, ApiClient,
    AssistantEvent, AssistantEventStream, ContentBlock, ConversationMessage, ConversationRuntime,
    PermissionPrompter, RuntimeError, RuntimeObserver, ToolDispatchContext, ToolExecutor,
    INTERRUPT_MESSAGE,
};
use futures::StreamExt;
use std::sync::Arc;

impl<C: ApiClient, T: ToolExecutor> ConversationRuntime<C, T> {
    fn dispatch_context(
        &self,
        observer: &Option<&mut dyn RuntimeObserver>,
        prompter: &Option<&mut dyn PermissionPrompter>,
    ) -> ToolDispatchContext {
        ToolDispatchContext {
            parent_assistant_message: None,
            parent_session_messages: self.session.messages.clone(),
            tool_results_dir: self.session.tool_results_dir(),
            progress_sink: observer
                .as_deref()
                .and_then(RuntimeObserver::tool_progress_sink),
            parent_reasoning_effort: self.api_client.reasoning_effort().map(str::to_string),
            parent_thinking_enabled: self.api_client.thinking_enabled(),
            parent_routing_session_id: self.api_client.routing_session_id().map(str::to_string),
            parent_requires_model_mount: self.api_client.requires_model_mount(),
            subagent_sink: observer.as_deref().and_then(RuntimeObserver::subagent_sink),
            background_tasks: observer
                .as_deref()
                .and_then(RuntimeObserver::background_tasks),
            tool_use_id: None,
            parent_permission_mode: Some(self.permission_policy.active_mode()),
            permission_sink: prompter
                .as_deref()
                .and_then(PermissionPrompter::delegation_sink),
        }
    }

    fn observe_tool_update(
        &mut self,
        update: tool_execution::ToolUpdate,
        scheduler: &mut tool_execution::ToolScheduler<'_, T>,
        iteration: usize,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) {
        match update {
            tool_execution::ToolUpdate::Started { id, name, input } => {
                self.record_tool_started(iteration, &name);
                if let Some(observer) = observer.as_deref_mut() {
                    observer.on_tool_started(&id, &name, &input);
                }
            }
            tool_execution::ToolUpdate::Completed {
                index,
                mut message,
                denial,
                input,
                context_action,
            } => {
                for block in &mut message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        tool_name,
                        output,
                        ..
                    } = block
                    {
                        if let (Some(reason), Some(observer)) =
                            (denial.as_ref(), observer.as_deref_mut())
                        {
                            observer.on_permission_denied(tool_use_id, tool_name, &input, reason);
                        }
                        // A restart's tool result is the continuation itself;
                        // offloading it would discard the approved document.
                        if context_action.is_none() {
                            *output = self.maybe_offload_tool_output(
                                tool_use_id,
                                tool_name,
                                std::mem::take(output),
                            );
                        }
                    }
                }
                notify_tool_result(runtime_observer_mut(observer), &message);
                scheduler.store_result(index, message);
            }
        }
    }

    fn checkpoint_assistant(
        &mut self,
        index: &mut Option<usize>,
        message: ConversationMessage,
    ) -> Result<(), RuntimeError> {
        if let Some(index) = *index {
            self.session.update_assistant_message(index, message)
        } else {
            let next = self.session.messages.len();
            self.session.push_message(message).map(|()| {
                *index = Some(next);
            })
        }
        .map_err(|error| RuntimeError::new(error.to_string()))
    }

    fn persist_ready_results(
        &mut self,
        scheduler: &tool_execution::ToolScheduler<'_, T>,
        next: &mut usize,
    ) -> Result<(), RuntimeError> {
        while let Some(message) = scheduler.result(*next) {
            self.session
                .push_message(message.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            *next += 1;
        }
        Ok(())
    }

    /// Poll the provider and the same tool lifecycle together. A provider error
    /// after dispatch is never retried as an empty response: retain completed
    /// outputs and close every remaining tool id before returning the error.
    #[allow(clippy::await_holding_lock)]
    pub(super) async fn execute_response(
        &mut self,
        mut stream: AssistantEventStream,
        iteration: usize,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> ExecutedResponse {
        let abort = self.hook_abort_signal.for_current_turn();
        self.tool_executor_mut().set_abort_signal(abort.clone());
        let executor = Arc::clone(&self.tool_executor);
        let executor = executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut scheduler = tool_execution::ToolScheduler::new(
            &*executor,
            self.hook_runner.clone(),
            self.permission_policy.clone(),
            abort.clone(),
            self.hook_progress_reporter.clone(),
        );
        let mut events = Vec::new();
        let mut ended = false;
        let mut error = None;
        let mut cancelled = false;
        let mut assistant_index = None;
        let mut persisted_results = 0;
        loop {
            if ended && scheduler.is_finished() {
                break;
            }
            tokio::select! {
                biased;
                () = abort.cancelled() => { cancelled = true; break; }
                update = scheduler.next(prompter), if !scheduler.is_finished() => {
                    self.observe_tool_update(update, &mut scheduler, iteration, observer);
                    if let Err(failure) = self.persist_ready_results(&scheduler, &mut persisted_results) { error = Some(failure); break; }
                }
                next = stream.next(), if !ended => match next {
                    Some(Ok(event)) => {
                        if let AssistantEvent::ToolUse { id, name, input, .. } = &event {
                            if assistant_index.is_none() {
                                scheduler.initialize_context(self.dispatch_context(observer, prompter));
                            }
                            let mut prefix = events.clone();
                            prefix.push(event.clone());
                            prefix.push(AssistantEvent::MessageStop);
                            let (mut parent, ..) = build_assistant_message(prefix).expect("complete tool block supplies content");
                            parent.model = Some(self.running_model().to_string());
                            if let Err(failure) = self.checkpoint_assistant(&mut assistant_index, parent.clone()) { error = Some(failure); break; }
                            if let Err(failure) = scheduler.enqueue(id.clone(), name.clone(), input.clone(), parent) {
                                error = Some(failure); break;
                            }
                        }
                        observe_assistant_event(observer, &event);
                        let stopped = matches!(event, AssistantEvent::MessageStop);
                        events.push(event);
                        if stopped && assistant_index.is_some() {
                            if let Ok((mut message, ..)) = build_assistant_message(events.clone()) {
                                message.model = Some(self.running_model().to_string());
                                if let Err(failure) = self.checkpoint_assistant(&mut assistant_index, message) { error = Some(failure); break; }
                            }
                        }
                    }
                    Some(Err(failure)) => { error = Some(failure); break; }
                    None => {
                        ended = true;
                        if !events.iter().any(|event| matches!(event, AssistantEvent::MessageStop)) {
                            error = Some(RuntimeError::new("assistant stream ended without a message stop event"));
                            break;
                        }
                    }
                }
            }
        }
        drop(stream);
        if cancelled || error.is_some() {
            // Latch cancellation for workers already running on the blocking
            // pool. A later turn reset cannot resurrect these invocations.
            abort.abort();
            scheduler.cancel();
            while let Some(update) = scheduler.take_update() {
                self.observe_tool_update(update, &mut scheduler, iteration, observer);
            }
            events.push(AssistantEvent::TextDelta(if cancelled {
                format!("\n\n[{INTERRUPT_MESSAGE}]")
            } else {
                "\n\n[Provider stream failed; unfinished tool calls were cancelled.]".into()
            }));
            events.push(AssistantEvent::MessageStop);
        }
        if let Err(failure) = self.persist_ready_results(&scheduler, &mut persisted_results) {
            error = Some(failure);
        }
        ExecutedResponse {
            events,
            context_action: scheduler.context_action(),
            results: scheduler.into_results(),
            assistant_index,
            persisted_results,
            error,
            cancelled,
        }
    }
}

pub(super) struct ExecutedResponse {
    pub(super) assistant_index: Option<usize>,
    pub(super) persisted_results: usize,
    pub(super) events: Vec<AssistantEvent>,
    pub(super) results: Vec<ConversationMessage>,
    pub(super) error: Option<RuntimeError>,
    pub(super) cancelled: bool,
    pub(super) context_action: Option<crate::image_input::ToolContextAction>,
}

fn observe_assistant_event(
    observer: &mut Option<&mut dyn RuntimeObserver>,
    event: &AssistantEvent,
) {
    if let Some(obs) = observer.as_deref_mut() {
        match &event {
            AssistantEvent::ThinkingStart => {}
            AssistantEvent::Thinking { thinking, .. } => {
                // A signature-only event carries
                // no text; forwarding it would
                // make renderers open a thinking
                // section with nothing in it.
                if !thinking.is_empty() {
                    obs.on_thinking_delta(thinking);
                }
            }
            // Ciphertext: there is no delta a
            // renderer could show. Printing
            // nothing, though, is
            // indistinguishable from a turn that
            // never thought — so say it once per
            // block, through the same channel the
            // thinking text uses so it lands in
            // the same dim style, and with the
            // wording the export already uses.
            // Display only: the block itself is
            // replayed from the message, not from
            // anything the observer saw.
            AssistantEvent::RedactedThinking { .. } => {
                obs.on_thinking_delta("[thinking: redacted by the provider]\n");
            }
            AssistantEvent::TextDelta(delta) => {
                obs.on_text_delta(delta);
            }
            AssistantEvent::ToolUse {
                id, name, input, ..
            } => {
                obs.on_tool_use(id, name, input);
            }
            AssistantEvent::Model(model) => {
                obs.on_model(model);
            }
            AssistantEvent::Usage(usage) => {
                obs.on_usage(usage);
            }
            AssistantEvent::PromptCache(cache_event) => {
                obs.on_prompt_cache(cache_event);
            }
            AssistantEvent::MessageStop => {
                obs.on_message_stop();
            }
        }
    }
}
