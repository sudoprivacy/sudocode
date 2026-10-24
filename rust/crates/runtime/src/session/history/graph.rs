use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    invalid, metadata_value, to_value, HistoryEvent, HistoryRecord, ProjectionMessage,
    RecordOutcome,
};
use crate::json::JsonValue;
use crate::{ContentBlock, ConversationMessage, MessageRole, Session};

#[derive(Clone, Default)]
struct MessageChain {
    prefix: Option<Arc<Self>>,
    tail: Vec<String>,
}

impl MessageChain {
    fn append(chain: &mut Arc<Self>, id: String) {
        if chain.tail.len() == 64 {
            *chain = Arc::new(Self {
                prefix: Some(Arc::clone(chain)),
                tail: vec![id],
            });
        } else {
            Arc::make_mut(chain).tail.push(id);
        }
    }

    fn ids(&self) -> Vec<String> {
        let mut chunks = vec![self.tail.as_slice()];
        let mut prefix = self.prefix.as_deref();
        while let Some(chain) = prefix {
            chunks.push(chain.tail.as_slice());
            prefix = chain.prefix.as_deref();
        }
        chunks.into_iter().rev().flatten().cloned().collect()
    }
}

impl Drop for MessageChain {
    fn drop(&mut self) {
        let mut prefix = self.prefix.take();
        while let Some(chain) = prefix {
            match Arc::try_unwrap(chain) {
                Ok(mut chain) => prefix = chain.prefix.take(),
                Err(_) => break,
            }
        }
    }
}

struct MessageNode {
    message: ConversationMessage,
    parent: Option<String>,
    tool_origins: BTreeMap<String, String>,
}

#[derive(Clone)]
struct Projection {
    chain: Arc<MessageChain>,
    metadata: Arc<Value>,
}

struct Context {
    revision: String,
    base_commit: String,
    projection: Projection,
    unsealed: BTreeSet<String>,
    open_tools: BTreeMap<String, String>,
    inherited_unfinished: bool,
}

#[derive(Clone)]
struct CompletedProjection {
    projection: Projection,
    turn_id: String,
}

type SeenRecord = ([u8; 32], RecordOutcome);

/// Stable messages and completed projections. Streamed updates replace only
/// this context's uncommitted assistant; completed prefixes remain immutable.
#[derive(Default)]
pub struct HistoryGraph {
    session_id: Option<String>,
    messages: HashMap<String, MessageNode>,
    contexts: HashMap<String, Context>,
    commits: HashMap<String, CompletedProjection>,
    latest_commit: Option<String>,
    latest_context: Option<String>,
    seen: HashMap<String, SeenRecord>,
    parents: HashMap<String, Option<String>>,
}

fn digest(record: &HistoryRecord) -> io::Result<[u8; 32]> {
    let mut canonical = serde_json::to_value(record).map_err(io::Error::other)?;
    canonical.sort_all_objects();
    Ok(Sha256::digest(serde_json::to_vec(&canonical).map_err(io::Error::other)?).into())
}

fn decode_message(value: &Value) -> io::Result<ConversationMessage> {
    let value = JsonValue::parse(&value.to_string()).map_err(io::Error::other)?;
    let mut message =
        super::super::ConversationMessage::from_json(&value).map_err(io::Error::other)?;
    message.history = None;
    Ok(message)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn rejected(reason: &str) -> RecordOutcome {
    RecordOutcome::Rejected(reason.into())
}

fn touch(projection: &mut Projection, timestamp: u64) {
    let metadata = Arc::make_mut(&mut projection.metadata);
    let before = metadata["updated_at_ms"].as_u64().unwrap_or(0);
    metadata["updated_at_ms"] = Value::from(before.max(timestamp));
}

fn metadata_snapshot(metadata: &Value, source: &Value) -> io::Result<Option<Arc<Value>>> {
    if !metadata.is_object()
        || ["session_id", "created_at_ms", "identity"]
            .iter()
            .any(|field| metadata.get(*field) != source.get(*field))
        || metadata.get("messages").is_some()
    {
        return Ok(None);
    }
    let mut checked = metadata.clone();
    checked["messages"] = Value::Array(Vec::new());
    Session::from_json(&JsonValue::parse(&checked.to_string()).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    Ok(Some(Arc::new(metadata.clone())))
}

fn tool_changes(
    message: &ConversationMessage,
    assistant_id: &str,
    open: &BTreeMap<String, String>,
) -> io::Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
    let mut open = open.clone();
    let mut origins = BTreeMap::new();
    let mut calls = BTreeSet::new();
    for block in &message.blocks {
        match block {
            ContentBlock::ToolUse { id, .. } => {
                if message.role != MessageRole::Assistant
                    || id.is_empty()
                    || id.len() > 512
                    || !calls.insert(id.clone())
                {
                    return Err(invalid("invalid or duplicate tool call"));
                }
                if open.get(id).is_some_and(|origin| origin != assistant_id) {
                    return Err(invalid(
                        "tool call identity already pending for another assistant",
                    ));
                }
                open.insert(id.clone(), assistant_id.into());
            }
            ContentBlock::ToolResult { tool_use_id, .. } => {
                if matches!(message.role, MessageRole::Assistant | MessageRole::System) {
                    return Err(invalid("tool result has an invalid speaker role"));
                }
                let origin = open
                    .remove(tool_use_id)
                    .ok_or_else(|| invalid("tool result has no open originating assistant"))?;
                origins.insert(tool_use_id.clone(), origin);
            }
            _ => {}
        }
    }
    Ok((open, origins))
}

fn is_recovery_projection(messages: &[ProjectionMessage], source: &[String]) -> io::Result<bool> {
    let mut retained = source.iter();
    for entry in messages {
        if let Some(value) = &entry.message {
            let message = decode_message(value)?;
            if !matches!(message.role, MessageRole::Tool | MessageRole::User)
                || message.blocks.is_empty()
                || !message
                    .blocks
                    .iter()
                    .all(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
            {
                return Ok(false);
            }
        } else if retained.next() != Some(&entry.id) {
            return Ok(false);
        }
    }
    Ok(retained.next().is_none())
}

impl HistoryGraph {
    /// The newest completed turn in this reader's folded prefix.
    #[must_use]
    pub fn latest_commit(&self) -> Option<&str> {
        self.latest_commit.as_deref()
    }

    #[must_use]
    pub fn revision(&self, context: &str) -> Option<&str> {
        self.contexts
            .get(context)
            .map(|context| context.revision.as_str())
    }

    #[must_use]
    pub fn tool_origin(&self, message: &str, call: &str) -> Option<&str> {
        self.messages
            .get(message)?
            .tool_origins
            .get(call)
            .map(String::as_str)
    }

    #[must_use]
    pub fn parent(&self, message: &str) -> Option<&str> {
        self.messages.get(message)?.parent.as_deref()
    }

    pub fn outcome(&self, record: &HistoryRecord) -> io::Result<Option<RecordOutcome>> {
        match self.seen.get(record.id()) {
            Some((known, outcome)) if *known == digest(record)? => Ok(Some(outcome.clone())),
            Some(_) => Err(invalid("history record ID reused with different contents")),
            None => Ok(None),
        }
    }

    pub fn apply(&mut self, record: &HistoryRecord) -> io::Result<RecordOutcome> {
        if record.version != 3 || !valid_id(record.id()) || !valid_id(record.context_id()) {
            return Err(invalid("unsupported history record version or identity"));
        }
        if record.expected_revision.is_some()
            && matches!(
                record.event,
                HistoryEvent::Initialize { .. } | HistoryEvent::OpenContext { .. }
            )
        {
            return Err(invalid(
                "initialization and context opening cannot replace a revision",
            ));
        }
        if let Some(outcome) = self.outcome(record)? {
            return Ok(outcome);
        }
        let parent = match &record.event {
            HistoryEvent::Initialize { .. } => None,
            HistoryEvent::OpenContext { source_commit } => source_commit.clone().or_else(|| {
                self.latest_context
                    .as_ref()
                    .and_then(|id| self.revision(id))
                    .map(str::to_owned)
            }),
            _ => record.expected_revision.clone(),
        };
        let outcome = match &record.event {
            HistoryEvent::Initialize { session } => self.initialize(record, session)?,
            HistoryEvent::OpenContext { source_commit } => {
                self.open_context(record, source_commit.as_deref())?
            }
            _ => self.change(record)?,
        };
        if outcome == RecordOutcome::Applied {
            self.parents.insert(record.id.clone(), parent);
        }
        self.seen
            .insert(record.id.clone(), (digest(record)?, outcome.clone()));
        Ok(outcome)
    }

    fn open_context(
        &mut self,
        record: &HistoryRecord,
        source: Option<&str>,
    ) -> io::Result<RecordOutcome> {
        if self.contexts.contains_key(record.context_id()) {
            return Ok(rejected("context_already_exists"));
        }
        let (projection, mutable, base_commit, inherited_unfinished, mut open) =
            if let Some(commit) = source {
                let Some(source) = self.commits.get(commit) else {
                    return Ok(rejected("source_turn_not_committed"));
                };
                (
                    source.projection.clone(),
                    BTreeSet::new(),
                    commit.to_string(),
                    false,
                    BTreeMap::new(),
                )
            } else {
                let source = self
                    .latest_context
                    .as_ref()
                    .and_then(|id| self.contexts.get(id))
                    .ok_or_else(|| invalid("resume context missing"))?;
                (
                    source.projection.clone(),
                    source.unsealed.clone(),
                    source.base_commit.clone(),
                    source.inherited_unfinished || !source.unsealed.is_empty(),
                    source.open_tools.clone(),
                )
            };
        let mut chain = Arc::clone(&projection.chain);
        let mut nodes = Vec::new();
        if !mutable.is_empty() {
            chain = Arc::new(MessageChain::default());
            open.clear();
            let mut copied_tail = false;
            for (index, original) in projection.chain.ids().into_iter().enumerate() {
                let message = &self
                    .messages
                    .get(&original)
                    .expect("indexed source message")
                    .message;
                copied_tail |= mutable.contains(&original);
                let id = if copied_tail {
                    let bytes = serde_json::to_vec(&(
                        "resume-message-v1",
                        record.context_id(),
                        index,
                        Some(original.as_str()),
                    ))
                    .map_err(io::Error::other)?;
                    format!("resume-{:x}", Sha256::digest(bytes))
                } else {
                    original
                };
                let (next_open, origins) = tool_changes(message, &id, &open)?;
                open = next_open;
                if copied_tail {
                    if self.messages.contains_key(&id) {
                        return Ok(rejected("message_identity_conflict"));
                    }
                    nodes.push((
                        id.clone(),
                        MessageNode {
                            message: message.clone(),
                            parent: chain.tail.last().cloned(),
                            tool_origins: origins,
                        },
                    ));
                }
                MessageChain::append(&mut chain, id);
            }
        }
        let projection = Projection {
            chain,
            metadata: projection.metadata,
        };
        self.messages.extend(nodes);
        self.contexts.insert(
            record.context_id.clone(),
            Context {
                revision: record.id.clone(),
                base_commit,
                projection,
                unsealed: BTreeSet::new(),
                open_tools: open,
                inherited_unfinished,
            },
        );
        Ok(RecordOutcome::Applied)
    }

    fn initialize(&mut self, record: &HistoryRecord, value: &Value) -> io::Result<RecordOutcome> {
        if self.session_id.is_some() {
            return Ok(rejected("history_already_initialized"));
        }
        let parsed = JsonValue::parse(&value.to_string()).map_err(io::Error::other)?;
        let session = Session::from_json(&parsed).map_err(io::Error::other)?;
        if !valid_id(&session.session_id) {
            return Err(invalid("invalid durable session identity"));
        }
        let mut metadata = value.clone();
        metadata
            .as_object_mut()
            .ok_or_else(|| invalid("session metadata must be an object"))?
            .remove("messages");
        let mut chain = Arc::new(MessageChain::default());
        let mut nodes = HashMap::new();
        let mut open = BTreeMap::new();
        for (index, message) in session.messages.iter().enumerate() {
            let mut identity = serde_json::to_value((
                "legacy-message-v1",
                &session.session_id,
                index,
                to_value(message)?,
            ))
            .map_err(io::Error::other)?;
            identity.sort_all_objects();
            let bytes = serde_json::to_vec(&identity).map_err(io::Error::other)?;
            let id = message.history.as_ref().map_or_else(
                || format!("legacy-{:x}", Sha256::digest(bytes)),
                |identity| identity.id.clone(),
            );
            if !valid_id(&id) || nodes.contains_key(&id) {
                return Ok(rejected("initial_message_identity_conflict"));
            }
            let Ok((next_open, origins)) = tool_changes(message, &id, &open) else {
                return Ok(rejected("initial_tool_association_invalid"));
            };
            open = next_open;
            nodes.insert(
                id.clone(),
                MessageNode {
                    message: decode_message(&to_value(message)?)?,
                    parent: chain.tail.last().cloned(),
                    tool_origins: origins,
                },
            );
            MessageChain::append(&mut chain, id);
        }
        if !open.is_empty() {
            return Ok(rejected("initial_context_has_unfinished_tools"));
        }
        let projection = Projection {
            chain,
            metadata: Arc::new(metadata),
        };
        self.session_id = Some(session.session_id);
        self.messages = nodes;
        self.contexts.insert(
            record.context_id.clone(),
            Context {
                revision: record.id.clone(),
                base_commit: record.id.clone(),
                projection: projection.clone(),
                unsealed: BTreeSet::new(),
                open_tools: BTreeMap::new(),
                inherited_unfinished: false,
            },
        );
        self.commits.insert(
            record.id.clone(),
            CompletedProjection {
                projection,
                turn_id: record.id.clone(),
            },
        );
        self.latest_commit = Some(record.id.clone());
        self.latest_context = Some(record.context_id.clone());
        Ok(RecordOutcome::Applied)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keep ordered context validation and atomic state mutation together"
    )]
    fn change(&mut self, record: &HistoryRecord) -> io::Result<RecordOutcome> {
        let Some(context) = self.contexts.get_mut(record.context_id()) else {
            return Ok(rejected("context_missing"));
        };
        if record.expected_revision.as_deref() != Some(context.revision.as_str()) {
            return Ok(rejected("context_revision_changed"));
        }
        let was_completed = context.unsealed.is_empty()
            && context.open_tools.is_empty()
            && !context.inherited_unfinished;
        match &record.event {
            HistoryEvent::Message {
                id,
                parent_id,
                message,
            } => {
                if !valid_id(id) || self.messages.contains_key(id) {
                    return Ok(rejected("message_identity_conflict"));
                }
                if parent_id.as_ref() != context.projection.chain.tail.last() {
                    return Ok(rejected("message_parent_changed"));
                }
                let message = decode_message(message)?;
                let Ok((open, origins)) = tool_changes(&message, id, &context.open_tools) else {
                    return Ok(rejected("tool_association_invalid"));
                };
                self.messages.insert(
                    id.clone(),
                    MessageNode {
                        message,
                        parent: parent_id.clone(),
                        tool_origins: origins,
                    },
                );
                MessageChain::append(&mut context.projection.chain, id.clone());
                context.unsealed.insert(id.clone());
                context.open_tools = open;
            }
            HistoryEvent::AssistantUpdate {
                message_id,
                message,
            } => {
                if !context.unsealed.contains(message_id) {
                    return Ok(rejected("assistant_already_committed_or_inherited"));
                }
                let node = self
                    .messages
                    .get(message_id)
                    .ok_or_else(|| invalid("assistant update target missing"))?;
                let message = decode_message(message)?;
                if node.message.role != MessageRole::Assistant
                    || message.role != MessageRole::Assistant
                {
                    return Ok(rejected("update_target_is_not_assistant"));
                }
                let old_calls: BTreeSet<_> = node
                    .message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, .. } => Some(id),
                        _ => None,
                    })
                    .collect();
                let new_calls: BTreeSet<_> = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse { id, .. } => Some(id),
                        _ => None,
                    })
                    .collect();
                if !old_calls.is_subset(&new_calls)
                    || new_calls.len()
                        != message
                            .blocks
                            .iter()
                            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                            .count()
                    || node
                        .message
                        .blocks
                        .iter()
                        .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                        .any(|block| !message.blocks.contains(block))
                {
                    return Ok(rejected("streamed_tool_call_changed"));
                }
                // Only newly streamed calls affect the pending set. Previously
                // completed calls must not reopen when later text is persisted.
                let mut additions = message.clone();
                additions.blocks.retain(|block| match block {
                    ContentBlock::ToolUse { id, .. } => !old_calls.contains(id),
                    _ => false,
                });
                let Ok((open, _)) = tool_changes(&additions, message_id, &context.open_tools)
                else {
                    return Ok(rejected("tool_association_invalid"));
                };
                self.messages
                    .get_mut(message_id)
                    .expect("validated target")
                    .message = message;
                context.open_tools = open;
            }
            HistoryEvent::Metadata { metadata } => {
                let Some(metadata) = metadata_snapshot(metadata, &context.projection.metadata)?
                else {
                    return Ok(rejected("metadata_session_identity_changed"));
                };
                context.projection.metadata = metadata;
            }
            HistoryEvent::Projection { metadata, messages } => {
                if !context.open_tools.is_empty()
                    && !is_recovery_projection(messages, &context.projection.chain.ids())?
                {
                    return Ok(rejected("unfinished_context_requires_preserving_recovery"));
                }
                let Some(metadata) = metadata_snapshot(metadata, &context.projection.metadata)?
                else {
                    return Ok(rejected("projection_session_identity_changed"));
                };
                let source: BTreeSet<_> = context.projection.chain.ids().into_iter().collect();
                let mut ids = BTreeSet::new();
                let mut nodes = Vec::new();
                let mut chain = Arc::new(MessageChain::default());
                let mut open = BTreeMap::new();
                for entry in messages {
                    if !valid_id(&entry.id) || !ids.insert(entry.id.clone()) {
                        return Ok(rejected("projection_message_identity_conflict"));
                    }
                    let message = match &entry.message {
                        Some(value) if !self.messages.contains_key(&entry.id) => {
                            decode_message(value)?
                        }
                        None if source.contains(&entry.id) => self
                            .messages
                            .get(&entry.id)
                            .expect("source node")
                            .message
                            .clone(),
                        _ => return Ok(rejected("projection_source_message_changed")),
                    };
                    let Ok((next_open, origins)) = tool_changes(&message, &entry.id, &open) else {
                        return Ok(rejected("projection_tool_association_invalid"));
                    };
                    open = next_open;
                    if entry.message.is_none()
                        && self
                            .messages
                            .get(&entry.id)
                            .expect("retained node")
                            .tool_origins
                            != origins
                    {
                        return Ok(rejected("projection_tool_association_changed"));
                    }
                    if entry.message.is_some() {
                        nodes.push((
                            entry.id.clone(),
                            MessageNode {
                                message,
                                parent: chain.tail.last().cloned(),
                                tool_origins: origins,
                            },
                        ));
                    }
                    MessageChain::append(&mut chain, entry.id.clone());
                }
                if !open.is_empty() {
                    return Ok(rejected("projection_has_unfinished_tools"));
                }
                context.unsealed.retain(|id| ids.contains(id));
                for (id, node) in nodes {
                    context.unsealed.insert(id.clone());
                    self.messages.insert(id, node);
                }
                context.projection = Projection { chain, metadata };
                context.open_tools = open;
            }
            HistoryEvent::CommitTurn { trace_id } => {
                if trace_id.is_empty() || trace_id.len() > 512 {
                    return Ok(rejected("completed_turn_identity_invalid"));
                }
                if !context.open_tools.is_empty() {
                    return Ok(rejected("context_has_unfinished_tools"));
                }
            }
            HistoryEvent::Initialize { .. } | HistoryEvent::OpenContext { .. } => unreachable!(),
        }
        touch(&mut context.projection, record.timestamp_ms);
        context.revision.clone_from(&record.id);
        if !matches!(record.event, HistoryEvent::Metadata { .. }) {
            self.latest_context = Some(record.context_id.clone());
        }
        if matches!(record.event, HistoryEvent::CommitTurn { .. }) {
            self.commits.insert(
                record.id.clone(),
                CompletedProjection {
                    projection: context.projection.clone(),
                    turn_id: record.id.clone(),
                },
            );
            self.latest_commit = Some(record.id.clone());
            context.base_commit.clone_from(&record.id);
            context.unsealed.clear();
            context.inherited_unfinished = false;
        } else if was_completed
            && matches!(
                record.event,
                HistoryEvent::Metadata { .. } | HistoryEvent::Projection { .. }
            )
        {
            let turn_id = self
                .commits
                .get(&context.base_commit)
                .ok_or_else(|| invalid("completed context source missing"))?
                .turn_id
                .clone();
            let is_selected_turn = self
                .latest_commit
                .as_ref()
                .and_then(|id| self.commits.get(id))
                .is_some_and(|selected| selected.turn_id == turn_id);
            self.commits.insert(
                record.id.clone(),
                CompletedProjection {
                    projection: context.projection.clone(),
                    turn_id,
                },
            );
            if is_selected_turn {
                self.latest_commit = Some(record.id.clone());
            }
            context.base_commit.clone_from(&record.id);
            context.unsealed.clear();
        }
        Ok(RecordOutcome::Applied)
    }

    pub fn message_ids(&self, context: &str) -> io::Result<Vec<String>> {
        Ok(self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?
            .projection
            .chain
            .ids())
    }

    pub fn message_head(&self, context: &str) -> io::Result<Option<&str>> {
        Ok(self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?
            .projection
            .chain
            .tail
            .last()
            .map(String::as_str))
    }

    pub(super) fn context_metadata(&self, context: &str) -> io::Result<&Value> {
        Ok(&self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?
            .projection
            .metadata)
    }

    pub(super) fn has_open_tools(&self, context: &str) -> io::Result<bool> {
        Ok(!self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?
            .open_tools
            .is_empty())
    }

    pub(super) fn has_uncommitted_messages(&self, context: &str) -> io::Result<bool> {
        Ok(!self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?
            .unsealed
            .is_empty())
    }

    /// Applied revision ancestry includes the completed source of a fork.
    pub fn ancestry(&self, revision: &str) -> io::Result<Vec<String>> {
        let mut result = Vec::new();
        let mut next = Some(revision);
        while let Some(id) = next {
            let parent = self
                .parents
                .get(id)
                .ok_or_else(|| invalid("history revision missing"))?;
            result.push(id.into());
            next = parent.as_deref();
        }
        Ok(result)
    }

    /// Read complete durable records; an interrupted final frame is not visible.
    pub fn decode(contents: &[u8]) -> io::Result<Self> {
        let mut graph = Self::default();
        for frame in contents.split_inclusive(|byte| *byte == b'\n') {
            if frame.last() != Some(&b'\n') {
                break;
            }
            if frame.len() > 1 {
                let record = serde_json::from_slice(frame).map_err(io::Error::other)?;
                graph.apply(&record)?;
            }
        }
        Ok(graph)
    }

    fn session(&self, projection: &Projection) -> io::Result<Session> {
        let mut value = (*projection.metadata).clone();
        value["messages"] = Value::Array(
            projection
                .chain
                .ids()
                .iter()
                .map(|id| {
                    let node = self.messages.get(id).expect("indexed message");
                    let mut message = to_value(&node.message)?;
                    message["history"] =
                        serde_json::to_value(crate::session_history::MessageIdentity {
                            id: id.clone(),
                            parent_id: node.parent.clone(),
                            tool_origins: node.tool_origins.clone(),
                        })
                        .map_err(io::Error::other)?;
                    Ok(message)
                })
                .collect::<io::Result<_>>()?,
        );
        Session::from_json(&JsonValue::parse(&value.to_string()).map_err(io::Error::other)?)
            .map_err(io::Error::other)
    }

    pub fn completed_session(&self, commit: Option<&str>) -> io::Result<Session> {
        let commit = commit
            .or(self.latest_commit())
            .ok_or_else(|| invalid("no completed context"))?;
        self.session(
            &self
                .commits
                .get(commit)
                .ok_or_else(|| invalid("completed context missing"))?
                .projection,
        )
        .map(|session| session.with_history_head(Some(commit.into())))
    }

    /// The continuation most recently advanced by a message, turn or projection.
    pub fn resume_session(&self) -> io::Result<Session> {
        self.context_session(
            self.latest_context
                .as_deref()
                .ok_or_else(|| invalid("no resume context"))?,
        )
    }

    pub fn context_session(&self, context: &str) -> io::Result<Session> {
        self.session(
            &self
                .contexts
                .get(context)
                .ok_or_else(|| invalid("context missing"))?
                .projection,
        )
        .map(|session| session.with_history_head(self.revision(context).map(str::to_owned)))
    }

    pub fn revision_session(&self, revision: &str) -> io::Result<Session> {
        if self.commits.contains_key(revision) {
            return self.completed_session(Some(revision));
        }
        let context = self
            .contexts
            .iter()
            .find(|(_, context)| context.revision == revision)
            .map(|(id, _)| id)
            .ok_or_else(|| invalid("history revision has no current context"))?;
        self.context_session(context)
    }

    pub fn prepare_message(
        &self,
        context: &str,
        message: &ConversationMessage,
    ) -> io::Result<HistoryRecord> {
        let current = self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?;
        let parent_id = current.projection.chain.tail.last().cloned();
        if message
            .history
            .as_ref()
            .is_some_and(|identity| identity.parent_id != parent_id)
        {
            return Err(invalid("message parent changed"));
        }
        Ok(HistoryRecord::new(
            context.into(),
            Some(current.revision.clone()),
            HistoryEvent::Message {
                id: message.history.as_ref().map_or_else(
                    || uuid::Uuid::new_v4().to_string(),
                    |identity| identity.id.clone(),
                ),
                parent_id,
                message: to_value(message)?,
            },
        ))
    }

    /// Bind each candidate message to its retained source ID, or `None` for new
    /// content. Identical payloads cannot establish which source message survived.
    pub fn prepare_projection(
        &self,
        context: &str,
        source_revision: &str,
        session: &Session,
        retained_ids: &[Option<&str>],
    ) -> io::Result<HistoryRecord> {
        let current = self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?;
        if current.revision != source_revision {
            return Err(invalid("compaction source context changed"));
        }
        if retained_ids.len() != session.messages.len() {
            return Err(invalid("projection message identity count differs"));
        }
        let source: HashSet<_> = current.projection.chain.ids().into_iter().collect();
        let mut used = HashSet::new();
        let messages = session
            .messages
            .iter()
            .zip(retained_ids)
            .map(|(message, retained)| {
                let payload = to_value(message)?;
                match retained {
                    Some(id) if source.contains(*id) && used.insert(*id) => {
                        let original = self.messages.get(*id).expect("source node");
                        if to_value(&original.message)? != payload {
                            return Err(invalid("retained projection message changed"));
                        }
                        Ok(ProjectionMessage {
                            id: (*id).into(),
                            message: None,
                        })
                    }
                    Some(_) => Err(invalid(
                        "retained projection message is not a unique source",
                    )),
                    None => Ok(ProjectionMessage {
                        id: message.history.as_ref().map_or_else(
                            || uuid::Uuid::new_v4().to_string(),
                            |identity| identity.id.clone(),
                        ),
                        message: Some(payload),
                    }),
                }
            })
            .collect::<io::Result<_>>()?;
        let metadata = metadata_value(session)?;
        Ok(HistoryRecord::new(
            context.into(),
            Some(source_revision.into()),
            HistoryEvent::Projection { metadata, messages },
        ))
    }

    /// Change this context's metadata without replacing its message view or
    /// making an old completed turn displace a newer sibling continuation.
    pub fn prepare_metadata(&self, context: &str, session: &Session) -> io::Result<HistoryRecord> {
        let current = self
            .contexts
            .get(context)
            .ok_or_else(|| invalid("context missing"))?;
        Ok(HistoryRecord::new(
            context.into(),
            Some(current.revision.clone()),
            HistoryEvent::Metadata {
                metadata: metadata_value(session)?,
            },
        ))
    }
}
