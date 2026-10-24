use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use super::{
    invalid, metadata_value, to_value, HistoryEvent, HistoryLog, HistoryRecord, RecordOutcome,
};
use crate::fs_backend::RecordLog;
use crate::{ConversationMessage, Session};

const COMMIT_TIMEOUT: Duration = Duration::from_secs(30);

/// One runtime continuation. Effects follow an observed applied record; an
/// uncertain commit retires this writer until the caller restores the session.
pub(crate) struct ContextHistory {
    history: HistoryLog,
    context: String,
    failed: Option<String>,
}

impl ContextHistory {
    pub(crate) fn open(
        log: Arc<dyn RecordLog>,
        seed: Option<&Session>,
        source: Option<&str>,
    ) -> io::Result<Self> {
        let mut history = HistoryLog::new(log);
        history.refresh()?;
        let record = if history.graph().latest_commit().is_none() {
            HistoryRecord::initialize(seed.ok_or_else(|| invalid("new history requires a seed"))?)?
        } else {
            source.map_or_else(HistoryRecord::branch_latest, |id| {
                HistoryRecord::branch(id.into())
            })
        };
        let mut current = Self {
            history,
            context: record.context_id().into(),
            failed: None,
        };
        match current.history.commit(&record, COMMIT_TIMEOUT)? {
            RecordOutcome::Applied => {}
            RecordOutcome::Rejected(reason) if reason == "history_already_initialized" => {
                let branch = source.map_or_else(HistoryRecord::branch_latest, |id| {
                    HistoryRecord::branch(id.into())
                });
                current.context = branch.context_id().into();
                current.commit(&branch)?;
            }
            RecordOutcome::Rejected(reason) => return Err(invalid(reason)),
        }
        current.recover_inherited_calls()?;
        Ok(current)
    }

    fn recover_inherited_calls(&mut self) -> io::Result<()> {
        let mut session = self.session()?;
        if session.sanitize_orphan_tool_uses() == 0 {
            return Ok(());
        }
        let graph = self.history.graph();
        let source: HashSet<_> = graph.message_ids(&self.context)?.into_iter().collect();
        let retained: Vec<_> = session
            .messages
            .iter()
            .map(|message| {
                message
                    .history
                    .as_ref()
                    .map(|identity| identity.id.as_str())
                    .filter(|id| source.contains(*id))
            })
            .collect();
        // The existing projection record carries the exact recovery payload.
        // Replay never regenerates durable diagnostics from today's wording.
        let record =
            graph.prepare_projection(&self.context, self.revision()?, &session, &retained)?;
        self.commit(&record)
    }

    fn check(&self) -> io::Result<()> {
        self.failed.as_ref().map_or(Ok(()), |reason| {
            Err(io::Error::other(format!(
                "session history requires restore: {reason}"
            )))
        })
    }

    fn commit(&mut self, record: &HistoryRecord) -> io::Result<()> {
        self.check()?;
        match self.history.commit(record, COMMIT_TIMEOUT) {
            Ok(RecordOutcome::Applied) => Ok(()),
            Ok(RecordOutcome::Rejected(reason)) => Err(invalid(reason)),
            Err(error) => {
                self.failed = Some(error.to_string());
                Err(error)
            }
        }
    }

    pub(crate) fn session(&self) -> io::Result<Session> {
        self.check()?;
        self.history.graph().context_session(&self.context)
    }

    pub(crate) fn revision(&self) -> io::Result<&str> {
        self.check()?;
        self.history
            .graph()
            .revision(&self.context)
            .ok_or_else(|| invalid("context missing"))
    }

    pub(crate) fn message_head(&self) -> io::Result<Option<&str>> {
        self.check()?;
        self.history.graph().message_head(&self.context)
    }

    pub(crate) fn append(
        &mut self,
        message: &ConversationMessage,
        session: &Session,
    ) -> io::Result<()> {
        self.check()?;
        let mut metadata = metadata_value(session)?;
        // The following message advances the clock. A local touch alone must
        // not add a second durable metadata append before every message.
        metadata["updated_at_ms"] =
            self.history.graph().context_metadata(&self.context)?["updated_at_ms"].clone();
        self.save_metadata_value(metadata)?;
        let record = self
            .history
            .graph()
            .prepare_message(&self.context, message)?;
        self.commit(&record)
    }

    pub(crate) fn update(&mut self, message: &ConversationMessage) -> io::Result<()> {
        let identity = message
            .history
            .as_ref()
            .ok_or_else(|| invalid("message identity missing"))?;
        let record = HistoryRecord::new(
            self.context.clone(),
            Some(self.revision()?.into()),
            HistoryEvent::AssistantUpdate {
                message_id: identity.id.clone(),
                message: to_value(message)?,
            },
        );
        self.commit(&record)
    }

    pub(crate) fn save(&mut self, session: &Session, replace_view: bool) -> io::Result<()> {
        self.check()?;
        let graph = self.history.graph();
        let current = graph.message_ids(&self.context)?;
        let unchanged = current.len() == session.messages.len()
            && current.iter().zip(&session.messages).all(|(id, message)| {
                message
                    .history
                    .as_ref()
                    .is_some_and(|identity| identity.id == *id)
            });
        if !replace_view {
            if !unchanged {
                return Err(invalid("metadata save cannot replace the message view"));
            }
            return self.save_metadata(session);
        }
        let record = {
            let source: HashSet<_> = current.iter().map(String::as_str).collect();
            let retained: Vec<_> = session
                .messages
                .iter()
                .map(|message| {
                    message
                        .history
                        .as_ref()
                        .map(|identity| identity.id.as_str())
                        .filter(|id| source.contains(id))
                })
                .collect();
            graph.prepare_projection(&self.context, self.revision()?, session, &retained)?
        };
        self.commit(&record)
    }

    pub(crate) fn save_metadata(&mut self, session: &Session) -> io::Result<()> {
        self.save_metadata_value(metadata_value(session)?)
    }

    fn save_metadata_value(&mut self, mut metadata: serde_json::Value) -> io::Result<()> {
        self.check()?;
        let stored = self.history.graph().context_metadata(&self.context)?;
        let updated = metadata["updated_at_ms"]
            .as_u64()
            .unwrap_or(0)
            .max(stored["updated_at_ms"].as_u64().unwrap_or(0));
        metadata["updated_at_ms"] = updated.into();
        if metadata == *stored {
            return Ok(());
        }
        let record = HistoryRecord::new(
            self.context.clone(),
            Some(self.revision()?.into()),
            HistoryEvent::Metadata { metadata },
        );
        self.commit(&record)
    }

    pub(crate) fn finish(&mut self, turn_id: &str) -> io::Result<()> {
        self.check()?;
        if self.history.graph().has_open_tools(&self.context)? {
            return Err(invalid("cannot complete a turn with unfinished tools"));
        }
        if !self
            .history
            .graph()
            .has_uncommitted_messages(&self.context)?
        {
            return Ok(());
        }
        let record = HistoryRecord::new(
            self.context.clone(),
            Some(self.revision()?.into()),
            HistoryEvent::CommitTurn {
                trace_id: turn_id.into(),
            },
        );
        self.commit(&record)
    }
}
