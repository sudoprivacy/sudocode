//! Durable handoff of direct sub-agent results. This is delivery state only;
//! the child's manifest remains the authority for its execution outcome.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_backend::FsBackend;
use crate::{ContentBlock, Session, SubagentLifecycle, SubagentPhase};

mod control;
mod terminal;
use terminal::TerminalSource;
pub use terminal::{ShellKind, TaskCompletion, TerminalCompletion, TerminalSnapshot};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Worker {
    agent_id: String,
    background: bool,
    finished: Option<SubagentLifecycle>,
}

/// Shared qualification rules for volatile and durable engine sessions.
/// The engine owns turn admission, event observation and closure.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Completions {
    active: bool,
    workers: BTreeMap<String, Worker>,
    collected: Vec<String>,
}

impl Completions {
    pub fn begin(&mut self) {
        self.active = true;
    }

    pub fn lifecycle(&mut self, event: &SubagentLifecycle) {
        match event.phase {
            SubagentPhase::Started => {
                self.workers
                    .entry(event.tool_call_id.clone())
                    .or_insert_with(|| Worker {
                        agent_id: event.agent.agent_id.clone(),
                        ..Worker::default()
                    });
            }
            SubagentPhase::Finished => {
                if let Some(worker) = self.workers.get_mut(&event.tool_call_id) {
                    worker.finished = Some(event.clone());
                }
                self.suppress_collected();
            }
        }
    }

    pub fn tool_result(&mut self, id: &str, name: &str, output: &str) {
        let value = serde_json::from_str::<serde_json::Value>(output).ok();
        if let Some(worker) = self.workers.get_mut(id) {
            if value
                .as_ref()
                .is_some_and(|v| matches!(v["status"].as_str(), Some("running" | "backgrounded")))
            {
                worker.background = true;
            } else {
                self.workers.remove(id);
            }
        }
        // Only an explicit read consumes a result. An unrelated tool returning
        // similarly shaped JSON must not erase a pending completion.
        if crate::tool_names::canonicalize_tool_name(name) == "pid_output" {
            if let Some(value) = value {
                if matches!(
                    value["status"].as_str(),
                    Some("completed" | "failed" | "killed" | "cancelled")
                ) {
                    if let Some(agent_id) = value["agentId"]
                        .as_str()
                        .or_else(|| value["agent_id"].as_str())
                    {
                        self.workers.retain(|_, worker| worker.agent_id != agent_id);
                    }
                }
            } else if output.starts_with("<task-notification>") {
                self.collected.push(output.to_owned());
                self.suppress_collected();
            }
        }
    }

    fn suppress_collected(&mut self) {
        self.workers.retain(|_, worker| {
            !worker
                .finished
                .as_ref()
                .and_then(|event| event.completion_notification.as_ref())
                .is_some_and(|text| self.collected.contains(text))
        });
    }

    pub fn close(&mut self) {
        self.active = false;
    }

    pub fn drain(&mut self) -> Vec<SubagentLifecycle> {
        if self.active {
            return Vec::new();
        }
        let mut ready = Vec::new();
        self.workers.retain(|_, worker| {
            if worker.background {
                if let Some(finished) = worker.finished.take() {
                    ready.push(finished);
                    return false;
                }
            }
            true
        });
        if self.workers.is_empty() {
            self.collected.clear();
        }
        ready
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextRef {
    pub session_id: String,
    pub revision: u64,
    pub digest: String,
    pub last_trace_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Completion {
    pub session_id: String,
    pub completion_id: String,
    pub agent_id: String,
    pub parent_tool_call_id: String,
    pub origin_trace_id: String,
    pub status: String,
    pub message: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Source {
    ordinal: u64,
    #[serde(default)]
    lifecycle: Option<SubagentLifecycle>,
    #[serde(default)]
    terminal: Option<TerminalSource>,
    #[serde(default)]
    suppressed: bool,
    trace_id: String,
    manifest_path: String,
    closed: bool,
    settled: bool,
    payload: Option<TaskCompletion>,
    ack: Option<String>,
    blocked: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Journal {
    version: u32,
    #[serde(default)]
    storage_ref: String,
    #[serde(default)]
    deleted: Option<String>,
    #[serde(default)]
    closed_traces: BTreeSet<String>,
    watermark: u64,
    session_id: String,
    cwd: PathBuf,
    context: Option<ContextRef>,
    active_trace: Option<String>,
    reducer: Completions,
    sources: BTreeMap<String, Source>,
}

// Session writes must reject a journal this executable cannot preserve, even
// when ACP completion delivery was not negotiated (for example after rollback).
pub(crate) fn validate_journal(value: serde_json::Value) -> Result<(), String> {
    serde_json::from_value::<Journal>(value)
        .map(|_| ())
        .map_err(|error| format!("completion_journal_unsupported: {error}"))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStatus {
    pub watermark: u64,
    pub waiting: usize,
    pub pending: usize,
    pub unresolved: usize,
    /// Exact source ownership for controller stop controls, including blocked
    /// delivery. Session-wide counts cannot identify which old root is active.
    pub unsettled_by_trace: BTreeMap<String, bool>,
    pub settled: usize,
    pub blocked: Vec<String>,
    pub context_ref: Option<ContextRef>,
    pub storage_ref: String,
    pub execution_unconfirmed: usize,
    pub running: usize,
    pub terminal_sources: Vec<serde_json::Value>,
    pub task_set_hash: String,
    pub next_task_id: Option<String>,
}

/// One storage-level writer per exact session, shared by transcript writes,
/// registration and ACK handling. Only the operation mutex is released for ACK waits.
pub struct CompletionJournal {
    fs: Arc<dyn FsBackend>,
    path: String,
    transcript: PathBuf,
    state: Mutex<Journal>,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    clear_cache: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    instance: String,
    task_protocol: std::sync::atomic::AtomicBool,
    writer: Arc<crate::session_writer::SessionWriter>,
}

impl CompletionJournal {
    pub fn set_task_protocol(&self, enabled: bool) {
        self.task_protocol
            .store(enabled, std::sync::atomic::Ordering::Release);
    }

    pub fn deleted(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deleted
            .is_some()
    }

    pub fn tasks_enabled(&self) -> bool {
        self.task_protocol
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn set_cache_clear(&self, clear: impl Fn() + Send + Sync + 'static) {
        *self
            .clear_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(clear));
    }

    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(wake));
    }

    pub fn open(session: &Session, cwd: &Path, create: bool) -> Result<Arc<Self>, String> {
        let transcript = session
            .persistence_path()
            .ok_or("session has no transcript")?
            .to_path_buf();
        let fs = session.fs_handle();
        let writer = session.completion_writer()?;
        let path = transcript
            .with_extension("completions.json")
            .to_string_lossy()
            .into_owned();
        let state = if fs.exists(&path).map_err(|e| e.to_string())? {
            let state: Journal =
                serde_json::from_str(&fs.read_to_string(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if state.version != crate::session_writer::JOURNAL_VERSION
                || state.session_id != session.session_id
                || state.cwd != cwd
            {
                return Err("completion journal identity mismatch".into());
            }
            state
        } else if create {
            Journal {
                version: crate::session_writer::JOURNAL_VERSION,
                storage_ref: checksum(
                    &serde_json::to_vec(&(1, &session.session_id, &transcript))
                        .map_err(|e| e.to_string())?,
                ),
                deleted: None,
                closed_traces: BTreeSet::new(),
                watermark: 0,
                session_id: session.session_id.clone(),
                cwd: cwd.to_path_buf(),
                context: None,
                active_trace: None,
                reducer: Completions::default(),
                sources: BTreeMap::new(),
            }
        } else {
            return Err("completion journal missing".into());
        };
        let interrupted = state.active_trace.clone();
        let journal = Arc::new(Self {
            fs,
            path,
            transcript,
            state: Mutex::new(state),
            wake: Mutex::new(None),
            clear_cache: Mutex::new(None),
            instance: uuid::Uuid::new_v4().to_string(),
            task_protocol: std::sync::atomic::AtomicBool::new(false),
            writer,
        });
        // Opening a new writer means the previous process no longer owns this
        // journal. Its interrupted turn cannot publish, but must not prevent a
        // later explicit human turn from repairing the conversation.
        if let Some(trace) = interrupted {
            journal.fail_turn(&trace)?;
        }
        journal.change(|state| {
            for source in state.sources.values_mut() {
                if let Some(terminal) = &mut source.terminal {
                    if terminal.snapshot.is_none() && terminal.state != "not_started" {
                        terminal.state = "execution_unconfirmed".into();
                        if !source.settled && source.blocked.is_none() {
                            source.blocked = Some("execution_unconfirmed".into());
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(journal)
    }

    fn change<T>(&self, f: impl FnOnce(&mut Journal) -> Result<T, String>) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "completion journal poisoned")?;
        let write = self.writer.write()?;
        // A failed write must not leave an in-memory ACK/publication that the
        // next mutation could silently treat as durable.
        let mut next = state.clone();
        next.watermark += 1;
        let result = f(&mut next)?;
        let watermark = next.watermark;
        next.watermark = state.watermark;
        // The first write establishes the durable storage identity even when
        // the new session has not started any tasks yet.
        if state.watermark != 0
            && serde_json::to_vec(&next).map_err(|e| e.to_string())?
                == serde_json::to_vec(&*state).map_err(|e| e.to_string())?
        {
            return Ok(result);
        }
        next.watermark = watermark;
        Self::check_reservation(&next)?;
        let bytes = serde_json::to_vec(&next).map_err(|e| e.to_string())?;
        if bytes.len() > 64 * 1024 * 1024 {
            return Err("completion_journal_capacity: exceeds 64 MiB".into());
        }
        self.fs
            .write_atomic(&self.path, &bytes)
            .map_err(|e| e.to_string())?;
        *state = next;
        drop(state);
        drop(write);
        if let Some(wake) = self
            .wake
            .lock()
            .map_err(|_| "completion wake poisoned")?
            .as_ref()
        {
            wake();
        }
        Ok(result)
    }

    pub fn matches_identity(&self, session: &Session, cwd: &Path) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.session_id == session.session_id
            && state.cwd == cwd
            && session.persistence_path() == Some(self.transcript.as_path())
    }

    pub fn attach_session(&self, session: &Session, cwd: &Path) -> Result<(), String> {
        if !self.matches_identity(session, cwd) {
            return Err("completion journal identity mismatch".into());
        }
        session.adopt_completion_writer(Arc::clone(&self.writer))
    }

    pub fn begin(&self, trace: &str, expected: Option<&ContextRef>) -> Result<(), String> {
        if trace.is_empty() {
            return Err("completion prompt requires traceId".into());
        }
        let digest = self.digest()?;
        self.change(|state| {
            if let Some(expected) = expected {
                if state.context.as_ref() != Some(expected)
                    || digest != expected.digest
                    || state.active_trace.is_some()
                {
                    return Err(
                        "[context_mismatch] expectedContext does not match committed session"
                            .into(),
                    );
                }
            }
            if state.active_trace.is_some() {
                return Err("source_context_unconfirmed".into());
            }
            if state.deleted.is_some() {
                return Err("session_deleted".into());
            }
            if state.closed_traces.contains(trace) {
                return Err("completion trace already closed".into());
            }
            if trace.len() > 512 || state.closed_traces.len() >= 10000 {
                return Err("completion_journal_capacity: trace limit".into());
            }
            state.active_trace = Some(trace.to_owned());
            state.reducer.begin();
            Ok(())
        })
    }

    /// Called synchronously BEFORE the child executes. A storage failure is a
    /// tool error and must prevent the spawn, including synchronous children.
    pub fn register(
        &self,
        lifecycle: SubagentLifecycle,
        manifest_path: String,
    ) -> Result<(), String> {
        if lifecycle.tool_call_id.is_empty()
            || lifecycle.tool_call_id.len() > 512
            || lifecycle.agent.agent_id.is_empty()
            || lifecycle.agent.agent_id.len() > 512
            || serde_json::to_vec(&lifecycle)
                .map_err(|e| e.to_string())?
                .len()
                > 16 * 1024
            || manifest_path.len() > 4096
        {
            return Err("completion_registration_capacity".into());
        }
        self.change(|state| {
            let trace_id = state
                .active_trace
                .clone()
                .ok_or("no active completion source")?;
            if state.sources.len() >= 10000 {
                return Err(
                    "completion_journal_capacity: session has 10000 registered children".into(),
                );
            }
            if state.sources.contains_key(&lifecycle.tool_call_id) {
                return Err("duplicate spawn tool id".into());
            }
            state.reducer.lifecycle(&lifecycle);
            state.sources.insert(
                lifecycle.tool_call_id.clone(),
                Source {
                    ordinal: 0,
                    lifecycle: Some(lifecycle),
                    terminal: None,
                    suppressed: false,
                    trace_id,
                    manifest_path,
                    closed: false,
                    settled: false,
                    payload: None,
                    ack: None,
                    blocked: None,
                },
            );
            Ok(())
        })
    }

    pub fn lifecycle(&self, event: &SubagentLifecycle) -> Result<(), String> {
        if event.completion_notification.is_none()
            || event.phase == SubagentPhase::Started
            || !matches!(
                event.status.as_deref(),
                Some("completed" | "failed" | "killed" | "cancelled")
            )
        {
            return Ok(());
        } // registration is already durable
        self.change(|state| {
            if !state.sources.contains_key(&event.tool_call_id) {
                return Ok(());
            }
            if let Some(source) = state.sources.get_mut(&event.tool_call_id) {
                if source.payload.is_some() || source.settled {
                    return Ok(());
                }
                if source.closed {
                    source.blocked = None;
                }
            }
            let mut bounded = event.clone();
            bounded.raw_output = None;
            state.reducer.lifecycle(&bounded);
            Self::publish(state)
        })
    }

    pub fn tool_result(&self, id: &str, name: &str, output: &str) -> Result<(), String> {
        if self.deleted() {
            return Ok(());
        }
        self.change(|state| {
            state.reducer.tool_result(id, name, output);
            // Auto-backgrounding is observed at tool handoff, after registration.
            // Keep that fact on the source: fail_turn removes reducer workers
            // to block publication, but does not end the child's execution.
            if state.reducer.workers.get(id).is_some_and(|w| w.background) {
                if let Some(lifecycle) =
                    state.sources.get_mut(id).and_then(|s| s.lifecycle.as_mut())
                {
                    lifecycle.agent.background = true;
                }
            }
            Self::terminal_tool_result(state, id, name, output);
            Self::publish(state)
        })
    }

    /// Called only after the turn event channel drains AND the transcript is
    /// committed. The context digest covers the persisted, recoverable bytes.
    pub fn commit(&self, trace: &str) -> Result<ContextRef, String> {
        let digest = self.digest()?;
        self.change(|state| {
            if state.active_trace.as_deref() != Some(trace) {
                return Err("completion turn identity mismatch".into());
            }
            let context = ContextRef {
                session_id: state.session_id.clone(),
                revision: state.context.as_ref().map_or(1, |c| c.revision + 1),
                digest,
                last_trace_id: trace.to_owned(),
            };
            state.context = Some(context.clone());
            state.active_trace = None;
            state.closed_traces.insert(trace.to_owned());
            for source in state.sources.values_mut().filter(|s| s.trace_id == trace) {
                source.closed = true;
            }
            for source in state.sources.values_mut() {
                if let Some(terminal) = &mut source.terminal {
                    if terminal.collected_in.as_deref() == Some(trace) {
                        terminal.collected = true;
                        terminal.collected_in = None;
                    }
                }
            }
            state.reducer.close();
            Self::publish(state)?;
            Ok(context)
        })
    }

    pub fn fail_turn(&self, trace: &str) -> Result<(), String> {
        self.change(|state| {
            if state.active_trace.as_deref() == Some(trace) {
                state.active_trace = None;
                state.context = None;
                state.closed_traces.insert(trace.to_owned());
                for source in state.sources.values_mut() {
                    if let Some(terminal) = &mut source.terminal {
                        if terminal.collected_in.as_deref() == Some(trace) {
                            terminal.collected_in = None;
                        }
                    }
                }
                state.reducer.close();
                for source in state.sources.values_mut().filter(|s| s.trace_id == trace) {
                    source.blocked = Some("source_context_unconfirmed".into());
                }
                // Keep those workers inert; a failed parent did not commit a
                // delivery boundary. A later human turn can still proceed.
                state
                    .reducer
                    .workers
                    .retain(|tool, _| state.sources.get(tool).is_none_or(|s| s.trace_id != trace));
            }
            Ok(())
        })
    }

    pub fn active(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_trace
            .is_some()
    }

    fn publish(state: &mut Journal) -> Result<(), String> {
        if state.deleted.is_some() {
            return Ok(());
        }
        for event in state.reducer.drain() {
            let source = state
                .sources
                .get_mut(&event.tool_call_id)
                .ok_or("completion source missing")?;
            if !source.closed || source.payload.is_some() || source.settled {
                continue;
            }
            let message = event
                .completion_notification
                .ok_or("terminal completion text missing")?;
            if message.len() > 65536 {
                return Err("completion message exceeds 64 KiB".into());
            }
            let id = checksum(
                &serde_json::to_vec(&(
                    1,
                    &state.session_id,
                    &event.tool_call_id,
                    &event.agent.agent_id,
                ))
                .map_err(|e| e.to_string())?,
            );
            // Order published results, not spawns: an earlier child finishing
            // late must not overtake a frozen delivery awaiting its durable ACK,
            // including when the sender restarts between attempts.
            source.ordinal = state.watermark;
            source.payload = Some(TaskCompletion::Subagent(Completion {
                session_id: state.session_id.clone(),
                completion_id: id,
                agent_id: event.agent.agent_id,
                parent_tool_call_id: event.tool_call_id,
                origin_trace_id: source.trace_id.clone(),
                status: match event.status.as_deref() {
                    Some("completed") => "completed",
                    Some("cancelled" | "killed") => "cancelled",
                    _ => "failed",
                }
                .into(),
                message,
            }));
        }
        for (tool, source) in &mut state.sources {
            if source.terminal.is_none()
                && source.closed
                && source.blocked.is_none()
                && source.payload.is_none()
                && !state.reducer.workers.contains_key(tool)
            {
                source.settled = true;
            }
        }
        Self::publish_terminals(state)
    }

    fn digest(&self) -> Result<String, String> {
        self.fs
            .read(&self.transcript.to_string_lossy())
            .map(|bytes| checksum(&bytes))
            .map_err(|e| e.to_string())
    }

    pub fn context(&self) -> Result<Option<ContextRef>, String> {
        let digest = self.digest()?;
        let state = self
            .state
            .lock()
            .map_err(|_| "completion journal poisoned")?;
        if state.active_trace.is_some()
            || state.context.as_ref().is_some_and(|c| c.digest != digest)
        {
            return Err("source_context_unconfirmed".into());
        }
        Ok(state.context.clone())
    }

    pub fn pending(&self) -> Vec<Completion> {
        self.pending_tasks()
            .into_iter()
            .filter_map(|p| match p {
                TaskCompletion::Subagent(p) => Some(p),
                TaskCompletion::Terminal(_) => None,
            })
            .collect()
    }

    pub fn pending_tasks(&self) -> Vec<TaskCompletion> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut sources: Vec<_> = state
            .sources
            .values()
            .filter(|s| !s.settled && s.ack.is_none() && s.blocked.is_none())
            .collect();
        sources.sort_by_key(|s| s.ordinal);
        sources
            .into_iter()
            .filter_map(|s| s.payload.clone())
            .collect()
    }

    pub fn acknowledge(&self, id: &str, disposition: &str) -> Result<(), String> {
        if !matches!(disposition, "stored" | "session_deleted") {
            return Err("invalid completion acknowledgement".into());
        }
        if self.deleted() {
            return Ok(());
        }
        self.change(|state| {
            let source = state
                .sources
                .values_mut()
                .find(|s| s.payload.as_ref().is_some_and(|p| p.id() == id))
                .ok_or("unknown completion acknowledgement")?;
            source.ack = Some(disposition.to_owned());
            source.settled = true;
            // Keep identity/ACK tombstones for duplicate suppression while
            // releasing the potentially large result after durable receipt.
            if let Some(payload) = &mut source.payload {
                payload.clear_message();
            }
            if let Some(lifecycle) = &mut source.lifecycle {
                lifecycle.raw_output = None;
                lifecycle.completion_notification = None;
            }
            Ok(())
        })
    }

    pub fn quarantine(&self, id: &str, reason: &str) -> Result<(), String> {
        if self.deleted() {
            return Ok(());
        }
        self.change(|state| {
            let source = state
                .sources
                .values_mut()
                .find(|s| s.payload.as_ref().is_some_and(|p| p.id() == id))
                .ok_or("unknown completion")?;
            source.blocked = Some(reason.to_owned());
            Ok(())
        })
    }

    /// Reconcile only registered children. This reads metadata, transcript and
    /// manifests; it never creates an engine or restarts a missing child.
    pub fn reconcile(
        &self,
        terminal: impl Fn(&str, &SubagentLifecycle) -> Result<Option<SubagentLifecycle>, String>,
    ) -> Result<ReplayStatus, String> {
        let session = Session::load_from_path_with(self.fs.as_ref(), &self.transcript)
            .map_err(|e| e.to_string())?;
        let digest = self.digest()?;
        self.change(|state| {
            if state.active_trace.is_some()
                || state.context.is_none()
                || state.context.as_ref().is_some_and(|c| c.digest != digest)
            {
                for source in state
                    .sources
                    .values_mut()
                    .filter(|s| !s.settled && s.payload.is_none())
                {
                    source.blocked = Some("source_context_unconfirmed".into());
                }
            } else {
                // Replaying committed reads before terminal lifecycle events also
                // handles pid_output winning the manifest/event race.
                for message in &session.messages {
                    for block in &message.blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            tool_name,
                            output,
                            ..
                        } = block
                        {
                            state.reducer.tool_result(tool_use_id, tool_name, output);
                        }
                    }
                }
                for source in state
                    .sources
                    .values_mut()
                    .filter(|s| s.closed && !s.settled && s.payload.is_none())
                {
                    let Some(lifecycle) = &source.lifecycle else {
                        continue;
                    };
                    match terminal(&source.manifest_path, lifecycle) {
                        Ok(Some(event)) => {
                            source.blocked = None;
                            state.reducer.lifecycle(&event);
                        }
                        Ok(None) => {
                            source.blocked = Some("execution_unconfirmed".into());
                        }
                        Err(_) => {
                            source.blocked = Some("terminal_manifest_unavailable".into());
                        }
                    }
                }
                Self::publish(state)?;
            }
            Ok(())
        })?;
        Ok(self.status())
    }

    pub fn status(&self) -> ReplayStatus {
        Self::status_inner(
            &self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn status_inner(state: &Journal) -> ReplayStatus {
        ReplayStatus {
            watermark: state.watermark,
            waiting: state
                .sources
                .values()
                .filter(|s| !s.settled && s.payload.is_none() && s.blocked.is_none())
                .count(),
            pending: state
                .sources
                .values()
                .filter(|s| {
                    !s.settled && s.payload.is_some() && s.ack.is_none() && s.blocked.is_none()
                })
                .count(),
            unresolved: state.sources.values().filter(|s| !s.settled).count(),
            unsettled_by_trace: state.sources.iter().fold(
                BTreeMap::new(),
                |mut traces, (tool, source)| {
                    let unsettled = !source.settled
                        && !source.suppressed
                        && (source.payload.is_some()
                            || source.terminal.as_ref().is_some_and(|t| t.backgrounded)
                            || source
                                .lifecycle
                                .as_ref()
                                .is_some_and(|e| e.agent.background)
                            || state
                                .reducer
                                .workers
                                .get(tool)
                                .is_some_and(|w| w.background));
                    // A prepared synchronous source may still hand off to the
                    // background. Do not publish false before its prompt closes.
                    if state.closed_traces.contains(&source.trace_id) || unsettled {
                        *traces.entry(source.trace_id.clone()).or_default() |= unsettled;
                    }
                    traces
                },
            ),
            settled: state.sources.values().filter(|s| s.settled).count(),
            blocked: state
                .sources
                .values()
                .filter_map(|s| s.blocked.clone())
                .take(16)
                .collect(),
            context_ref: state.context.clone(),
            storage_ref: state.storage_ref.clone(),
            execution_unconfirmed: state
                .sources
                .values()
                .filter(|s| {
                    s.terminal
                        .as_ref()
                        .is_some_and(|t| t.snapshot.is_none() && t.state == "execution_unconfirmed")
                })
                .count(),
            running: state
                .sources
                .values()
                .filter(|s| {
                    s.terminal.as_ref().is_some_and(|t| {
                        t.snapshot.is_none() && matches!(t.state.as_str(), "running" | "prepared")
                    })
                })
                .count(),
            terminal_sources: state
                .sources
                .values()
                .filter_map(Self::terminal_view)
                .take(50)
                .collect(),
            next_task_id: (state
                .sources
                .values()
                .filter(|s| s.terminal.is_some())
                .count()
                > 50)
                .then(|| {
                    state
                        .sources
                        .iter()
                        .filter(|(_, s)| s.terminal.is_some())
                        .nth(49)
                        .map(|(id, _)| id.clone())
                        .unwrap_or_default()
                }),
            task_set_hash: checksum(
                &serde_json::to_vec(
                    &state
                        .sources
                        .iter()
                        .filter_map(|(tool, s)| {
                            s.terminal
                                .as_ref()
                                .map(|t| (tool, &t.state, s.suppressed, s.settled))
                        })
                        .collect::<Vec<_>>(),
                )
                .expect("serializable task identities"),
            ),
        }
    }
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
