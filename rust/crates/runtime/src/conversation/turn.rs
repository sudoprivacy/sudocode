//! Turn admission, budget checks, streaming recovery and terminal accounting.
use super::{
    build_assistant_message, estimate_session_tokens, forced_compaction_config,
    has_pending_tool_uses, merge_auto_compaction, response, runtime_observer_mut,
    sum_assistant_message_usage, ApiClient, ApiRequest, AssistantEvent, AssistantEventStream,
    AutoCompactionEvent, BuiltAssistantMessage, CompactionOutcome, CompactionTrigger, ContentBlock,
    ConversationMessage, ConversationRuntime, MessageRole, PermissionPrompter, PromptCacheEvent,
    RuntimeError, RuntimeObserver, ToolExecutor, TurnSummary, EMPTY_POST_TOOL_DELIVERABLE_REMINDER,
    MAX_STREAM_CONTINUATIONS, MAX_TURN_COMPACTIONS, STREAM_INTERRUPTED_REMINDER,
};

#[derive(Default)]
struct StepProgress {
    completed: u32,
    final_requested: bool,
}
#[derive(Default)]
struct TurnRecovery {
    retried_empty_post_tool: bool,
    repaired_thinking: bool,
    stream_continuations: u32,
}
#[derive(Default)]
struct TurnCompactions {
    count: usize,
    recorded_exhausted: bool,
    overflow: Option<AutoCompactionEvent>,
}
#[derive(Default)]
struct TurnProgress {
    assistant_messages: Vec<ConversationMessage>,
    tool_results: Vec<ConversationMessage>,
    prompt_cache_events: Vec<PromptCacheEvent>,
    iterations: usize,
    response_model: Option<String>,
    steps: StepProgress,
    recovery: TurnRecovery,
    compaction: TurnCompactions,
}
enum TurnStream {
    Ready(AssistantEventStream),
    Yielded,
    Cancelled,
}
enum TurnDisposition {
    Retry,
    EmptyPostTool,
}

impl<C: ApiClient, T: ToolExecutor> ConversationRuntime<C, T> {
    pub(super) async fn run_turn_with_blocks_inner(
        &mut self,
        blocks: Vec<ContentBlock>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
        mut observer: Option<&mut dyn RuntimeObserver>,
        turn_id: String,
    ) -> Result<TurnSummary, RuntimeError> {
        let started = self
            .begin_turn_input(blocks, &mut observer, turn_id)
            .await?;
        let mut state = TurnProgress::default();
        loop {
            if self.hook_abort_signal.is_aborted() {
                self.finalize_cancelled_turn(Vec::new());
                return Ok(self.cancelled_turn_state(&mut state));
            }
            self.commit_turn_inputs(&mut observer)?;
            let final_answer = self.prepare_turn_iteration(&mut state)?;
            if !self
                .enforce_turn_request_budget(&mut state, &mut observer)
                .await?
            {
                return Ok(self.cancelled_turn_state(&mut state));
            }
            let stream = match self
                .open_turn_stream(&mut state, final_answer, &mut observer)
                .await?
            {
                TurnStream::Ready(stream) => stream,
                TurnStream::Yielded => continue,
                TurnStream::Cancelled => return Ok(self.cancelled_turn_state(&mut state)),
            };
            let mut response = self
                .execute_response(
                    stream,
                    state.iterations,
                    !final_answer,
                    &mut observer,
                    &mut prompter,
                )
                .await;
            let events = std::mem::take(&mut response.events);
            let assistant = match self.resolve_turn_assistant(
                events,
                response.yielded,
                final_answer,
                &mut state,
            )? {
                std::ops::ControlFlow::Continue(assistant) => assistant,
                std::ops::ControlFlow::Break(TurnDisposition::Retry) => continue,
                std::ops::ControlFlow::Break(TurnDisposition::EmptyPostTool) => {
                    state.response_model = None;
                    return self
                        .finish_turn_state(&mut state, None, &mut observer)
                        .await;
                }
            };
            let (exchange_start, pending_tool_count) =
                self.checkpoint_turn_response(assistant, &mut response, &mut state)?;
            if let Some(error) = response.error.take() {
                self.recover_turn_stream(
                    error,
                    response.cancelled,
                    pending_tool_count,
                    &mut state,
                    &mut observer,
                )
                .await?;
                continue;
            }
            if response.cancelled || self.hook_abort_signal.is_aborted() {
                return Ok(self.cancelled_turn_state(&mut state));
            }
            if let Some(action) = response.context_action {
                self.apply_tool_context_action(action, exchange_start, &mut observer)?;
            }
            if pending_tool_count == 0 && self.turn_inputs.seal_if_empty() {
                break;
            }
            state.steps.completed = state.steps.completed.saturating_add(1);
        }
        self.finish_turn_state(&mut state, Some(started), &mut observer)
            .await
    }

    async fn begin_turn_input(
        &mut self,
        blocks: Vec<ContentBlock>,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        turn_id: String,
    ) -> Result<std::time::Instant, RuntimeError> {
        let blocks = self.inject_date_context(blocks);
        let blocks = self.inject_model_context(blocks);
        let label = blocks
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();

        if self.session.compaction.is_some() {
            if let Err(error) = self.run_session_health_probe().await {
                return Err(RuntimeError::new(format!(
                    "Session health probe failed after compaction: {error}. \
                     The session may be in an inconsistent state. \
                     Consider starting a fresh session with /session new."
                )));
            }
        }

        let prepared_blocks = self.prepare_input_blocks(blocks)?;

        // Start file tracking for this turn
        self.current_turn_id = Some(turn_id.clone());
        self.file_tracker.start_turn(turn_id);

        // Analyze user request for file intent
        self.user_request_intent = Some(crate::file_intent::UserRequestIntent::analyze(&label));

        self.record_turn_started(&label);
        // Start a fresh per-turn usage accumulator so the status line bills the
        // whole turn (all tool-loop requests), not just the last request.
        self.usage_tracker.begin_turn();
        // Wall-clock start of the turn: recorded at turn end into the tracker
        // (the status line's `+Ns`/`Σs`) and stamped onto the turn's last
        // assistant message so it persists and re-sums on resume.
        let turn_started_at = std::time::Instant::now();
        // Match CC's resume sentinel: an unanswered prompt or tool result must
        // not merge with a new user turn. Append it only at this outer turn
        // boundary, never between a tool result and its in-flight continuation.
        if self
            .session
            .messages
            .iter()
            .rev()
            .find(|message| message.role != MessageRole::System)
            .is_some_and(|message| matches!(message.role, MessageRole::User | MessageRole::Tool))
        {
            self.session
                .push_message(ConversationMessage::unanswered_turn_boundary())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
        }
        self.session
            .push_user_blocks(prepared_blocks)
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        if let Some(observer) = observer.as_deref_mut() {
            observer.on_prompt_committed();
        }
        self.compaction_pending_input_tokens = 0;

        // Hook workers share the current observer's progress sink, so slow
        // hooks never hold the event loop while reporting lifecycle changes.
        if let Some(sink) = observer
            .as_deref()
            .and_then(RuntimeObserver::hook_progress_sink)
        {
            self.hook_progress_reporter = Some(sink);
        }

        // Retry reporting follows the observer, so it is set unconditionally —
        // passing `None` when this observer wants none is what stops a previous
        // turn's renderer from still receiving events.
        self.api_client
            .set_retry_sink(observer.as_deref().and_then(RuntimeObserver::retry_sink));

        Ok(turn_started_at)
    }

    fn commit_turn_inputs(
        &mut self,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<(), RuntimeError> {
        for input in self.turn_inputs.batch() {
            if self.hook_abort_signal.is_aborted() {
                break;
            }
            let view = input.view();
            // Resolve human references through the session filesystem;
            // peer and task notifications keep their literal text.
            let prepared = if input.source == crate::turn_input::InputSource::Human {
                crate::image_input::prompt_blocks(input.text, self.session.fs_handle().as_ref())
                    .map_err(|error| RuntimeError::new(error.to_string()))
                    .and_then(|mut blocks| {
                        blocks.extend(input.attachments);
                        self.prepare_input_blocks(blocks)
                    })
            } else {
                Ok(vec![ContentBlock::Text { text: input.text }])
            };
            // Preparation can perform I/O; cancellation still owns admission.
            if self.hook_abort_signal.is_aborted() {
                break;
            }
            self.session
                .push_user_blocks(prepared?)
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            self.turn_inputs.committed(input.id);
            if let Some(observer) = observer.as_deref_mut() {
                observer.on_input_committed(&view);
            }
        }
        Ok(())
    }

    fn prepare_turn_iteration(&mut self, state: &mut TurnProgress) -> Result<bool, RuntimeError> {
        let final_answer = self
            .max_steps
            .is_some_and(|limit| state.steps.completed >= limit);
        if final_answer && !state.steps.final_requested {
            let completed_steps = state.steps.completed;
            self.session.push_user_text(format!(
                    "<system-reminder>\nThe computation step limit ({completed_steps} steps) has been reached. No further tools or execution steps are available in this turn. Answer the user's question using the results already obtained. Clearly distinguish completed work, findings, and remaining uncertainties. Explain any unfinished work and suggest concrete next steps for discussion with the user. Do not claim unperformed work is complete. Respond in the user's language.\n</system-reminder>"
                )).map_err(|error| RuntimeError::new(error.to_string()))?;
            state.steps.final_requested = true;
        }
        state.iterations += 1;
        if state.iterations > self.max_iterations {
            let error =
                RuntimeError::new("conversation loop exceeded the maximum number of iterations");
            self.record_turn_failed(state.iterations, &error);
            return Err(error);
        }

        Ok(final_answer)
    }

    async fn enforce_turn_request_budget(
        &mut self,
        state: &mut TurnProgress,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<bool, RuntimeError> {
        // Compact before dispatching, not after the provider rejects.
        // A turn that takes many tool-call steps grows its own history
        // while it runs; the per-turn preflight ran once, before any of
        // those steps existed. Without this the first the runtime hears
        // of the overflow is a rejection, and the single salvage below
        // is all a long turn ever gets.
        while self.next_request_exceeds_budget() {
            if state.compaction.count < MAX_TURN_COMPACTIONS {
                let attempt = self
                    .compact_in_place(
                        forced_compaction_config(),
                        CompactionTrigger::InTurnBudget,
                        runtime_observer_mut(observer),
                    )
                    .await;
                if self.hook_abort_signal.is_aborted()
                    && !(attempt.is_err()
                        && self
                            .last_compaction_report
                            .as_ref()
                            .is_some_and(|report| report.outcome == CompactionOutcome::Failed))
                {
                    return Ok(false);
                }
                if let Some(event) =
                    attempt.map_err(|error| RuntimeError::new(error.to_string()))?
                {
                    state.compaction.overflow =
                        merge_auto_compaction(state.compaction.overflow.take(), Some(event));
                } else {
                    return Err(RuntimeError::context_window_blocked(
                            "Context cannot be compacted safely; history preserved. No model request sent."));
                }
                state.compaction.count += 1;
            } else if !state.compaction.recorded_exhausted {
                self.record_compaction_budget_exhausted(CompactionTrigger::InTurnBudget);
                state.compaction.recorded_exhausted = true;
            }
            if state.compaction.count >= MAX_TURN_COMPACTIONS && self.next_request_exceeds_budget()
            {
                return Err(RuntimeError::context_window_blocked(
                    "Compaction retry budget exhausted; history preserved. No model request sent.",
                ));
            }
        }

        let (budget, _) = self.safe_history_budget(&self.session);
        if !budget.fits_buffered(estimate_session_tokens(&self.session)) {
            return Err(RuntimeError::context_window_blocked(
                    "Context remains too large after compaction; history preserved. No model request sent."));
        }

        Ok(true)
    }

    async fn open_turn_stream(
        &mut self,
        state: &mut TurnProgress,
        final_answer: bool,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<TurnStream, RuntimeError> {
        loop {
            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
                trace_id: self.trace_id.clone(),
                pre_compact_discovered_tools: self
                    .session
                    .compaction
                    .as_ref()
                    .map(|c| c.pre_compact_discovered_tools.clone())
                    .unwrap_or_default(),
            };
            // Race the API stream (which includes the retry loop) against
            // the abort signal so ESC/Ctrl-C cancels even during retries.
            let stream_result = {
                let abort = &self.hook_abort_signal;
                let inbox = self.turn_inputs.clone();
                tokio::select! {
                    biased;
                    () = abort.cancelled() => {
                        self.finalize_cancelled_turn(Vec::new());
                        return Ok(TurnStream::Cancelled);
                    }
                    () = inbox.ready() => return Ok(TurnStream::Yielded),
                    result = async {
                        if final_answer {
                            self.api_client.stream_final_answer(request).await
                        } else {
                            self.api_client.stream(request).await
                        }
                    } => result,
                }
            };
            let stream = match stream_result {
                Ok(stream) => stream,
                Err(error) => {
                    if error.is_thinking_replay_rejected() && !state.recovery.repaired_thinking {
                        state.recovery.repaired_thinking = true;
                        if self.repair_rejected_thinking()? > 0 {
                            if let Some(observer) = observer.as_deref_mut() {
                                observer.on_notice("Archived session history and removed rejected thinking; retrying once.");
                            }
                            continue;
                        }
                    }
                    // A context-window rejection is the one request failure
                    // the runtime can fix on its own: compact the history and
                    // resend. Shares the turn's compaction allowance with the
                    // proactive check above; when compaction can no longer
                    // remove anything the error is real and must surface.
                    if error.is_context_window_blocked()
                        && state.compaction.count < MAX_TURN_COMPACTIONS
                    {
                        let attempt = self
                            .compact_in_place(
                                forced_compaction_config(),
                                CompactionTrigger::ProviderRejection,
                                runtime_observer_mut(observer),
                            )
                            .await;
                        if self.hook_abort_signal.is_aborted()
                            && !(attempt.is_err()
                                && self.last_compaction_report.as_ref().is_some_and(|report| {
                                    report.outcome == CompactionOutcome::Failed
                                }))
                        {
                            return Ok(TurnStream::Cancelled);
                        }
                        if let Some(event) =
                            attempt.map_err(|error| RuntimeError::new(error.to_string()))?
                        {
                            state.compaction.count += 1;
                            state.compaction.overflow = merge_auto_compaction(
                                state.compaction.overflow.take(),
                                Some(event),
                            );
                            return Ok(TurnStream::Yielded);
                        }
                    }
                    self.record_turn_failed(state.iterations, &error);
                    return Err(error);
                }
            };
            return Ok(TurnStream::Ready(stream));
        }
    }

    fn resolve_turn_assistant(
        &mut self,
        events: Vec<AssistantEvent>,
        yielded: bool,
        final_answer: bool,
        state: &mut TurnProgress,
    ) -> Result<std::ops::ControlFlow<TurnDisposition, BuiltAssistantMessage>, RuntimeError> {
        match build_assistant_message(events) {
            Ok(result) => Ok(std::ops::ControlFlow::Continue(result)),
            Err(error) if yielded && error.message == "assistant stream produced no content" => {
                Ok(std::ops::ControlFlow::Break(TurnDisposition::Retry))
            }
            Err(error) if final_answer => {
                self.record_turn_failed(state.iterations, &error);
                Err(error)
            }
            Err(error)
                if state
                    .assistant_messages
                    .last()
                    .is_some_and(has_pending_tool_uses)
                    && error.message == "assistant stream produced no content" =>
            {
                if self.turn_inputs.has_pending() {
                    return Ok(std::ops::ControlFlow::Break(TurnDisposition::Retry));
                }
                self.record_empty_post_tool_completion(state.iterations);
                let has_unfinished_deliverable = self
                    .has_unfinished_requested_deliverable_after_tool_empty(
                        &state.assistant_messages,
                        &state.tool_results,
                    );
                if has_unfinished_deliverable && !state.recovery.retried_empty_post_tool {
                    state.recovery.retried_empty_post_tool = true;
                    self.session
                        .push_user_text(EMPTY_POST_TOOL_DELIVERABLE_REMINDER)
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    return Ok(std::ops::ControlFlow::Break(TurnDisposition::Retry));
                }
                if has_unfinished_deliverable {
                    let error = RuntimeError::new(
                                "model returned an empty response after tool use before producing the requested file deliverable",
                            );
                    self.record_turn_failed(state.iterations, &error);
                    return Err(error);
                }
                if !self.turn_inputs.seal_if_empty() {
                    return Ok(std::ops::ControlFlow::Break(TurnDisposition::Retry));
                }
                Ok(std::ops::ControlFlow::Break(TurnDisposition::EmptyPostTool))
            }
            Err(error) => {
                self.record_turn_failed(state.iterations, &error);
                Err(error)
            }
        }
    }

    fn checkpoint_turn_response(
        &mut self,
        assistant: BuiltAssistantMessage,
        response: &mut response::ExecutedResponse,
        state: &mut TurnProgress,
    ) -> Result<(usize, usize), RuntimeError> {
        let BuiltAssistantMessage {
            message: mut assistant_message,
            usage,
            prompt_cache_events: turn_prompt_cache_events,
            response_model: iter_response_model,
        } = assistant;
        state.response_model = iter_response_model;
        assistant_message.model = {
            let running = self.running_model();
            (!running.is_empty()).then(|| running.to_string())
        };
        if let Some(usage) = usage {
            self.usage_tracker.record(usage);
        }
        state.prompt_cache_events.extend(turn_prompt_cache_events);
        let pending_tool_uses = assistant_message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse {
                    id, name, input, ..
                } => Some((id.clone(), name.clone(), input.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.record_assistant_iteration(
            state.iterations,
            &assistant_message,
            pending_tool_uses.len(),
        );

        let exchange_start = response
            .assistant_index
            .unwrap_or(self.session.messages.len());
        match response.assistant_index {
            Some(index) => self
                .session
                .update_assistant_message(index, assistant_message.clone()),
            None => self.session.push_message(assistant_message.clone()),
        }
        .map_err(|error| RuntimeError::new(error.to_string()))?;
        state.assistant_messages.push(assistant_message.clone());

        for (index, message) in std::mem::take(&mut response.results)
            .into_iter()
            .enumerate()
        {
            if index >= response.persisted_results {
                self.session
                    .push_message(message.clone())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
            }
            self.record_tool_finished(state.iterations, &message);
            state.tool_results.push(message);
        }
        Ok((exchange_start, pending_tool_uses.len()))
    }

    async fn recover_turn_stream(
        &mut self,
        error: RuntimeError,
        cancelled: bool,
        pending_tool_count: usize,
        state: &mut TurnProgress,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<(), RuntimeError> {
        if let Some(path) = self.session.persistence_path() {
            self.session.save_to_path(path).map_err(|failure| {
                RuntimeError::new(format!(
                    "failed to persist interrupted tools: {failure}; provider error: {error}"
                ))
            })?;
        }
        // The partial reply and every tool result are saved above, so
        // continue from them as a user's "continue" would. Never resend
        // the failed request: its output, usage and tools already ran.
        if error.is_stream_interrupted()
            && state.recovery.stream_continuations < MAX_STREAM_CONTINUATIONS
            && !cancelled
        {
            state.recovery.stream_continuations += 1;
            // The failed response latched the shared abort flag to stop
            // its unfinished tools. Those workers hold the old turn
            // generation and stay cancelled; clear the flag so the
            // continuation runs and a later Esc still stops it.
            self.hook_abort_signal.reset();
            let stream_continuations = state.recovery.stream_continuations;
            if let Some(observer) = observer.as_deref_mut() {
                // The interrupted reply never received its stop event.
                // Close it for observers as it is saved, so the
                // continuation is a separate message.
                observer.on_message_stop();
                observer.on_notice(&format!(
                            "Model stream dropped mid-response; continuing ({stream_continuations}/{MAX_STREAM_CONTINUATIONS})."
                        ));
            }
            for _ in 0..10 {
                if self.hook_abort_signal.is_aborted() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if pending_tool_count == 0 {
                self.session
                    .push_user_text(STREAM_INTERRUPTED_REMINDER)
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
            }
            return Ok(());
        }
        self.record_turn_failed(state.iterations, &error);
        Err(error)
    }

    fn cancelled_turn_state(&mut self, state: &mut TurnProgress) -> TurnSummary {
        self.cancelled_summary(
            std::mem::take(&mut state.assistant_messages),
            std::mem::take(&mut state.tool_results),
            std::mem::take(&mut state.prompt_cache_events),
            state.iterations,
        )
    }

    async fn finish_turn_state(
        &mut self,
        state: &mut TurnProgress,
        turn_started_at: Option<std::time::Instant>,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<TurnSummary, RuntimeError> {
        let attempt = self
            .maybe_auto_compact(runtime_observer_mut(observer))
            .await;
        if self.hook_abort_signal.is_aborted()
            && !(attempt.is_err()
                && self
                    .last_compaction_report
                    .as_ref()
                    .is_some_and(|report| report.outcome == CompactionOutcome::Failed))
        {
            return Ok(self.cancelled_turn_state(state));
        }
        let auto_compaction = match attempt {
            Ok(event) => merge_auto_compaction(state.compaction.overflow.take(), event),
            Err(error) => {
                self.record_turn_failed(state.iterations, &error);
                return Err(error);
            }
        };

        self.finish_current_turn_tracking();

        if let Some(turn_started_at) = turn_started_at {
            // Record the turn's wall-clock time: into the tracker (status line) and
            // stamped onto the last assistant message in the persisted session, so
            // it survives to resume and re-sums into the cumulative total.
            let turn_duration_ms =
                u64::try_from(turn_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            self.usage_tracker.record_turn_duration(turn_duration_ms);
            self.session
                .record_turn_duration(turn_duration_ms)
                .map_err(|error| {
                    RuntimeError::new(format!("failed to persist turn duration: {error}"))
                })?;
        }

        let turn_usage = sum_assistant_message_usage(&state.assistant_messages);
        let session_usage = self.usage_tracker.cumulative_usage();
        let summary = TurnSummary {
            assistant_messages: std::mem::take(&mut state.assistant_messages),
            tool_results: std::mem::take(&mut state.tool_results),
            prompt_cache_events: std::mem::take(&mut state.prompt_cache_events),
            iterations: state.iterations,
            turn_usage,
            session_usage,
            auto_compaction,
            cancelled: false,
            response_model: state.response_model.take(),
        };
        self.record_turn_completed(&summary);

        Ok(summary)
    }
}
