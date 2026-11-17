//! Apply trusted tool transitions inside the model loop, for every renderer.
//! A completed exchange is the boundary: never leave an orphaned tool result,
//! reset during concurrent execution, or ask a renderer to start another turn.

use super::{
    ApiClient, CompactionProgress, CompactionStatus, ConversationMessage, ConversationRuntime,
    PermissionPrompter, RuntimeError, RuntimeObserver, ToolExecutor,
};
use crate::compact::{aggregate_compaction_usage, extract_pre_compact_discovered_tools};
use crate::image_input::ToolContextAction;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptInputCommit {
    Committed,
    PreviouslyCommitted,
    Returned,
}

impl PromptInputCommit {
    pub(super) fn is_consumed(self) -> bool {
        self != Self::Returned
    }
}

impl<C: ApiClient, T: ToolExecutor> ConversationRuntime<C, T> {
    /// Clear-history approval and its reply receipt commit together. A restart
    /// can never observe an acknowledged reply whose context transition was lost.
    pub(super) async fn commit_prompt_input(
        &mut self,
        blocks: Vec<crate::ContentBlock>,
        inputs: Vec<crate::turn_input::InputEcho>,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> Result<PromptInputCommit, RuntimeError> {
        if Self::prompt_already_committed(&inputs, observer)? {
            return Ok(PromptInputCommit::PreviouslyCommitted);
        }
        if let Some(reason) = Self::waiting_interaction(&inputs, observer)? {
            return Self::return_interaction_input(&inputs, &reason, observer);
        }
        let result = self
            .commit_new_prompt_input(blocks, inputs.clone(), observer, prompter)
            .await;
        match result {
            Ok(()) => Ok(PromptInputCommit::Committed),
            Err(error) => {
                // Another continuation can win canonical history admission
                // after our preflight. Only a durable receipt permits dropping
                // this input; arbitrary persistence errors keep ownership.
                if Self::prompt_already_committed(&inputs, observer)? {
                    Ok(PromptInputCommit::PreviouslyCommitted)
                } else if let Some(reason) = Self::waiting_interaction(&inputs, observer)? {
                    Self::return_interaction_input(&inputs, &reason, observer)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn waiting_interaction(
        inputs: &[crate::turn_input::InputEcho],
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<Option<String>, RuntimeError> {
        let receipts: Vec<_> = inputs
            .iter()
            .filter_map(|input| match input.source {
                crate::turn_input::InputSource::Interaction(receipt) => Some(receipt),
                _ => None,
            })
            .collect();
        if receipts.is_empty() {
            return Ok(None);
        }
        let journal = observer
            .as_deref()
            .ok_or_else(|| RuntimeError::new("interaction input requires its engine"))?
            .interaction_journal()?
            .ok_or_else(|| RuntimeError::new("interaction input requires its session journal"))?;
        for receipt in receipts {
            if let Some(reason) = journal
                .delivery_block_reason(receipt)
                .map_err(RuntimeError::new)?
            {
                return Ok(Some(reason));
            }
        }
        Ok(None)
    }

    pub(super) fn return_interaction_input(
        inputs: &[crate::turn_input::InputEcho],
        reason: &str,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<PromptInputCommit, RuntimeError> {
        if inputs.is_empty()
            || !inputs
                .iter()
                .all(|input| matches!(input.source, crate::turn_input::InputSource::Interaction(_)))
        {
            return Err(RuntimeError::new(
                "waiting interaction cannot discard mixed human input",
            ));
        }
        let observer = observer
            .as_deref_mut()
            .ok_or_else(|| RuntimeError::new("interaction input requires its engine"))?;
        for input in inputs {
            if let crate::turn_input::InputSource::Interaction(receipt) = input.source {
                observer.on_interaction_delivery_returned(receipt);
            }
        }
        observer.on_notice(reason);
        Ok(PromptInputCommit::Returned)
    }

    pub(super) fn prompt_already_committed(
        inputs: &[crate::turn_input::InputEcho],
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<bool, RuntimeError> {
        if !inputs
            .iter()
            .any(|input| matches!(input.source, crate::turn_input::InputSource::Interaction(_)))
        {
            return Ok(false);
        }
        let journal = observer
            .as_deref()
            .ok_or_else(|| RuntimeError::new("interaction input requires its engine"))?
            .interaction_journal()?
            .ok_or_else(|| RuntimeError::new("interaction input requires its session journal"))?;
        let committed = journal
            .committed_receipts(inputs)
            .map_err(RuntimeError::new)?;
        if committed.is_empty() {
            return Ok(false);
        }
        if !inputs.iter().all(|input| match input.source {
            crate::turn_input::InputSource::Interaction(receipt) => committed.contains(&receipt),
            _ => false,
        }) {
            return Err(RuntimeError::new(
                "an interaction reply was already consumed; mixed input was retained for retry",
            ));
        }
        if let Some(observer) = observer.as_deref_mut() {
            observer.on_notice("This answer was already consumed by another session context.");
        }
        Ok(true)
    }

    async fn commit_new_prompt_input(
        &mut self,
        mut blocks: Vec<crate::ContentBlock>,
        inputs: Vec<crate::turn_input::InputEcho>,
        observer: &mut Option<&mut dyn RuntimeObserver>,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> Result<(), RuntimeError> {
        let action = if let Some(observer) = observer.as_deref() {
            observer.validate_input_echoes(&inputs)?;
            observer.input_context_action(&inputs)?
        } else {
            None
        };
        self.continue_interactions(&mut blocks, &inputs, observer, prompter)
            .await?;
        if let Some(action) = action {
            let mut prompt = ConversationMessage::user_text("");
            prompt.blocks = blocks;
            prompt.inputs = inputs;
            self.apply_context_action(action, self.session.messages.len(), Some(prompt), observer)
        } else {
            self.session
                .push_prompt(blocks, inputs)
                .map_err(|error| RuntimeError::new(error.to_string()))
        }
    }

    pub(super) fn apply_tool_context_action(
        &mut self,
        action: ToolContextAction,
        exchange_start: usize,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<(), RuntimeError> {
        self.apply_context_action(action, exchange_start, None, observer)
    }

    fn apply_context_action(
        &mut self,
        action: ToolContextAction,
        exchange_start: usize,
        pending_prompt: Option<ConversationMessage>,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<(), RuntimeError> {
        match action {
            ToolContextAction::RestartFromCurrentExchange => {
                let before = crate::estimate_session_tokens(&self.session);
                let mut progress = CompactionProgress::started("plan_approval", before);
                if let Some(observer) = observer.as_deref_mut() {
                    observer.on_compaction(&progress);
                }
                let result = self.restart_from_current_exchange(exchange_start, pending_prompt);
                progress.status = if result.is_ok() {
                    progress.after_tokens = Some(crate::estimate_session_tokens(&self.session));
                    CompactionStatus::Completed
                } else {
                    CompactionStatus::Failed
                };
                if let Some(observer) = observer.as_deref_mut() {
                    observer.on_compaction(&progress);
                }
                result
            }
        }
    }

    fn restart_from_current_exchange(
        &mut self,
        exchange_start: usize,
        pending_prompt: Option<ConversationMessage>,
    ) -> Result<(), RuntimeError> {
        let mut continuation = String::from(
            "Exploration context was cleared after the user approved the implementation plan \
             recorded below.",
        );
        if let Some(todos) = crate::render_todo_continuity_block(&self.session.fs_handle()) {
            continuation.push_str("\n\n");
            continuation.push_str(&todos);
        }

        // Clone session metadata, including the frozen prompt snapshot, model,
        // filesystem and identity. Only message history changes, as in compact.
        let mut candidate = self.session.clone();
        candidate.messages = vec![ConversationMessage::user_text(&continuation)];
        candidate
            .messages
            .extend_from_slice(&self.session.messages[exchange_start..]);
        candidate.messages.extend(pending_prompt);
        candidate.record_compaction_with_usage(
            continuation,
            exchange_start,
            aggregate_compaction_usage(
                self.session.compaction.as_ref().and_then(|c| c.usage),
                &self.session.messages[..exchange_start],
            ),
            extract_pre_compact_discovered_tools(&self.session),
        );
        if let Some(path) = self.session.persistence_path() {
            candidate
                .save_compacted_to_path(&self.session, path)
                .map_err(|error| {
                    RuntimeError::new(format!(
                        "failed to persist approved-plan continuation: {error}"
                    ))
                })?;
        }
        self.install_compacted_session(candidate);
        Ok(())
    }
}
