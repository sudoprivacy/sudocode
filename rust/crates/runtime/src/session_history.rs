//! Durable message identity and branch replay shared by every model adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::json::JsonValue;
use crate::{ContentBlock, ConversationMessage, MessageRole};

pub(super) const HISTORY_VERSION: i64 = 1;

/// A provider call ID belongs to one assistant message. Legacy completion
/// journals use the raw call ID until their legacy sources have settled.
#[must_use]
pub fn tool_source_key(assistant_message_id: Option<&str>, call_id: &str) -> String {
    assistant_message_id.map_or_else(
        || call_id.to_owned(),
        |assistant| serde_json::to_string(&(assistant, call_id)).expect("string tuple is JSON"),
    )
}

/// Read complete committed records while another writer may be appending its
/// final record. Storage recovery truncates that tail under the commit lock.
pub(super) fn decode(contents: &[u8]) -> Result<&str, crate::SessionError> {
    match std::str::from_utf8(contents) {
        Ok(contents) => Ok(contents),
        Err(error) => {
            let tail = contents
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map(|end| end + 1);
            if let Some(tail) =
                tail.filter(|tail| *tail < contents.len() && error.valid_up_to() >= *tail)
            {
                return std::str::from_utf8(&contents[..tail]).map_err(|error| {
                    crate::SessionError::Format(format!("invalid session UTF-8: {error}"))
                });
            }
            Err(crate::SessionError::Format(format!(
                "invalid session UTF-8: {error}"
            )))
        }
    }
}

pub(super) fn records(
    contents: &str,
) -> impl Iterator<Item = Result<(usize, JsonValue), crate::SessionError>> + '_ {
    contents
        .split_inclusive('\n')
        .enumerate()
        .filter_map(|(index, line)| {
            if line.trim().is_empty() {
                return None;
            }
            match JsonValue::parse(line) {
                Ok(value) => Some(Ok((index + 1, value))),
                Err(_) if !line.ends_with('\n') => {
                    tracing::warn!(
                        line = index + 1,
                        "incomplete trailing session append awaiting recovery"
                    );
                    None
                }
                Err(error) => Some(Err(crate::SessionError::Format(format!(
                    "invalid JSONL record at line {}: {error}",
                    index + 1
                )))),
            }
        })
}

/// The provider's tool-call ID is scoped to its originating assistant message.
/// These references describe conversation history, independently of a process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageIdentity {
    pub id: String,
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_origins: BTreeMap<String, String>,
}

impl MessageIdentity {
    pub(super) fn to_json(&self) -> JsonValue {
        let mut value = BTreeMap::from([
            ("id".into(), JsonValue::String(self.id.clone())),
            (
                "parent_id".into(),
                self.parent_id
                    .as_ref()
                    .map_or(JsonValue::Null, |id| JsonValue::String(id.clone())),
            ),
        ]);
        if !self.tool_origins.is_empty() {
            value.insert(
                "tool_origins".into(),
                JsonValue::Object(
                    self.tool_origins
                        .iter()
                        .map(|(call, origin)| (call.clone(), JsonValue::String(origin.clone())))
                        .collect(),
                ),
            );
        }
        JsonValue::Object(value)
    }

    pub(super) fn from_json(value: &JsonValue) -> Result<Self, crate::SessionError> {
        serde_json::from_str(&value.render()).map_err(|error| {
            crate::SessionError::Format(format!("invalid message identity: {error}"))
        })
    }

    pub(super) fn new(parent_id: Option<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id,
            tool_origins: BTreeMap::new(),
        }
    }
}

/// Give legacy messages reproducible identities before adding a branch. Loading
/// the same historical bytes on two devices must produce identical ancestors.
pub(super) fn identify_legacy(session_id: &str, messages: &mut [ConversationMessage]) {
    let mut parent = None;
    for (index, message) in messages.iter_mut().enumerate() {
        if message.history.is_none() {
            let mut digest = Sha256::new();
            digest.update(b"sudocode-message-v1\0");
            digest.update(session_id.as_bytes());
            digest.update((index as u64).to_be_bytes());
            digest.update(message.to_json().render().as_bytes());
            message.history = Some(Arc::new(MessageIdentity {
                id: format!("legacy-{:x}", digest.finalize()),
                parent_id: parent.clone(),
                tool_origins: BTreeMap::new(),
            }));
        }
        parent = message.history.as_ref().map(|identity| identity.id.clone());
    }
    let mut origins = BTreeMap::new();
    for message in messages {
        let Some(identity) = &mut message.history else {
            continue;
        };
        let identity = Arc::make_mut(identity);
        for block in &message.blocks {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    origins.insert(id.clone(), identity.id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    if let Some(origin) = origins.get(tool_use_id) {
                        identity
                            .tool_origins
                            .entry(tool_use_id.clone())
                            .or_insert_with(|| origin.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

pub(super) fn identify_append(
    message: &mut ConversationMessage,
    ancestors: &[ConversationMessage],
) -> Result<(), String> {
    let parent = ancestors
        .last()
        .and_then(|message| message.history.as_ref())
        .map(|identity| identity.id.clone());
    let mut identity = MessageIdentity::new(parent);
    for block in &message.blocks {
        if let ContentBlock::ToolResult { tool_use_id, .. } = block {
            let expected_origin = tool_origin(message, tool_use_id);
            let origin = ancestors.iter().rev().find(|ancestor| {
                ancestor.role == MessageRole::Assistant
                    && expected_origin.is_none_or(|origin| {
                        ancestor.history.as_ref().is_some_and(|identity| identity.id == origin)
                    })
                    && ancestor.blocks.iter().any(|block| {
                        matches!(block, ContentBlock::ToolUse { id, .. } if id == tool_use_id)
                    })
            });
            let origin = origin
                .and_then(|message| message.history.as_ref())
                .ok_or_else(|| {
                    format!("tool result has no originating assistant: {tool_use_id}")
                })?;
            identity
                .tool_origins
                .insert(tool_use_id.clone(), origin.id.clone());
        }
    }
    message.history = Some(Arc::new(identity));
    Ok(())
}

/// A provider call ID identifies an invocation within one assistant message.
pub(crate) fn tool_origin<'a>(message: &'a ConversationMessage, call_id: &str) -> Option<&'a str> {
    let identity = message.history.as_ref()?;
    if message.role == MessageRole::Assistant {
        Some(&identity.id)
    } else {
        identity.tool_origins.get(call_id).map(String::as_str)
    }
}

/// A replacement context gets its own immutable ancestry. Retained messages
/// keep their model payloads while their transcript references follow the new
/// summary, rather than the history that the summary replaced.
pub(super) fn rebase(messages: &mut [ConversationMessage]) {
    let ids: Vec<_> = messages
        .iter()
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let replacements: BTreeMap<_, _> = messages
        .iter()
        .zip(&ids)
        .filter_map(|(message, id)| {
            message
                .history
                .as_ref()
                .map(|identity| (identity.id.clone(), id.clone()))
        })
        .collect();
    let mut parent = None;
    let mut origins = BTreeMap::new();
    for (message, id) in messages.iter_mut().zip(ids) {
        let previous = message.history.take();
        let mut identity = MessageIdentity {
            id,
            parent_id: parent,
            tool_origins: BTreeMap::new(),
        };
        for block in &message.blocks {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    origins.insert(id.clone(), identity.id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    let origin = previous
                        .as_ref()
                        .and_then(|identity| identity.tool_origins.get(tool_use_id))
                        .and_then(|origin| replacements.get(origin))
                        .or_else(|| origins.get(tool_use_id));
                    if let Some(origin) = origin {
                        identity
                            .tool_origins
                            .insert(tool_use_id.clone(), origin.clone());
                    }
                }
                _ => {}
            }
        }
        parent = Some(identity.id.clone());
        message.history = Some(Arc::new(identity));
    }
}

/// A replay index exists only while restoring a transcript. Each live runtime
/// keeps its selected message chain; appending never reparses the shared log.
enum HistoryNode {
    Message {
        message: ConversationMessage,
        prompt_snapshot: Option<JsonValue>,
    },
    Checkpoint {
        parent: Option<String>,
        metadata: JsonValue,
        view: Option<Vec<String>>,
    },
}

#[derive(Default)]
pub(super) struct HistoryReplay {
    nodes: BTreeMap<String, HistoryNode>,
    head: Option<String>,
    genesis: Option<String>,
    receipts: Vec<crate::MaintenanceUsageReceipt>,
}

impl HistoryReplay {
    fn legacy_append(
        &self,
        session_id: &str,
        line_number: usize,
        message: ConversationMessage,
    ) -> Result<ConversationMessage, String> {
        // An older external writer can append an unversioned message after
        // migration. Bind it to this physical prefix with a reproducible ID;
        // repeated payloads and repeated restores must not alias each other.
        let mut digest = Sha256::new();
        digest.update(b"sudocode-legacy-tail-v1\0");
        digest.update(session_id.as_bytes());
        digest.update((line_number as u64).to_be_bytes());
        digest.update(message.to_json().render().as_bytes());
        let id = format!("legacy-tail-{:x}", digest.finalize());
        let mut messages = self.project(self.head.as_deref())?.messages;
        messages.push(message);
        // Reuse the legacy interpreter's tool-origin association. This cold
        // compatibility path does not run for identified or v3 records.
        identify_legacy(session_id, &mut messages);
        let mut message = messages.pop().expect("legacy append was inserted");
        let identity = Arc::make_mut(message.history.as_mut().expect("legacy identity assigned"));
        identity.id = id;
        identity.parent_id.clone_from(&self.head);
        Ok(message)
    }

    fn insert(
        &mut self,
        message: ConversationMessage,
        snapshot: Option<JsonValue>,
    ) -> Result<(), String> {
        let identity = message.history.as_ref().ok_or("message identity missing")?;
        if identity.id.is_empty() || identity.parent_id.as_deref() == Some(&identity.id) {
            return Err("invalid message identity".into());
        }
        if self.nodes.contains_key(&identity.id) {
            return Err(format!("duplicate history identity: {}", identity.id));
        }
        self.head = Some(identity.id.clone());
        self.nodes.insert(
            identity.id.clone(),
            HistoryNode::Message {
                message,
                prompt_snapshot: snapshot,
            },
        );
        Ok(())
    }

    fn checkpoint(
        &mut self,
        id: String,
        parent: Option<String>,
        metadata: JsonValue,
        messages: Vec<ConversationMessage>,
        view: Option<Vec<String>>,
        replaces_context: bool,
    ) -> Result<(), String> {
        let receipt_source = metadata
            .as_object()
            .ok_or("checkpoint metadata must be an object")?;
        let receipts = crate::session::maintenance_usage_from_json(receipt_source)
            .map_err(|error| error.to_string())?;
        crate::session::merge_maintenance_receipts(&mut self.receipts, &receipts);
        let previous_head = self.head.clone();
        for message in messages {
            let identity = message
                .history
                .as_ref()
                .ok_or("checkpoint message identity missing")?;
            if let Some(existing) = self.nodes.get(&identity.id) {
                // Legacy tool inputs may store either a JSON object or its
                // encoded string, including different Unicode escapes. The
                // persisted representation owns their canonical meaning.
                if !matches!(existing, HistoryNode::Message { message: previous, .. }
                    if previous == &message || previous.to_json() == message.to_json())
                {
                    return Err(
                        "checkpoint changes an existing message; append an update instead".into(),
                    );
                }
            } else {
                self.insert(message, None)?;
            }
        }
        if id.is_empty() || self.nodes.contains_key(&id) || parent.as_deref() == Some(&id) {
            return Err("invalid checkpoint identity".into());
        }
        if let Some(view) = &view {
            let mut selected = BTreeSet::new();
            for id in view {
                if !selected.insert(id)
                    || !matches!(self.nodes.get(id), Some(HistoryNode::Message { .. }))
                {
                    return Err("checkpoint view contains a missing or repeated message".into());
                }
            }
        }
        self.head = if replaces_context || previous_head.is_none() || previous_head == parent {
            Some(id.clone())
        } else {
            previous_head
        };
        self.genesis.get_or_insert_with(|| id.clone());
        self.nodes.insert(
            id,
            HistoryNode::Checkpoint {
                parent,
                metadata,
                view,
            },
        );
        Ok(())
    }

    fn update(&mut self, message: ConversationMessage) -> Result<(), String> {
        let identity = message.history.as_ref().ok_or("message identity missing")?;
        let Some(HistoryNode::Message {
            message: original, ..
        }) = self.nodes.get_mut(&identity.id)
        else {
            return Err("assistant update target missing".into());
        };
        if original.history.as_ref() != Some(identity)
            || original.role != MessageRole::Assistant
            || message.role != MessageRole::Assistant
        {
            return Err("assistant update changes message identity".into());
        }
        *original = message;
        Ok(())
    }

    fn project(&self, head: Option<&str>) -> Result<crate::Session, String> {
        let mut chain = Vec::new();
        let mut visited = BTreeSet::new();
        let mut cursor = head;
        let mut metadata = None;
        let mut latest_snapshot = None;
        while let Some(id) = cursor {
            if !visited.insert(id) {
                return Err("cycle in session history".into());
            }
            match self.nodes.get(id).ok_or("session ancestor missing")? {
                HistoryNode::Message {
                    message,
                    prompt_snapshot,
                } => {
                    if metadata.is_none() && latest_snapshot.is_none() {
                        latest_snapshot.clone_from(prompt_snapshot);
                    }
                    chain.push(message.clone());
                    cursor = message
                        .history
                        .as_ref()
                        .and_then(|identity| identity.parent_id.as_deref());
                }
                HistoryNode::Checkpoint {
                    parent,
                    metadata: checkpoint,
                    view,
                } => {
                    if metadata.is_none() {
                        metadata = Some(checkpoint.clone());
                    }
                    if let Some(view) = view {
                        for id in view.iter().rev() {
                            let Some(HistoryNode::Message { message, .. }) = self.nodes.get(id)
                            else {
                                return Err("checkpoint view message missing".into());
                            };
                            chain.push(message.clone());
                        }
                        break;
                    }
                    cursor = parent.as_deref();
                }
            }
        }
        chain.reverse();
        // Legacy readers can select an ancestor that predates the first
        // checkpoint. That immutable prefix inherits the genesis metadata.
        let metadata = metadata.or_else(|| {
            self.genesis
                .as_ref()
                .and_then(|id| match self.nodes.get(id) {
                    Some(HistoryNode::Checkpoint { metadata, .. }) => Some(metadata.clone()),
                    _ => None,
                })
        });
        let Some(JsonValue::Object(mut metadata)) = metadata else {
            return Err("session metadata missing".into());
        };
        if let Some(snapshot) = latest_snapshot {
            metadata.insert("prompt_snapshot".into(), snapshot);
        }
        metadata.insert(
            "messages".into(),
            JsonValue::Array(chain.iter().map(ConversationMessage::to_json).collect()),
        );
        let mut session = crate::Session::from_json(&JsonValue::Object(metadata))
            .map_err(|error| error.to_string())?;
        crate::session::merge_maintenance_receipts(&mut session.maintenance_usage, &self.receipts);
        Ok(session.with_history_head(head.map(str::to_owned)))
    }
}

fn replay_checkpoint(
    replay: &mut HistoryReplay,
    object: &BTreeMap<String, JsonValue>,
    session_id: &mut Option<String>,
) -> Result<(), crate::SessionError> {
    use crate::SessionError;
    if object.get("history_version").and_then(JsonValue::as_i64) != Some(HISTORY_VERSION) {
        return Err(SessionError::Format(
            "unsupported session history version".into(),
        ));
    }
    let metadata = object
        .get("meta")
        .cloned()
        .ok_or_else(|| SessionError::Format("history checkpoint metadata missing".into()))?;
    let id = metadata
        .as_object()
        .and_then(|metadata| metadata.get("session_id"))
        .and_then(JsonValue::as_str)
        .ok_or_else(|| SessionError::Format("history checkpoint session ID missing".into()))?;
    if session_id.as_ref().is_some_and(|previous| previous != id) {
        return Err(SessionError::Format(
            "history checkpoint changes session identity".into(),
        ));
    }
    *session_id = Some(id.to_owned());
    let messages = object
        .get("messages")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| SessionError::Format("history checkpoint messages missing".into()))?
        .iter()
        .map(ConversationMessage::from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let id = object
        .get("id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| SessionError::Format("history checkpoint identity missing".into()))?;
    let parent = match object.get("parent_id") {
        Some(JsonValue::Null) => None,
        Some(JsonValue::String(id)) => Some(id.clone()),
        _ => {
            return Err(SessionError::Format(
                "history checkpoint parent missing".into(),
            ))
        }
    };
    let view = object
        .get("view")
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| SessionError::Format("checkpoint view must be an array".into()))?
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        SessionError::Format("checkpoint message reference must be a string".into())
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    replay
        .checkpoint(
            id.to_owned(),
            parent,
            metadata,
            messages,
            view,
            object
                .get("replaces_context")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false),
        )
        .map_err(SessionError::Format)?;
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum Selection<'a> {
    Latest,
    Head(Option<&'a str>),
}

fn parse(contents: &str) -> Result<HistoryReplay, crate::SessionError> {
    use crate::SessionError;
    let mut replay = HistoryReplay::default();
    let mut session_id = None;
    for record in records(contents) {
        let (line_number, record) = record?;
        let object = record.as_object().ok_or_else(|| {
            SessionError::Format(format!("history record {line_number} must be an object"))
        })?;
        match object.get("type").and_then(JsonValue::as_str) {
            Some("history_checkpoint") => replay_checkpoint(&mut replay, object, &mut session_id)?,
            Some("message") => {
                let mut message = ConversationMessage::from_json(
                    object
                        .get("message")
                        .ok_or_else(|| SessionError::Format("message payload missing".into()))?,
                )?;
                if message.history.is_none() {
                    message = replay
                        .legacy_append(
                            session_id.as_deref().ok_or_else(|| {
                                SessionError::Format(
                                    "legacy append session identity missing".into(),
                                )
                            })?,
                            line_number,
                            message,
                        )
                        .map_err(SessionError::Format)?;
                }
                replay
                    .insert(message, object.get("prompt_snapshot").cloned())
                    .map_err(SessionError::Format)?;
            }
            Some("assistant_update") => {
                let id = object
                    .get("message_id")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| {
                        SessionError::Format("assistant update identity missing".into())
                    })?;
                let message =
                    ConversationMessage::from_json(object.get("message").ok_or_else(|| {
                        SessionError::Format("assistant update payload missing".into())
                    })?)?;
                if message
                    .history
                    .as_ref()
                    .map(|identity| identity.id.as_str())
                    != Some(id)
                {
                    return Err(SessionError::Format(
                        "assistant update identity mismatch".into(),
                    ));
                }
                replay.update(message).map_err(SessionError::Format)?;
            }
            other => {
                return Err(SessionError::Format(format!(
                    "unsupported shared history record: {other:?}"
                )))
            }
        }
    }
    Ok(replay)
}

/// Replay one immutable ancestry, independently of another runtime's appends.
pub(super) fn replay(
    contents: &str,
    selected: Selection<'_>,
) -> Result<crate::Session, crate::SessionError> {
    let replay = parse(contents)?;
    let head = match selected {
        Selection::Latest => replay.head.as_deref(),
        Selection::Head(head) => head,
    };
    replay.project(head).map_err(crate::SessionError::Format)
}

/// Nearest-first physical ancestry also crosses context-replacement checkpoints.
/// Completion ownership follows this chain while projection follows each view.
pub(super) fn ancestry(
    contents: &str,
    head: Option<&str>,
) -> Result<Vec<String>, crate::SessionError> {
    let replay = parse(contents)?;
    let mut ancestors = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursor = head;
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return Err(crate::SessionError::Format(
                "cycle in session history".into(),
            ));
        }
        ancestors.push(id.to_owned());
        cursor = match replay.nodes.get(id) {
            Some(HistoryNode::Message { message, .. }) => message
                .history
                .as_ref()
                .and_then(|identity| identity.parent_id.as_deref()),
            Some(HistoryNode::Checkpoint { parent, .. }) => parent.as_deref(),
            None => {
                return Err(crate::SessionError::Format(
                    "session ancestor missing".into(),
                ))
            }
        };
    }
    Ok(ancestors)
}
