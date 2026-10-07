//! A single incremental scheduler for streamed and already-collected calls.
//! Hooks prepare independently. Admission stays ordered so a hook that changes
//! a read into a write fences every later call, including already-prepared ones.

use std::collections::VecDeque;
use std::sync::Arc;

use futures::{future::LocalBoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};

use super::{
    format_hook_message, interrupted_tool_output, max_tool_use_concurrency, merge_hook_feedback,
    ConversationMessage, HookAbortSignal, HookProgressSink, HookRunResult, HookRunner,
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter, RuntimeError,
    SinkHookReporter, ToolDispatchContext, ToolError, ToolExecutor,
};
use crate::{
    hooks::HookEvent,
    image_input::{ToolContextAction, ToolOutput},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Queued,
    Preparing,
    Prepared,
    Authorizing,
    Authorized,
    Running,
    Finishing,
    Complete,
}

struct Invocation {
    id: String,
    name: String,
    input: String,
    parent: Option<ConversationMessage>,
    parallel: bool,
    phase: Phase,
    pre_hook: HookRunResult,
    output: Option<ToolOutput>,
    is_error: bool,
    result: Option<ConversationMessage>,
}

enum Stage {
    Prepared(HookRunResult),
    Authorized(PermissionOutcome),
    Executed(Result<ToolOutput, ToolError>),
    Finished(HookRunResult),
}

pub(super) enum ToolUpdate {
    Started {
        id: String,
        name: String,
        input: String,
    },
    Completed {
        index: usize,
        message: ConversationMessage,
        denial: Option<String>,
        input: String,
        context_action: Option<ToolContextAction>,
    },
}

/// Owns one assistant response's tool lifetimes. The runtime remains the only
/// owner of transcript mutation; completion is observable before durable commit.
pub(super) struct ToolScheduler<'a, T> {
    executor: &'a T,
    calls: Vec<Invocation>,
    active: FuturesUnordered<LocalBoxFuture<'a, (usize, Stage)>>,
    updates: VecDeque<ToolUpdate>,
    hooks: Arc<HookRunner>,
    policy: PermissionPolicy,
    abort: HookAbortSignal,
    progress: Option<HookProgressSink>,
    limit: usize,
    context: Option<ToolDispatchContext>,
    context_action: Option<ToolContextAction>,
}

impl<'a, T: ToolExecutor> ToolScheduler<'a, T> {
    pub(super) fn new(
        executor: &'a T,
        hooks: HookRunner,
        policy: PermissionPolicy,
        abort: HookAbortSignal,
        progress: Option<HookProgressSink>,
    ) -> Self {
        Self {
            executor,
            calls: Vec::new(),
            active: FuturesUnordered::new(),
            updates: VecDeque::new(),
            hooks: Arc::new(hooks),
            policy,
            abort,
            progress,
            limit: max_tool_use_concurrency(),
            context: None,
            context_action: None,
        }
    }

    /// Pure text responses never need a cloned tool history. Capture it once,
    /// immediately before checkpointing the first streamed call.
    pub(super) fn initialize_context(&mut self, context: ToolDispatchContext) {
        debug_assert!(self.context.is_none());
        self.context = Some(context);
    }

    pub(super) fn enqueue(
        &mut self,
        id: String,
        name: String,
        input: String,
        parent: ConversationMessage,
    ) -> Result<(), RuntimeError> {
        if self.calls.iter().any(|call| call.id == id) {
            return Err(RuntimeError::new(format!(
                "duplicate tool_use id in assistant response: {id}"
            )));
        }
        self.calls.push(Invocation {
            parallel: self.executor.is_concurrency_safe(&name, &input),
            id,
            name,
            input,
            parent: Some(parent),
            phase: Phase::Queued,
            pre_hook: HookRunResult::allow(Vec::new()),
            output: None,
            is_error: false,
            result: None,
        });
        Ok(())
    }

    pub(super) fn is_finished(&self) -> bool {
        self.calls.iter().all(|call| call.phase == Phase::Complete) && self.updates.is_empty()
    }

    /// Taking this future out of a select does not lose work: all stage futures
    /// and state transitions live in the scheduler, never in the polling frame.
    pub(super) async fn next(
        &mut self,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> ToolUpdate {
        loop {
            if !self.abort.is_aborted() {
                self.admit(prompter);
            }
            if let Some(update) = self.updates.pop_front() {
                return update;
            }
            if self.active.is_empty() {
                std::future::pending::<()>().await;
            }
            if let Some((index, stage)) = self.active.next().await {
                self.advance(index, stage);
            }
        }
    }

    fn admit(&mut self, prompter: &mut Option<&mut dyn PermissionPrompter>) {
        let mut inflight = self
            .calls
            .iter()
            .filter(|call| !matches!(call.phase, Phase::Queued | Phase::Complete))
            .count();
        let mut earlier_pending = false;
        let mut earlier_unclassified = false;
        for index in 0..self.calls.len() {
            let call = &mut self.calls[index];
            if call.phase == Phase::Complete {
                continue;
            }
            if !call.parallel && earlier_pending {
                break;
            }
            if call.phase == Phase::Queued {
                if inflight >= self.limit {
                    break;
                }
                call.phase = Phase::Preparing;
                inflight += 1;
                let hook = run_hook(
                    self.hooks.clone(),
                    HookEvent::PreToolUse,
                    call.name.clone(),
                    call.input.clone(),
                    None,
                    self.abort.clone(),
                    self.progress.clone(),
                );
                self.active
                    .push(async move { (index, Stage::Prepared(hook.await)) }.boxed_local());
            }
            if matches!(call.phase, Phase::Queued | Phase::Preparing) {
                earlier_unclassified = true;
            } else if !earlier_unclassified {
                if call.phase == Phase::Prepared {
                    let context = PermissionContext::new(
                        call.pre_hook.permission_override(),
                        call.pre_hook.permission_reason().map(ToOwned::to_owned),
                    );
                    let reply = self.policy.begin_authorization(
                        &call.name,
                        &call.input,
                        &context,
                        prompter
                            .as_mut()
                            .map(|p| &mut **p as &mut dyn PermissionPrompter),
                    );
                    call.phase = Phase::Authorizing;
                    self.active
                        .push(async move { (index, Stage::Authorized(reply.await)) }.boxed_local());
                } else if call.phase == Phase::Authorized {
                    call.phase = Phase::Running;
                    let executor = self.executor;
                    let name = call.name.clone();
                    let input = call.input.clone();
                    // Only executing calls clone session history; queued calls
                    // retain their assistant prefix, not another full session.
                    let mut context = self
                        .context
                        .as_ref()
                        .expect("context before first call")
                        .clone();
                    context.tool_use_id = Some(call.id.clone());
                    let parent = call.parent.take().expect("undispatched parent");
                    context.parent_session_messages.push(parent.clone());
                    context.parent_assistant_message = Some(parent);
                    self.updates.push_back(ToolUpdate::Started {
                        id: call.id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                    self.active.push(
                        async move {
                            (
                                index,
                                Stage::Executed(
                                    executor
                                        .execute_with_attachments(&name, &input, &context)
                                        .await,
                                ),
                            )
                        }
                        .boxed_local(),
                    );
                }
            }
            earlier_pending = true;
            if !call.parallel {
                break;
            }
        }
    }

    fn advance(&mut self, index: usize, stage: Stage) {
        let call = &mut self.calls[index];
        match stage {
            Stage::Prepared(hook) => {
                if let Some(input) = hook.updated_input() {
                    call.input = input.into();
                }
                call.parallel = self.executor.is_concurrency_safe(&call.name, &call.input);
                call.phase = Phase::Prepared;
                let denial = if hook.is_cancelled() || hook.is_failed() || hook.is_denied() {
                    let action = if hook.is_cancelled() {
                        "cancelled"
                    } else if hook.is_failed() {
                        "failed for"
                    } else {
                        "denied"
                    };
                    Some(format_hook_message(
                        &hook,
                        &format!("PreToolUse hook {action} tool `{}`", call.name),
                    ))
                } else {
                    None
                };
                call.pre_hook = hook;
                if let Some(reason) = denial {
                    self.complete(index, ToolOutput::text(reason.clone()), true, Some(reason));
                }
            }
            Stage::Authorized(PermissionOutcome::Allow) => call.phase = Phase::Authorized,
            Stage::Authorized(PermissionOutcome::Deny { reason }) => {
                self.complete(index, ToolOutput::text(reason.clone()), true, Some(reason));
            }
            Stage::Executed(result) => {
                let (mut output, is_error) = match result {
                    Ok(output) => (output, false),
                    Err(error) => (ToolOutput::text(error.to_string()), true),
                };
                output.text = merge_hook_feedback(call.pre_hook.messages(), output.text, false);
                call.is_error = is_error || self.abort.is_aborted();
                let event = if is_error {
                    HookEvent::PostToolUseFailure
                } else {
                    HookEvent::PostToolUse
                };
                let hook = run_hook(
                    self.hooks.clone(),
                    event,
                    call.name.clone(),
                    call.input.clone(),
                    self.hooks
                        .has_tool_hooks(event)
                        .then(|| output.text.clone()),
                    self.abort.clone(),
                    self.progress.clone(),
                );
                call.output = Some(output);
                call.phase = Phase::Finishing;
                self.active
                    .push(async move { (index, Stage::Finished(hook.await)) }.boxed_local());
            }
            Stage::Finished(hook) => {
                let mut output = call.output.take().expect("executed output");
                let failed = hook.is_denied() || hook.is_failed() || hook.is_cancelled();
                output.text = merge_hook_feedback(hook.messages(), output.text, failed);
                let is_error = call.is_error || failed;
                self.complete(index, output, is_error, None);
            }
        }
    }

    fn complete(
        &mut self,
        index: usize,
        mut output: ToolOutput,
        is_error: bool,
        denial: Option<String>,
    ) {
        let call = &mut self.calls[index];
        output.text = if denial.is_some() {
            merge_hook_feedback(call.pre_hook.messages(), output.text, true)
        } else {
            output.text
        };
        let context_action = if is_error {
            None
        } else {
            output.context_action.take()
        };
        if context_action.is_some() {
            self.context_action = context_action;
        }
        let message = output.into_message(call.id.clone(), call.name.clone(), is_error);
        call.phase = Phase::Complete;
        call.parent = None;
        self.updates.push_back(ToolUpdate::Completed {
            index,
            message,
            denial,
            input: call.input.clone(),
            context_action,
        });
    }

    pub(super) fn result(&self, index: usize) -> Option<&ConversationMessage> {
        self.calls.get(index).and_then(|call| call.result.as_ref())
    }

    pub(super) fn store_result(&mut self, index: usize, message: ConversationMessage) {
        self.calls[index].result = Some(message);
    }

    /// Preserve completed outputs, and answer each unfinished id exactly once.
    /// Dropping stage futures releases network and interaction waits; latched
    /// cancellation also stops blocking hook/tool workers after stream failure.
    pub(super) fn cancel(&mut self) {
        self.active.clear();
        for index in 0..self.calls.len() {
            let call = &mut self.calls[index];
            if call.phase != Phase::Complete {
                // The command may have finished while its post hook was still
                // pending. Retain its output when cancelling that hook.
                let output = call
                    .output
                    .take()
                    .unwrap_or_else(|| ToolOutput::text(interrupted_tool_output(&call.name)));
                self.complete(index, output, true, None);
            }
        }
    }

    pub(super) fn take_update(&mut self) -> Option<ToolUpdate> {
        self.updates.pop_front()
    }
    pub(super) fn context_action(&self) -> Option<ToolContextAction> {
        self.context_action
    }
    pub(super) fn into_results(mut self) -> Vec<ConversationMessage> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .map(|call| call.result.expect("every tool id has a result"))
            .collect()
    }
}

impl<T> Drop for ToolScheduler<'_, T> {
    fn drop(&mut self) {
        if self.calls.iter().any(|call| call.phase != Phase::Complete) {
            self.abort.abort();
        }
    }
}

async fn run_hook(
    hooks: Arc<HookRunner>,
    event: HookEvent,
    name: String,
    input: String,
    output: Option<String>,
    abort: HookAbortSignal,
    progress: Option<HookProgressSink>,
) -> HookRunResult {
    if !hooks.has_tool_hooks(event) {
        return HookRunResult::allow(Vec::new());
    }
    let workspace = crate::WorkspaceRootHandoff::capture();
    tokio::task::spawn_blocking(move || {
        let _workspace = workspace.enter();
        let mut reporter = progress.map(SinkHookReporter);
        let reporter = reporter
            .as_mut()
            .map(|r| r as &mut dyn super::HookProgressReporter);
        match event {
            HookEvent::PreToolUse => {
                hooks.run_pre_tool_use_with_context(&name, &input, Some(&abort), reporter)
            }
            HookEvent::PostToolUse => hooks.run_post_tool_use_with_context(
                &name,
                &input,
                output.as_deref().unwrap_or_default(),
                false,
                Some(&abort),
                reporter,
            ),
            HookEvent::PostToolUseFailure => hooks.run_post_tool_use_failure_with_context(
                &name,
                &input,
                output.as_deref().unwrap_or_default(),
                Some(&abort),
                reporter,
            ),
        }
    })
    .await
    .unwrap_or_else(|error| HookRunResult::failed(format!("hook worker failed: {error}")))
}
