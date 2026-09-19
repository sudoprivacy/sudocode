//! Live sub-agent events: what a spawned agent is doing, reported to the
//! renderer that drives the turn which spawned it.
//!
//! A sub-agent runs its own `ConversationRuntime` (see `tools::agent_spawn`),
//! so its text, thinking and tool calls never reach the parent's
//! [`RuntimeObserver`](crate::RuntimeObserver). A renderer that wants them
//! returns a [`SubagentSink`] from `RuntimeObserver::subagent_sink`; the
//! runtime threads it through [`ToolDispatchContext`](crate::ToolDispatchContext)
//! and the Agent tool attaches an observer to the child that forwards into it.
//! No sink (the default) means nothing is forwarded and the child runs exactly
//! as it did before this module existed.
//!
//! The data is plain and structured; the renderer decides the wire shape (the
//! ACP contract lives with `engine-acp`).

use std::sync::{Arc, Mutex, PoisonError};

/// One update from a sub-agent, or about one.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentEvent {
    /// The sub-agent stream this update belongs to. `None` only for the
    /// lifecycle of a top-level agent, which belongs to the parent session.
    pub stream: Option<SubagentStreamRef>,
    pub update: SubagentUpdate,
}

/// Where an update sits: which agent's stream, spawned by which call, and its
/// position in that agent's sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStreamRef {
    /// The client-visible id of the call that spawned `agent_id`.
    pub parent_tool_call_id: String,
    pub agent_id: String,
    /// Strictly increasing per agent, from 0, shared with the agent's
    /// lifecycle events.
    pub seq: u64,
}

/// What happened.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentUpdate {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    /// `id` is already namespaced (`<agentId>:<raw id>`), so it never collides
    /// with a parent's or a sibling's call.
    ToolCall {
        id: String,
        name: String,
        input: String,
    },
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Lifecycle(Box<SubagentLifecycle>),
}

/// Start or end of one spawned agent, reported against the call that spawned
/// it.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentLifecycle {
    /// Client-visible id of the spawning call.
    pub tool_call_id: String,
    pub phase: SubagentPhase,
    /// Position in the spawned agent's own sequence (0 for `Started`).
    pub seq: u64,
    pub agent: SubagentIdentity,
    /// `completed` | `failed` | `cancelled`; `None` for `Started`.
    pub status: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// The final `AgentOutput` manifest (with the result text) on `Finished`.
    pub raw_output: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentPhase {
    Started,
    Finished,
}

impl SubagentPhase {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Finished => "finished",
        }
    }
}

/// Who the spawned agent is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentIdentity {
    pub agent_id: String,
    pub name: String,
    pub description: String,
    pub subagent_type: Option<String>,
    pub model: Option<String>,
    pub color: Option<String>,
    /// Whether the model asked for a background run.
    pub background: bool,
}

/// One spawned agent's stream: its id, the call that spawned it, and its
/// sequence counter. Shared by everything that emits on the agent's behalf.
#[derive(Debug)]
pub struct SubagentScope {
    agent_id: String,
    parent_tool_call_id: String,
    next_seq: Mutex<u64>,
}

impl SubagentScope {
    #[must_use]
    pub fn new(agent_id: impl Into<String>, parent_tool_call_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            parent_tool_call_id: parent_tool_call_id.into(),
            next_seq: Mutex::new(0),
        }
    }

    #[must_use]
    pub fn parent_tool_call_id(&self) -> &str {
        &self.parent_tool_call_id
    }

    fn lock_seq(&self) -> std::sync::MutexGuard<'_, u64> {
        self.next_seq.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where sub-agent events go. Cheap to clone.
///
/// A sink is either the renderer's top-level one (no scope: the parent
/// session) or scoped to one spawned agent's stream. Emitting takes the
/// scope's sequence lock for the whole send, so one agent's events arrive in
/// `seq` order even when a nested agent's lifecycle is reported from another
/// thread.
#[derive(Clone)]
pub struct SubagentSink {
    emit: Arc<dyn Fn(SubagentEvent) + Send + Sync>,
    scope: Option<Arc<SubagentScope>>,
}

impl SubagentSink {
    /// The renderer's top-level sink.
    pub fn new(f: impl Fn(SubagentEvent) + Send + Sync + 'static) -> Self {
        Self {
            emit: Arc::new(f),
            scope: None,
        }
    }

    /// The same destination, scoped to `scope`'s stream.
    #[must_use]
    pub fn scoped(&self, scope: Arc<SubagentScope>) -> Self {
        Self {
            emit: Arc::clone(&self.emit),
            scope: Some(scope),
        }
    }

    /// The id a client sees for a call made inside this sink's stream.
    #[must_use]
    pub fn visible_tool_call_id(&self, raw: &str) -> String {
        match &self.scope {
            Some(scope) => format!("{}:{raw}", scope.agent_id),
            None => raw.to_string(),
        }
    }

    /// Emit an update of this sink's own stream. A top-level sink has no
    /// stream, so this is a no-op there.
    pub fn emit_in_scope(&self, update: SubagentUpdate) {
        let Some(scope) = &self.scope else {
            return;
        };
        let mut seq = scope.lock_seq();
        let event = SubagentEvent {
            stream: Some(SubagentStreamRef {
                parent_tool_call_id: scope.parent_tool_call_id.clone(),
                agent_id: scope.agent_id.clone(),
                seq: *seq,
            }),
            update,
        };
        *seq += 1;
        (self.emit)(event);
    }

    /// Report the start or end of `child`, an agent spawned from this sink's
    /// stream. The lifecycle takes its `seq` from `child`; when this sink is
    /// itself a sub-agent's stream the event also belongs to that stream.
    pub fn emit_lifecycle(&self, child: &SubagentScope, mut lifecycle: SubagentLifecycle) {
        let mut child_seq = child.lock_seq();
        lifecycle.seq = *child_seq;
        *child_seq += 1;
        let (stream, _guard) = match &self.scope {
            Some(scope) => {
                let mut seq = scope.lock_seq();
                let stream = SubagentStreamRef {
                    parent_tool_call_id: scope.parent_tool_call_id.clone(),
                    agent_id: scope.agent_id.clone(),
                    seq: *seq,
                };
                *seq += 1;
                (Some(stream), Some(seq))
            }
            None => (None, None),
        };
        (self.emit)(SubagentEvent {
            stream,
            update: SubagentUpdate::Lifecycle(Box::new(lifecycle)),
        });
    }
}

impl std::fmt::Debug for SubagentSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentSink")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}
