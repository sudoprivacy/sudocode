//! Apply trusted tool transitions inside the model loop, for every renderer.
//! A completed exchange is the boundary: never leave an orphaned tool result,
//! reset during concurrent execution, or ask a renderer to start another turn.

use super::{
    ApiClient, CompactionProgress, CompactionStatus, ConversationMessage, ConversationRuntime,
    RuntimeError, RuntimeObserver, ToolExecutor,
};
use crate::compact::{aggregate_compaction_usage, extract_pre_compact_discovered_tools};
use crate::image_input::ToolContextAction;

impl<C: ApiClient, T: ToolExecutor> ConversationRuntime<C, T> {
    pub(super) fn apply_tool_context_action(
        &mut self,
        action: ToolContextAction,
        exchange_start: usize,
        observer: &mut Option<&mut dyn RuntimeObserver>,
    ) -> Result<(), RuntimeError> {
        match action {
            ToolContextAction::RestartFromCurrentExchange => {
                let before = crate::estimate_session_tokens(&self.session);
                let mut progress = CompactionProgress::started("plan_approval", before);
                if let Some(observer) = observer.as_deref_mut() {
                    observer.on_compaction(&progress);
                }
                let result = self.restart_from_current_exchange(exchange_start);
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

    fn restart_from_current_exchange(&mut self, exchange_start: usize) -> Result<(), RuntimeError> {
        let mut continuation = String::from(
            "Exploration context was cleared after the user approved the implementation plan \
             in the following tool result.",
        );
        if let Some(todos) = crate::render_todo_continuity_block(self.session.fs_handle()) {
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
