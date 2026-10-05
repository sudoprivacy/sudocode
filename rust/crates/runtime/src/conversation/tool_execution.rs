//! One dispatch path for serial and concurrent calls. Execution completion is
//! visible immediately; transcript results retain the provider's call order.

use futures::{stream::FuturesUnordered, StreamExt};

use super::{
    format_hook_message, interrupted_tool_output, max_tool_use_concurrency, merge_hook_feedback,
    notify_tool_result, runtime_observer_mut, ApiClient, ConversationMessage, ConversationRuntime,
    HookRunResult, PermissionContext, PermissionOutcome, PermissionPrompter, RuntimeError,
    RuntimeObserver, ToolDispatchContext, ToolError, ToolExecutor,
};

pub(super) struct ToolBatch<'a> {
    pub calls: &'a [(String, String, String)],
    pub context: &'a ToolDispatchContext,
    pub iteration: usize,
}

struct PreparedTool {
    index: usize,
    id: String,
    name: String,
    input: String,
    pre_hook: HookRunResult,
    denial: Option<String>,
}

impl<C: ApiClient, T: ToolExecutor> ConversationRuntime<C, T> {
    /// Returns true after cancellation, with every requested id answered.
    pub(super) async fn execute_tool_calls(
        &mut self,
        executor: &T,
        batch: ToolBatch<'_>,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
        tool_results: &mut Vec<ConversationMessage>,
    ) -> Result<bool, RuntimeError> {
        let ToolBatch {
            calls,
            context,
            iteration,
        } = batch;
        let mut next = 0;
        let mut carry = None;
        let mut results = vec![None; calls.len()];
        let mut committed = 0;
        while next < calls.len() || carry.is_some() {
            if self.hook_abort_signal.is_aborted() {
                break;
            }
            let first = carry.take().unwrap_or_else(|| {
                let prepared = self.prepare_tool(next, &calls[next], observer, prompter);
                next += 1;
                prepared
            });
            let parallel = executor.is_concurrency_safe(&first.name, &first.input);
            let mut batch = vec![first];
            while parallel && next < calls.len() {
                let (_, name, input) = &calls[next];
                if !executor.is_concurrency_safe(name, input) || self.hook_abort_signal.is_aborted()
                {
                    break;
                }
                let prepared = self.prepare_tool(next, &calls[next], observer, prompter);
                next += 1;
                // A hook may turn a read into a writer. Keep that invocation as
                // the next barrier; never execute its hook a second time.
                if !executor.is_concurrency_safe(&prepared.name, &prepared.input) {
                    carry = Some(prepared);
                    break;
                }
                batch.push(prepared);
            }

            let limit = if parallel {
                max_tool_use_concurrency()
            } else {
                1
            };
            let mut waiting = batch.into_iter();
            let mut active = FuturesUnordered::new();
            loop {
                while active.len() < limit && !self.hook_abort_signal.is_aborted() {
                    let Some(prepared) = waiting.next() else {
                        break;
                    };
                    if prepared.denial.is_some() {
                        let index = prepared.index;
                        let message = self.finish_tool(prepared, None);
                        notify_tool_result(runtime_observer_mut(observer), &message);
                        results[index] = Some(message);
                        continue;
                    }
                    self.record_tool_started(iteration, &prepared.name);
                    if let Some(observer) = observer.as_deref_mut() {
                        observer.on_tool_started(&prepared.id, &prepared.name, &prepared.input);
                    }
                    let mut context = context.clone();
                    context.tool_use_id = Some(prepared.id.clone());
                    active.push(async move {
                        let result = executor
                            .execute_with_attachments(&prepared.name, &prepared.input, &context)
                            .await;
                        (prepared, result)
                    });
                }
                self.commit_tool_results(&mut results, &mut committed, iteration, tool_results)?;
                if active.is_empty() || self.hook_abort_signal.is_aborted() {
                    break;
                }
                let abort = self.hook_abort_signal.clone();
                let completed = tokio::select! {
                    biased;
                    () = abort.cancelled() => None,
                    result = active.next() => result,
                };
                let Some((prepared, result)) = completed else {
                    break;
                };
                let index = prepared.index;
                let message = self.finish_tool(prepared, Some(result));
                // Display completed siblings now, not after the slowest one.
                // Durable ordering is handled separately by the shared commit.
                notify_tool_result(runtime_observer_mut(observer), &message);
                results[index] = Some(message);
            }
            drop(active);
            self.commit_tool_results(&mut results, &mut committed, iteration, tool_results)?;
        }

        let cancelled = self.hook_abort_signal.is_aborted();
        if cancelled {
            fill_interrupted_results(calls, &mut results, committed, observer);
        }
        self.commit_tool_results(&mut results, &mut committed, iteration, tool_results)?;
        debug_assert_eq!(committed, calls.len(), "every requested id needs a result");
        Ok(cancelled)
    }

    fn prepare_tool(
        &mut self,
        index: usize,
        call: &(String, String, String),
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> PreparedTool {
        let (id, name, input) = call;
        let pre_hook = self.run_pre_tool_use_hook(name, input);
        let input = pre_hook.updated_input().unwrap_or(input).to_string();
        let context = PermissionContext::new(
            pre_hook.permission_override(),
            pre_hook.permission_reason().map(ToOwned::to_owned),
        );
        let denied = pre_hook.is_cancelled() || pre_hook.is_failed() || pre_hook.is_denied();
        let outcome = if denied {
            let action = if pre_hook.is_cancelled() {
                "cancelled"
            } else if pre_hook.is_failed() {
                "failed for"
            } else {
                "denied"
            };
            PermissionOutcome::Deny {
                reason: format_hook_message(
                    &pre_hook,
                    &format!("PreToolUse hook {action} tool `{name}`"),
                ),
            }
        } else if let Some(prompt) = prompter.as_mut() {
            self.permission_policy
                .authorize_with_context(name, &input, &context, Some(*prompt))
        } else {
            self.permission_policy
                .authorize_with_context(name, &input, &context, None)
        };
        let denial = match outcome {
            PermissionOutcome::Allow => None,
            PermissionOutcome::Deny { reason } => {
                if let Some(observer) = observer.as_deref_mut() {
                    observer.on_permission_denied(id, name, &input, &reason);
                }
                Some(reason)
            }
        };
        PreparedTool {
            index,
            id: id.clone(),
            name: name.clone(),
            input,
            pre_hook,
            denial,
        }
    }

    fn finish_tool(
        &mut self,
        prepared: PreparedTool,
        result: Option<Result<crate::image_input::ToolOutput, ToolError>>,
    ) -> ConversationMessage {
        let PreparedTool {
            id,
            name,
            input,
            pre_hook,
            denial,
            ..
        } = prepared;
        if let Some(reason) = denial {
            return ConversationMessage::tool_result(
                id,
                name,
                merge_hook_feedback(pre_hook.messages(), reason, true),
                true,
            );
        }
        let (output, attachments, mut is_error) = match result.expect("authorized tool result") {
            Ok(output) => (output.text, output.attachments, false),
            Err(error) => (error.to_string(), Vec::new(), true),
        };
        let mut output = merge_hook_feedback(pre_hook.messages(), output, false);
        if self.hook_abort_signal.is_aborted() {
            is_error = true;
        } else {
            let post_hook = if is_error {
                self.run_post_tool_use_failure_hook(&name, &input, &output)
            } else {
                self.run_post_tool_use_hook(&name, &input, &output, false)
            };
            let failed = post_hook.is_denied() || post_hook.is_failed() || post_hook.is_cancelled();
            is_error |= failed;
            output = merge_hook_feedback(post_hook.messages(), output, failed);
        }
        let output = self.maybe_offload_tool_output(&id, &name, output);
        crate::image_input::ToolOutput {
            text: output,
            attachments,
        }
        .into_message(id, name, is_error)
    }

    fn commit_tool_results(
        &mut self,
        results: &mut [Option<ConversationMessage>],
        committed: &mut usize,
        iteration: usize,
        tool_results: &mut Vec<ConversationMessage>,
    ) -> Result<(), RuntimeError> {
        while let Some(slot) = results.get_mut(*committed) {
            let Some(message) = slot.take() else { break };
            // The completion was already published. Only persistence/tracing
            // happens here, once, in the original provider call order.
            self.push_tool_result_message(&mut None, iteration, tool_results, message)?;
            *committed += 1;
        }
        Ok(())
    }
}

fn fill_interrupted_results(
    calls: &[(String, String, String)],
    results: &mut [Option<ConversationMessage>],
    committed: usize,
    observer: &mut Option<&mut dyn RuntimeObserver>,
) {
    for ((id, name, _), slot) in calls[committed..].iter().zip(&mut results[committed..]) {
        if slot.is_none() {
            let message = ConversationMessage::tool_result(
                id.clone(),
                name.clone(),
                interrupted_tool_output(name),
                true,
            );
            notify_tool_result(runtime_observer_mut(observer), &message);
            *slot = Some(message);
        }
    }
}
