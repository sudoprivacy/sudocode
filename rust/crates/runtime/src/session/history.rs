//! Append history with independent continuations. Opening a context freezes
//! its inherited prefix and applies interrupted-tool recovery to that branch.
//!
//! This layer does not admit writers. Hosts must retain their existing admission
//! protocol until transcript, completion and deletion consumers adopt this log.

mod context;
mod graph;
pub(crate) use context::ContextHistory;
pub use graph::HistoryGraph;

pub(crate) fn is_record_history(bytes: &[u8]) -> bool {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .next()
        .filter(|frame| frame.last() == Some(&b'\n'))
        .and_then(|frame| serde_json::from_slice::<serde_json::Value>(frame).ok())
        .is_some_and(|record| {
            record["type"] == "session_event"
                && record["version"] == 3
                && record["event"]["type"] == "initialize"
        })
}

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{ConversationMessage, Session};
use crate::fs_backend::RecordLog;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryRecord {
    #[serde(rename = "type")]
    kind: RecordKind,
    version: u32,
    id: String,
    context_id: String,
    expected_revision: Option<String>,
    timestamp_ms: u64,
    event: HistoryEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordKind {
    SessionEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryEvent {
    Initialize {
        session: Value,
    },
    OpenContext {
        source_commit: Option<String>,
    },
    Message {
        id: String,
        parent_id: Option<String>,
        message: Value,
    },
    AssistantUpdate {
        message_id: String,
        message: Value,
    },
    Metadata {
        metadata: Value,
    },
    Projection {
        metadata: Value,
        messages: Vec<ProjectionMessage>,
    },
    CommitTurn {
        trace_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionMessage {
    pub id: String,
    /// `None` retains the exact source message and its tool associations.
    pub message: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordOutcome {
    Applied,
    Rejected(String),
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn to_value(message: &ConversationMessage) -> io::Result<Value> {
    let mut value: Value =
        serde_json::from_str(&message.to_json().render()).map_err(io::Error::other)?;
    // Graph records own identity and ancestry; the provider payload must not
    // carry a second copy that changes when a context is projected.
    if let Some(object) = value.as_object_mut() {
        object.remove("history");
    }
    Ok(value)
}

fn metadata_value(session: &Session) -> io::Result<Value> {
    serde_json::from_str(
        &session
            .context_metadata()
            .map_err(io::Error::other)?
            .render(),
    )
    .map_err(io::Error::other)
}

impl HistoryRecord {
    #[must_use]
    pub fn new(context_id: String, expected_revision: Option<String>, event: HistoryEvent) -> Self {
        Self {
            kind: RecordKind::SessionEvent,
            version: 3,
            id: uuid::Uuid::new_v4().to_string(),
            context_id,
            expected_revision,
            timestamp_ms: super::current_time_millis(),
            event,
        }
    }

    pub fn initialize(session: &Session) -> io::Result<Self> {
        let session = serde_json::from_str(&session.to_json().map_err(io::Error::other)?.render())
            .map_err(io::Error::other)?;
        Ok(Self::new(
            uuid::Uuid::new_v4().to_string(),
            None,
            HistoryEvent::Initialize { session },
        ))
    }

    #[must_use]
    pub fn branch(source_commit: String) -> Self {
        Self::new(
            uuid::Uuid::new_v4().to_string(),
            None,
            HistoryEvent::OpenContext {
                source_commit: Some(source_commit),
            },
        )
    }

    /// Select the latest written continuation at this record's committed log position,
    /// including when the preparing client has only read a follower's old prefix.
    #[must_use]
    pub fn branch_latest() -> Self {
        Self::new(
            uuid::Uuid::new_v4().to_string(),
            None,
            HistoryEvent::OpenContext {
                source_commit: None,
            },
        )
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn context_id(&self) -> &str {
        &self.context_id
    }

    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        match &self.event {
            HistoryEvent::Message { id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Incrementally folds the authoritative record transport. Retried deliveries
/// retain their original record ID, including after an ambiguous append error.
pub struct HistoryLog {
    log: Arc<dyn RecordLog>,
    cursor: u64,
    graph: HistoryGraph,
}

impl HistoryLog {
    #[must_use]
    pub fn new(log: Arc<dyn RecordLog>) -> Self {
        Self {
            log,
            cursor: 0,
            graph: HistoryGraph::default(),
        }
    }

    #[must_use]
    pub fn graph(&self) -> &HistoryGraph {
        &self.graph
    }

    /// Fold a bounded prefix. This alone does not establish a follower read
    /// barrier; opening `branch_latest` and observing its committed position does.
    pub fn refresh(&mut self) -> io::Result<()> {
        // Bound work so a busy sibling cannot starve this writer. Only observing
        // our committed record establishes the prefix needed for its effects.
        for _ in 0..256 {
            let (bytes, next) = self.log.read(self.cursor)?;
            if bytes.is_empty() {
                return Ok(());
            }
            if next <= self.cursor {
                return Err(invalid("history cursor did not advance"));
            }
            for line in bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
            {
                let record: HistoryRecord =
                    serde_json::from_slice(line).map_err(io::Error::other)?;
                self.graph.apply(&record)?;
            }
            // A malformed later record leaves the batch cursor unchanged.
            // Retrying replay deduplicates its earlier already-applied records.
            self.cursor = next;
        }
        Ok(())
    }

    /// Observe a known initialization or commit before using a follower's view.
    pub fn observe(
        &mut self,
        record: &HistoryRecord,
        after: u64,
        timeout: Duration,
    ) -> io::Result<RecordOutcome> {
        let deadline = Instant::now() + timeout;
        loop {
            self.refresh()?;
            if let Some(outcome) = self.graph.outcome(record)? {
                if self.cursor >= after {
                    return Ok(outcome);
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "committed history record not yet observed",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Commit the same prepared record on every retry. Application effects
    /// require an observed Applied result, not merely a successful transport.
    pub fn commit(
        &mut self,
        record: &HistoryRecord,
        timeout: Duration,
    ) -> io::Result<RecordOutcome> {
        self.refresh()?;
        if let Some(outcome) = self.graph.outcome(record)? {
            return Ok(outcome);
        }
        match self.log.append(&record.encode()?) {
            Ok(after) => self.observe(record, after, timeout),
            Err(error) => {
                // A timed-out proposal can have committed. Refresh once before
                // returning uncertainty; a later retry still uses this ID.
                self.refresh()?;
                self.graph.outcome(record)?.ok_or(error)
            }
        }
    }
}
