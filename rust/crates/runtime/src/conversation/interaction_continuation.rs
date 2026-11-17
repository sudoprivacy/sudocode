//! Resume an approved operation at the normal input boundary, through the same
//! hooks, policy, scheduler and output storage as an immediate tool invocation.
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{
    tool_execution::{ApprovedInvocation, ToolUpdate},
    ApiClient, ContentBlock, ConversationMessage, ConversationRuntime, PermissionPrompter,
    RuntimeError, RuntimeObserver, ToolExecutor,
};
use crate::session_interactions::{InteractionJournal, PermissionExecution};
use crate::turn_input::{InputEcho, InputSource, InteractionReceipt};

impl<C: ApiClient, T: ToolExecutor> ConversationRuntime<C, T> {
    pub(super) async fn continue_interactions(
        &mut self,
        blocks: &mut Vec<ContentBlock>,
        inputs: &[InputEcho],
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> Result<(), RuntimeError> {
        if !inputs
            .iter()
            .any(|input| matches!(input.source, InputSource::Interaction(_)))
        {
            return Ok(());
        }
        let journal = observer
            .as_deref()
            .ok_or_else(|| RuntimeError::new("interaction continuation requires its engine"))?
            .interaction_journal()?
            .ok_or_else(|| {
                RuntimeError::new("interaction continuation requires its session journal")
            })?;
        for input in inputs {
            let InputSource::Interaction(receipt) = input.source else {
                continue;
            };
            let Some((request, execution)) = journal
                .approved_permission(receipt)
                .map_err(RuntimeError::new)?
            else {
                continue;
            };
            let result = match execution {
                Some(PermissionExecution::Finished { .. }) => {
                    let saved = journal.stored_result(&receipt.id.to_string()).map_err(RuntimeError::new)?
                        .ok_or_else(|| RuntimeError::new("approved operation result is missing"))?;
                    let value = crate::json::JsonValue::parse(&saved["message"].to_string())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                    ConversationMessage::from_json(&value).map_err(|error| RuntimeError::new(error.to_string()))?
                }
                Some(PermissionExecution::Claimed { .. }) => {
                    ConversationMessage::tool_result(
                        format!("deferred-{}", receipt.id), request.tool_name,
                        "The approved operation was claimed before an interruption. Its outcome is unknown; it has not been retried. Inspect its effects before requesting another execution.", true,
                    )
                }
                None => {
                    let result = self.run_approved_operation(ApprovedInvocation { receipt, request, journal: journal.clone() }, observer, prompter, None).await?;
                    self.save_interaction_result(receipt, &journal, &result, None)?;
                    result
                }
            };
            if self.hook_abort_signal.is_aborted() {
                return Err(RuntimeError::new(
                    "approved-operation continuation interrupted; its saved result will be delivered on retry",
                ));
            }
            // The original tool already returned a pending receipt. Report this
            // later outcome as trusted input, without duplicating its tool_result
            // ID or manufacturing an assistant turn in the provider transcript.
            for block in result.blocks {
                match block {
                    ContentBlock::ToolResult {
                        tool_name,
                        output,
                        is_error,
                        ..
                    } => {
                        blocks.push(ContentBlock::Text {
                            text: serde_json::json!({
                                "approved_operation_result": {
                                    "request_id": receipt.id,
                                    "tool_name": tool_name,
                                    "output": output,
                                    "is_error": is_error,
                                }
                            })
                            .to_string(),
                        });
                    }
                    attachment => blocks.push(attachment),
                }
            }
        }
        Ok(())
    }

    fn save_interaction_result(
        &self,
        receipt: InteractionReceipt,
        journal: &InteractionJournal,
        message: &ConversationMessage,
        direct: Option<&crate::external_a2a::ExternalToolResult>,
    ) -> Result<(), RuntimeError> {
        // Check before touching the shared result blob as well as at its
        // journal commit. A foreign live claimant owns both output and receipt.
        journal
            .check_result_owner(receipt)
            .map_err(RuntimeError::new)?;
        let message: serde_json::Value = serde_json::from_str(&message.to_json().render())
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let bytes = serde_json::to_vec(&serde_json::json!({"message": message, "direct": direct}))
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        // Concurrent recovery may produce a different result before the journal
        // CAS chooses its winner. Each candidate must retain its own bytes.
        self.session
            .offload_tool_result(
                &format!("interaction-result-{}-{digest}", receipt.id),
                &bytes,
            )
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        journal
            .finish_permission(receipt, digest)
            .map_err(RuntimeError::new)
    }

    /// A host approval resumes the captured call without model inference or
    /// conversation mutation. Its result remains queryable after notification.
    pub async fn continue_direct_interaction(
        &mut self,
        receipt: InteractionReceipt,
        journal: Arc<InteractionJournal>,
        prompter: &mut dyn PermissionPrompter,
        tasks: crate::background_tasks::BackgroundTasks,
    ) -> Result<(), RuntimeError> {
        use crate::session_interactions::{
            InteractionOrigin, InteractionReply, InteractionRequest, InteractionStatus,
        };
        let record = journal
            .record(&receipt.id.to_string())
            .map_err(RuntimeError::new)?;
        if record.revision != receipt.revision || record.status != InteractionStatus::Resolved {
            return Err(RuntimeError::new(
                "direct approval is stale or already delivered",
            ));
        }
        let InteractionOrigin::Direct { identity } = record.origin else {
            return Err(RuntimeError::new("interaction is not a direct host call"));
        };
        let InteractionRequest::Permission(request) = record.request else {
            return Err(RuntimeError::new(
                "direct interaction is not a permission request",
            ));
        };
        if journal
            .delivery_block_reason(receipt)
            .map_err(RuntimeError::new)?
            .is_some()
        {
            return Ok(());
        }
        let _operation = tasks.begin_operation().map_err(RuntimeError::new)?;
        if matches!(record.execution, Some(PermissionExecution::Finished { .. })) {
            journal
                .stored_result(&receipt.id.to_string())
                .map_err(RuntimeError::new)?
                .ok_or_else(|| RuntimeError::new("direct operation result is missing"))?;
        } else {
            let call = crate::external_a2a::ExternalA2aCall::restore(identity);
            let denied = match record.reply {
                Some(InteractionReply::Permission { approved: true, .. }) => None,
                Some(InteractionReply::Permission { reason, .. }) => {
                    Some(reason.unwrap_or_else(|| "User denied the operation.".into()))
                }
                Some(InteractionReply::Cancelled { reason }) => Some(reason),
                _ => return Err(RuntimeError::new("direct call has no permission decision")),
            };
            let message = if let Some(reason) = denied {
                ConversationMessage::tool_result(
                    call.identity.command_id.clone(),
                    &request.tool_name,
                    reason,
                    true,
                )
            } else if matches!(record.execution, Some(PermissionExecution::Claimed { .. })) {
                ConversationMessage::tool_result(call.identity.command_id.clone(), &request.tool_name,
                    "The approved direct call was claimed before an interruption. Its outcome is unknown; it has not been retried.", true)
            } else {
                self.run_approved_operation(
                    ApprovedInvocation {
                        receipt,
                        request,
                        journal: journal.clone(),
                    },
                    &mut None,
                    &mut Some(prompter),
                    Some((call.clone(), tasks)),
                )
                .await?
            };
            let (content, is_error) = message
                .blocks
                .iter()
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        output, is_error, ..
                    } => Some((output.clone(), *is_error)),
                    _ => None,
                })
                .ok_or_else(|| RuntimeError::new("direct operation produced no result"))?;
            self.save_interaction_result(
                receipt,
                &journal,
                &message,
                Some(&call.report(content, is_error)),
            )?;
        }
        if self.hook_abort_signal.is_aborted() {
            return Err(RuntimeError::new(
                "direct continuation interrupted; its saved result will be delivered on retry",
            ));
        }
        journal
            .acknowledge(&receipt.id.to_string(), receipt.revision)
            .map_err(RuntimeError::new)
    }

    #[allow(clippy::await_holding_lock)]
    async fn run_approved_operation(
        &mut self,
        approval: ApprovedInvocation,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
        direct: Option<(
            crate::external_a2a::ExternalA2aCall,
            crate::background_tasks::BackgroundTasks,
        )>,
    ) -> Result<ConversationMessage, RuntimeError> {
        let receipt = approval.receipt;
        let request = approval.request.clone();
        let abort = self.hook_abort_signal.for_current_turn();
        self.tool_executor_mut().set_abort_signal(abort.clone());
        let mut context = self.dispatch_context(observer, prompter);
        let direct = direct.map(|(call, tasks)| {
            context.background_tasks = Some(tasks);
            call
        });
        context.external_a2a = direct.clone().map(Arc::new);
        let executor = Arc::clone(&self.tool_executor);
        let executor = executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if direct.as_ref().is_some_and(|call| {
            call.identity.session_id != self.session.session_id
                || executor.external_a2a_binding().as_ref() != Some(&call.identity.binding)
                || call.identity.tool != request.tool_name
        }) {
            return Ok(ConversationMessage::tool_result(format!("deferred-{}", receipt.id), request.tool_name,
                "The direct call's session, tool or executor binding changed after approval; nothing was executed.", true));
        }
        let mut scheduler = self
            .new_response_scheduler(&*executor, abort.clone())
            .with_approval(approval);
        scheduler.initialize_context(context);
        scheduler.enqueue(
            direct.map_or_else(
                || format!("deferred-{}", receipt.id),
                |call| call.identity.command_id,
            ),
            request.tool_name,
            request.input,
            None,
        )?;
        loop {
            tokio::select! {
                biased;
                () = abort.cancelled() => {
                    scheduler.cancel();
                    // Use the scheduler's existing interruption contract,
                    // including a completed output whose post hook was cut
                    // short. The caller persists it before returning the
                    // cancelled input lease for an explicit retry.
                    while let Some(update) = scheduler.take_update() {
                        self.observe_tool_update(update, &mut scheduler, 0, observer);
                    }
                    return scheduler.result(0).cloned()
                        .ok_or_else(|| RuntimeError::new("cancelled approved operation produced no result"));
                }
                update = scheduler.next(prompter) => {
                    let completed = matches!(update, ToolUpdate::Completed { .. });
                    self.observe_tool_update(update, &mut scheduler, 0, observer);
                    if completed {
                        return scheduler.result(0).cloned()
                            .ok_or_else(|| RuntimeError::new("approved operation completed without a result"));
                    }
                }
            }
        }
    }
}
