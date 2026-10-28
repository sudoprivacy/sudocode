use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::fs_backend::{FsBackend, StdFsBackend};
use crate::json::{JsonError, JsonValue};
use crate::prompt::SessionPromptSnapshot;
use crate::usage::{parse_usage_cost_currency, TokenUsage};

pub mod history;

const SESSION_VERSION: u32 = 1;
const UNANSWERED_TURN_TEXT: &str = "No response requested.";
/// Cold-storage retention budget (bytes) for a transcript backed by a native
/// append-log (nexus DT_STREAM). `0` = keep-forever: cold segments spill out
/// of the hot tier but are never dropped, so the whole transcript stays
/// readable. Ignored by regular-file backends.
const TRANSCRIPT_RETENTION_BYTES: u64 = 0;
static LAST_TIMESTAMP_MS: AtomicU64 = AtomicU64::new(0);

/// Speaker role associated with a persisted conversation message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Structured message content stored inside a [`Session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type, e.g. `image/png`, `image/jpeg`.
        mime_type: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    /// Thinking the server chose to encrypt instead of returning in the clear.
    ///
    /// There is nothing to render — `data` is opaque ciphertext — but it has to
    /// be replayed for the same reason a signed thinking block does: it is part
    /// of the assistant turn the server produced, and a turn replayed without it
    /// invalidates the whole cached prefix on every tool round-trip. Measured on
    /// a live route (`ladder/tools/cache_prefix_probe.py`, trigger string in the
    /// prompt): dropping it gave turn 2 `read 0 / write 3121`, replaying it gave
    /// `read 3083 / write 109`.
    ///
    /// `data` holds the JSON *text* of whatever the provider sent, the same
    /// idiom [`ContentBlock::ToolUse::input`] uses, so an opaque payload
    /// survives a round-trip through session storage without this enum having
    /// to carry a `serde_json::Value` (which is not `Eq`).
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: String,
        thought_signature: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        tool_name: String,
        output: String,
        is_error: bool,
    },
}

/// One conversation message with optional token-usage metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    /// Stable transcript identity; provider payloads use role and blocks.
    pub history: Option<Arc<crate::session_history::MessageIdentity>>,
    pub role: MessageRole,
    pub blocks: Vec<ContentBlock>,
    pub usage: Option<TokenUsage>,
    /// The model that generated this message (set only for assistant messages).
    pub model: Option<String>,
    /// Wall-clock time the turn that produced this message took, in
    /// milliseconds (set only for assistant messages). Persisted so a resumed
    /// session can report cumulative wall time — the true cost when the turn
    /// ran on local compute rather than a token-billed API.
    pub duration_ms: Option<u64>,
}

/// Metadata describing the latest compaction that summarized a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCompaction {
    pub count: u32,
    pub removed_message_count: usize,
    pub summary: String,
    pub usage: Option<TokenUsage>,
    /// Tool names the model discovered (via ToolSearch) before compaction
    /// removed the ToolSearch results from the message history. Carried
    /// forward so `extract_discovered_tool_names()` can restore them as
    /// `defer_loading: false` after compaction. Mirrors CC's
    /// `preCompactDiscoveredTools`.
    pub pre_compact_discovered_tools: BTreeSet<String>,
}

/// Provenance recorded when a session is forked from another session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFork {
    pub parent_session_id: String,
    pub branch_name: Option<String>,
}

/// A single user prompt recorded with a timestamp for history tracking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPromptEntry {
    pub timestamp_ms: u64,
    pub text: String,
}

/// The interface that created a managed session. One-shot prompts remain
/// resumable by id, but must not displace the user's interactive `latest`
/// session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Interactive,
    NonInteractive,
}

impl Default for SessionMode {
    fn default() -> Self {
        Self::Interactive
    }
}

impl SessionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::NonInteractive => "non_interactive",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "interactive" => Some(Self::Interactive),
            "non_interactive" => Some(Self::NonInteractive),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct SessionPersistence {
    path: PathBuf,
    fs: Arc<dyn FsBackend>,
    writer: Arc<std::sync::Mutex<SessionWriterState>>,
    committed_messages: Arc<std::sync::Mutex<BTreeSet<String>>>,
    checkpoint: Arc<std::sync::Mutex<Option<CheckpointReceipt>>>,
}

#[derive(Default)]
enum SessionWriterState {
    #[default]
    Unopened,
    Active(Arc<crate::session_writer::SessionWriter>),
    Closed,
}

#[derive(Clone)]
struct CheckpointReceipt {
    head: String,
    view_revision: u64,
    metadata_hash: [u8; 32],
}

impl fmt::Debug for SessionPersistence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionPersistence")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Durable ownership and repository bindings for a hosted transcript.
/// Procfs paths change with each pid; these bindings must match before a host
/// may relocate a session to its new process workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedSessionIdentity {
    /// The Agent that created this transcript. Other Agents associate it by link.
    pub agent_name: String,
    pub owner_id: String,
    pub zone_id: String,
    pub repos: BTreeMap<String, String>,
}

impl HostedSessionIdentity {
    pub(crate) fn matches_storage_binding(&self, other: &Self) -> bool {
        self.owner_id == other.owner_id
            && self.zone_id == other.zone_id
            && self.repos == other.repos
    }

    fn to_json(&self) -> JsonValue {
        JsonValue::Object(BTreeMap::from([
            (
                "agent_name".into(),
                JsonValue::String(self.agent_name.clone()),
            ),
            ("owner_id".into(), JsonValue::String(self.owner_id.clone())),
            ("zone_id".into(), JsonValue::String(self.zone_id.clone())),
            (
                "repos".into(),
                JsonValue::Object(
                    self.repos
                        .iter()
                        .map(|(alias, path)| (alias.clone(), JsonValue::String(path.clone())))
                        .collect(),
                ),
            ),
        ]))
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("session identity must be an object".into()))?;
        let repos = object
            .get("repos")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| SessionError::Format("session identity is missing repos".into()))?
            .iter()
            .map(|(alias, value)| {
                value
                    .as_str()
                    .map(|path| (alias.clone(), path.to_string()))
                    .ok_or_else(|| {
                        SessionError::Format("session repo must be a path string".into())
                    })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            agent_name: required_string(object, "agent_name")?,
            owner_id: required_string(object, "owner_id")?,
            zone_id: required_string(object, "zone_id")?,
            repos,
        })
    }
}

/// One real model HTTP attempt during context maintenance. Missing usage means
/// the provider did not report a bill, rather than a zero-cost request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceUsageReceipt {
    pub run_id: String,
    pub attempt_id: u32,
    pub usage: Option<TokenUsage>,
}

impl MaintenanceUsageReceipt {
    fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::from([
            ("run_id".into(), JsonValue::String(self.run_id.clone())),
            (
                "attempt_id".into(),
                JsonValue::Number(i64::from(self.attempt_id)),
            ),
        ]);
        if let Some(usage) = self.usage {
            object.insert("usage".into(), usage_to_json(usage));
        }
        JsonValue::Object(object)
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("maintenance receipt must be an object".into()))?;
        Ok(Self {
            run_id: required_string(object, "run_id")?,
            attempt_id: required_u32(object, "attempt_id")?,
            usage: object.get("usage").map(usage_from_json).transpose()?,
        })
    }
}

/// Evidence for a previously committed, target-achieving compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionAchievement {
    pub fingerprint: String,
    pub before_history: usize,
    pub target_history: usize,
    pub after_history: usize,
    pub policy_version: String,
}

impl CompactionAchievement {
    fn to_json(&self) -> Result<JsonValue, SessionError> {
        Ok(JsonValue::Object(BTreeMap::from([
            (
                "fingerprint".into(),
                JsonValue::String(self.fingerprint.clone()),
            ),
            (
                "before_history".into(),
                JsonValue::Number(
                    i64::try_from(self.before_history)
                        .map_err(|_| SessionError::Format("before_history overflow".into()))?,
                ),
            ),
            (
                "target_history".into(),
                JsonValue::Number(
                    i64::try_from(self.target_history)
                        .map_err(|_| SessionError::Format("target_history overflow".into()))?,
                ),
            ),
            (
                "after_history".into(),
                JsonValue::Number(
                    i64::try_from(self.after_history)
                        .map_err(|_| SessionError::Format("after_history overflow".into()))?,
                ),
            ),
            (
                "policy_version".into(),
                JsonValue::String(self.policy_version.clone()),
            ),
        ])))
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value.as_object().ok_or_else(|| {
            SessionError::Format("compaction achievement must be an object".into())
        })?;
        let history = |key| {
            usize::try_from(required_u64(object, key)?)
                .map_err(|_| SessionError::Format(format!("{key} overflow")))
        };
        Ok(Self {
            fingerprint: required_string(object, "fingerprint")?,
            before_history: history("before_history")?,
            target_history: history("target_history")?,
            after_history: history("after_history")?,
            policy_version: required_string(object, "policy_version")?,
        })
    }
}

fn compaction_report_from_json(
    object: &BTreeMap<String, JsonValue>,
) -> Result<Option<crate::compact::CompactionReport>, SessionError> {
    object
        .get("last_compaction_report")
        .map(|value| {
            serde_json::from_str(&value.render())
                .map_err(|error| SessionError::Format(error.to_string()))
        })
        .transpose()
}

pub(crate) fn maintenance_usage_from_json(
    object: &BTreeMap<String, JsonValue>,
) -> Result<Vec<MaintenanceUsageReceipt>, SessionError> {
    object
        .get("maintenance_usage")
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| SessionError::Format("maintenance_usage must be an array".into()))?
                .iter()
                .map(MaintenanceUsageReceipt::from_json)
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(crate) fn merge_maintenance_receipts(
    current: &mut Vec<MaintenanceUsageReceipt>,
    receipts: &[MaintenanceUsageReceipt],
) -> Vec<TokenUsage> {
    let mut newly_known = Vec::new();
    for receipt in receipts {
        if let Some(existing) = current.iter_mut().find(|existing| {
            existing.run_id == receipt.run_id && existing.attempt_id == receipt.attempt_id
        }) {
            if existing.usage.is_none() && receipt.usage.is_some() {
                existing.usage = receipt.usage;
                newly_known.extend(receipt.usage);
            }
        } else {
            newly_known.extend(receipt.usage);
            current.push(receipt.clone());
        }
    }
    newly_known
}

/// Persisted conversational state for the runtime and CLI session manager.
///
/// `workspace_root` binds the session to the worktree it was created in. The
/// global session store under `~/.nexus/sudocode` is shared across every
/// `scode serve` instance, so without an explicit workspace root parallel
/// lanes can race and report success while writes land in the wrong CWD. See
/// ROADMAP.md item 41 (Phantom completions root cause) for the full
/// background.
#[derive(Debug, Clone)]
pub struct Session {
    pub version: u32,
    pub session_id: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub(crate) messages: Vec<ConversationMessage>,
    pub compaction: Option<SessionCompaction>,
    pub maintenance_usage: Vec<MaintenanceUsageReceipt>,
    pub compaction_achievement: Option<CompactionAchievement>,
    pub last_compaction_report: Option<crate::compact::CompactionReport>,
    pub fork: Option<SessionFork>,
    pub workspace_root: Option<PathBuf>,
    pub identity: Option<HostedSessionIdentity>,
    pub prompt_history: Vec<SessionPromptEntry>,
    /// The model used in this session, persisted so resumed sessions can
    /// report which model was originally used.
    /// Timestamp of last successful health check (ROADMAP #38)
    pub last_health_check_ms: Option<u64>,
    pub model: Option<String>,
    /// Whether this session was last created by the interactive REPL or by a
    /// one-shot prompt. Older transcripts omit the field and remain
    /// interactive for backwards compatibility.
    pub mode: SessionMode,
    prompt_snapshot: Option<SessionPromptSnapshot>,
    prompt_snapshot_dirty: bool,
    /// Repairs projected during a read-only load, awaiting the active writer.
    /// This bookkeeping is never serialized into the durable transcript.
    pending_tool_repairs: usize,
    persistence: Option<SessionPersistence>,
    restored_history_head: Option<String>,
    view_revision: u64,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
            && self.session_id == other.session_id
            && self.created_at_ms == other.created_at_ms
            && self.updated_at_ms == other.updated_at_ms
            && self.prompt_snapshot == other.prompt_snapshot
            && self.messages == other.messages
            && self.maintenance_usage == other.maintenance_usage
            && self.compaction_achievement == other.compaction_achievement
            && self.last_compaction_report == other.last_compaction_report
            && self.compaction == other.compaction
            && self.fork == other.fork
            && self.identity == other.identity
            && self.workspace_root == other.workspace_root
            && self.prompt_history == other.prompt_history
            && self.last_health_check_ms == other.last_health_check_ms
            && self.mode == other.mode
    }
}

impl Eq for Session {}

/// Errors raised while loading, parsing, or saving sessions.
#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Json(JsonError),
    Format(String),
    SourceChanged,
}

impl Display for SessionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Format(error) => write!(f, "{error}"),
            Self::SourceChanged => f.write_str("session source changed during compaction"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<JsonError> for SessionError {
    fn from(value: JsonError) -> Self {
        Self::Json(value)
    }
}

impl Session {
    /// The selected model context. Mutations use the session's append/update or
    /// context-building methods so its durable graph stays in sync.
    #[must_use]
    pub fn messages(&self) -> &[ConversationMessage] {
        &self.messages
    }

    /// Consume a context snapshot without copying its conversation payloads.
    #[must_use]
    pub fn into_messages(self) -> Vec<ConversationMessage> {
        self.messages
    }
    #[must_use]
    pub fn new() -> Self {
        // P1a (§8.11 R6.2/R6.4)：subprocess 侧唯一可信 zone 来源是 host 注入
        // 的 runner env（moss runner manifest），由 `KernelFsBackend::new` 在
        // 构造文件系统时经 `host_zone_once` 消费；无 env（本地 standalone）→
        // Absent，root 语义保持不变。
        let now = current_time_millis();
        Self {
            version: SESSION_VERSION,
            session_id: generate_session_id(),
            created_at_ms: now,
            updated_at_ms: now,
            messages: Vec::new(),
            compaction: None,
            maintenance_usage: Vec::new(),
            compaction_achievement: None,
            last_compaction_report: None,
            fork: None,
            workspace_root: None,
            identity: None,
            prompt_history: Vec::new(),
            last_health_check_ms: None,
            model: None,
            mode: SessionMode::Interactive,
            prompt_snapshot: None,
            prompt_snapshot_dirty: false,
            pending_tool_repairs: 0,
            persistence: None,
            restored_history_head: None,
            view_revision: 0,
        }
    }

    #[inline]
    #[must_use]
    pub fn prompt_snapshot(&self) -> Option<&SessionPromptSnapshot> {
        self.prompt_snapshot.as_ref()
    }

    /// Stage a new snapshot; persist it with the next message or full save.
    /// A failed/cancelled compaction of a legacy transcript stays untouched.
    pub fn set_prompt_snapshot(&mut self, snapshot: SessionPromptSnapshot) {
        if self.prompt_snapshot.as_ref() != Some(&snapshot) {
            self.prompt_snapshot = Some(snapshot);
            self.prompt_snapshot_dirty = true;
        }
    }

    /// Explicit feature reload discovers a replacement on the next build.
    pub fn clear_prompt_snapshot(&mut self) {
        self.prompt_snapshot = None;
        self.prompt_snapshot_dirty = false;
    }

    #[must_use]
    pub fn with_persistence_path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if self
            .persistence
            .as_ref()
            .is_some_and(|current| current.path == path)
        {
            return self;
        }
        self.persistence = Some(SessionPersistence {
            path,
            fs: Arc::new(StdFsBackend),
            writer: Arc::default(),
            committed_messages: Arc::default(),
            checkpoint: Arc::default(),
        });
        self
    }

    #[must_use]
    pub fn with_mode(mut self, mode: SessionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Attach a custom filesystem backend to the session's persistence
    /// layer. When not called, persistence defaults to [`StdFsBackend`].
    #[must_use]
    pub fn with_fs_backend(mut self, fs: Arc<dyn FsBackend>) -> Self {
        if let Some(persistence) = self.persistence.as_mut() {
            persistence.fs = fs;
        }
        self
    }

    /// Bind this session to the workspace root it was created in.
    ///
    /// This is the per-worktree counterpart to the global session store and
    /// lets downstream tooling reject writes that drift to the wrong CWD when
    /// multiple `opencode serve` instances share `~/.local/share/opencode`.
    #[must_use]
    pub fn with_workspace_root(mut self, workspace_root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(workspace_root.into());
        self
    }

    /// Pre-seed this session with a conversation prefix. Used by the fork
    /// subagent path to hand a child runtime a copy of the parent's
    /// in-flight assistant message + placeholder tool_results before the
    /// first API call, matching CC-fork's `buildForkedMessages`.
    ///
    /// The pre-seeded messages are NOT persisted here — persistence only
    /// engages once [`Session::with_persistence_path`] runs. Callers that
    /// want to persist the seeded state must set persistence first, then
    /// call [`Session::push_message`] for each message so they're written
    /// atomically.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<ConversationMessage>) -> Self {
        self.messages = messages;
        crate::session_history::rebase(&mut self.messages);
        self.view_revision = self.view_revision.wrapping_add(1);

        self.touch();
        self
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    #[must_use]
    pub fn persistence_path(&self) -> Option<&Path> {
        self.persistence.as_ref().map(|value| value.path.as_path())
    }

    /// Adopt this runtime's transcript writer before accepting input. Native
    /// ownership prevents deletion while the runtime is idle; activation also
    /// migrates legacy storage without publishing a stale in-memory snapshot.
    pub fn activate_persistence(&mut self) -> Result<(), SessionError> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let unopened = matches!(
            *persistence
                .writer
                .lock()
                .map_err(|_| SessionError::Format("session writer poisoned".into()))?,
            SessionWriterState::Unopened
        );
        let writer = self.writer_for(&persistence.path)?;
        if unopened {
            if let Some(mut restored) = writer.history_session().map_err(SessionError::Format)? {
                restored.persistence = self.persistence.clone();
                restored.view_revision = self.view_revision;
                if self.prompt_snapshot_dirty {
                    restored.prompt_snapshot.clone_from(&self.prompt_snapshot);
                    restored.prompt_snapshot_dirty = true;
                }
                *self = restored;
            }
        }
        Ok(())
    }

    /// Release this context's writer after its owned work and final save finish.
    /// Cloned snapshots share this state, so they cannot reopen a closed writer.
    pub fn close_persistence(&self) {
        if let Some(persistence) = &self.persistence {
            *persistence
                .writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SessionWriterState::Closed;
        }
    }

    /// Whether this activated context has committed isolation for sibling restores.
    pub fn supports_concurrent_resume(&self) -> Result<bool, SessionError> {
        let Some(persistence) = &self.persistence else {
            return Ok(false);
        };
        let guard = persistence
            .writer
            .lock()
            .map_err(|_| SessionError::Format("session writer poisoned".into()))?;
        match &*guard {
            SessionWriterState::Active(writer) => {
                writer.has_history().map_err(SessionError::Format)
            }
            SessionWriterState::Unopened | SessionWriterState::Closed => Ok(false),
        }
    }

    /// Cloned snapshots and their journal share one writer. Independently
    /// loaded sessions acquire their own runtime contexts.
    pub fn completion_writer(&self) -> Result<Arc<crate::session_writer::SessionWriter>, String> {
        if !self.backend().fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        self.ensure_context_persisted()
            .map_err(|error| error.to_string())?;
        let path = self.persistence_path().ok_or("session has no transcript")?;
        self.writer_for(path).map_err(|error| error.to_string())
    }

    pub(crate) fn adopt_completion_writer(
        &self,
        writer: Arc<crate::session_writer::SessionWriter>,
    ) -> Result<(), String> {
        writer.check()?;
        let persistence = self
            .persistence
            .as_ref()
            .ok_or("session has no transcript")?;
        let mut current = persistence
            .writer
            .lock()
            .map_err(|_| "session writer poisoned")?;
        match &*current {
            SessionWriterState::Closed => return Err("session_closed".into()),
            SessionWriterState::Active(existing) => {
                if !Arc::ptr_eq(existing, &writer) {
                    return Err("session_writer_busy".into());
                }
            }
            SessionWriterState::Unopened => *current = SessionWriterState::Active(writer),
        }
        Ok(())
    }

    fn writer_for(
        &self,
        path: &Path,
    ) -> Result<Arc<crate::session_writer::SessionWriter>, SessionError> {
        if let Some(persistence) = &self.persistence {
            if persistence.path == path {
                let mut guard = persistence
                    .writer
                    .lock()
                    .map_err(|_| SessionError::Format("session writer poisoned".into()))?;
                if matches!(*guard, SessionWriterState::Closed) {
                    return Err(SessionError::Format("session_closed".into()));
                }
                if matches!(*guard, SessionWriterState::Unopened) {
                    let writer = self.acquire_writer(path)?;
                    writer
                        .observe_head(self.restored_history_head.clone().or_else(|| {
                            self.messages
                                .last()
                                .and_then(|message| message.history.as_ref())
                                .map(|identity| identity.id.clone())
                        }))
                        .map_err(SessionError::Format)?;
                    *guard = SessionWriterState::Active(writer);
                }
                let SessionWriterState::Active(writer) = &*guard else {
                    unreachable!("writer acquired")
                };
                writer.check().map_err(SessionError::Format)?;
                return Ok(Arc::clone(writer));
            }
        }
        self.acquire_writer(path)
    }

    fn acquire_writer(
        &self,
        path: &Path,
    ) -> Result<Arc<crate::session_writer::SessionWriter>, SessionError> {
        let path_str = path.to_string_lossy();
        let existing = self.backend().exists(&path_str)?;
        let record_history =
            existing && history::is_record_history(&self.backend().read_snapshot(&path_str)?);
        if record_history
            || ((!existing || self.backend().fenced_locks())
                && crate::experiments::is_enabled(
                    crate::experiments::Experiment::SharedSessionHistory,
                ))
        {
            return crate::session_writer::SessionWriter::acquire_history(
                self.fs_handle(),
                path,
                self,
                None,
                self.view_revision,
            )
            .map_err(SessionError::Format);
        }
        let writer = if self.backend().fenced_locks() {
            crate::session_writer::SessionWriter::acquire(self.fs_handle(), path)
        } else {
            crate::session_writer::SessionWriter::acquire_stream(
                self.fs_handle(),
                path,
                TRANSCRIPT_RETENTION_BYTES,
            )
        };
        writer.map_err(SessionError::Format)
    }

    fn maintenance_writer_for(
        &self,
        path: &Path,
    ) -> Result<(Arc<crate::session_writer::SessionWriter>, bool), SessionError> {
        if let Some(persistence) = &self.persistence {
            let state = persistence
                .writer
                .lock()
                .map_err(|_| SessionError::Format("session writer poisoned".into()))?;
            match &*state {
                SessionWriterState::Closed => {
                    return Err(SessionError::Format("session_closed".into()));
                }
                SessionWriterState::Active(writer) => return Ok((Arc::clone(writer), false)),
                SessionWriterState::Unopened => {}
            }
            let legacy = if self.backend().fenced_locks() {
                match self.backend().read_snapshot(&path.to_string_lossy()) {
                    Ok(bytes) => !bytes.is_empty() && !history::is_record_history(&bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => return Err(error.into()),
                }
            } else {
                false
            };
            if legacy {
                // Do not cache this as a runtime writer: a later message append
                // must still obtain exclusive legacy admission.
                return crate::session_writer::SessionWriter::acquire_maintenance(
                    self.fs_handle(),
                    path,
                )
                .map(|writer| (writer, true))
                .map_err(SessionError::Format);
            }
        }
        self.writer_for(path).map(|writer| (writer, false))
    }

    /// Revision of the current durable transcript through its own backend.
    /// In-memory sessions have no revision. A configured path which is absent
    /// has a distinct sentinel, so external creation also counts as a change.
    pub fn durable_revision(&self) -> Result<Option<String>, SessionError> {
        let Some(path) = self.persistence_path() else {
            return Ok(None);
        };
        let bytes = match self.backend().read_snapshot(&path.to_string_lossy()) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(Some(self.revision_of_contents(bytes.as_deref())?))
    }

    /// Bind a source history to the revision of the same durable bytes.
    /// A source cloned before an external append must not capture the newer
    /// revision and subsequently replace messages it never saw. Metadata may
    /// differ, but replayed message history must match after the same orphan
    /// repair used on load. This is a read-time check, not a backend lock.
    pub fn durable_revision_for_history(&self) -> Result<Option<String>, SessionError> {
        self.durable_history_revision()
            .map(|(revision, _)| revision)
    }

    /// Bind the same history and preserve maintenance receipts observed in
    /// those durable bytes. Return newly known bills for the live tracker;
    /// pending receipts and staged prompt metadata remain in memory.
    pub fn capture_durable_revision_for_history(
        &mut self,
    ) -> Result<(Option<String>, Vec<TokenUsage>), SessionError> {
        self.activate_persistence()?;
        let (revision, receipts) = self.durable_history_revision()?;
        let newly_known = self.merge_maintenance_usage(&receipts);
        Ok((revision, newly_known))
    }

    fn durable_history_revision(
        &self,
    ) -> Result<(Option<String>, Vec<MaintenanceUsageReceipt>), SessionError> {
        let Some(path) = self.persistence_path() else {
            return Ok((None, Vec::new()));
        };
        let bytes = match self.backend().read_snapshot(&path.to_string_lossy()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Some(durable_bytes_revision(None)), Vec::new()));
            }
            Err(error) => return Err(error.into()),
        };
        let contents = crate::session_history::decode(&bytes)?;
        let durable = self.replay_durable(contents)?;
        // Tool input objects are stored canonically, while streamed input can
        // contain different whitespace. Compare the persisted message values;
        // the source revision below binds this context to its durable ancestry.
        if !durable
            .messages
            .iter()
            .map(ConversationMessage::to_json)
            .eq(self.messages.iter().map(ConversationMessage::to_json))
        {
            return Err(SessionError::SourceChanged);
        }
        let revision = durable.context_digest()?;
        Ok((Some(revision), durable.maintenance_usage))
    }

    pub(crate) fn current_history_head(&self) -> Result<Option<String>, SessionError> {
        if let Some(persistence) = &self.persistence {
            if let SessionWriterState::Active(writer) = &*persistence
                .writer
                .lock()
                .map_err(|_| SessionError::Format("session writer poisoned".into()))?
            {
                return writer.history_head().map_err(SessionError::Format);
            }
        }
        Ok(self.restored_history_head.clone().or_else(|| {
            self.messages
                .last()
                .and_then(|message| message.history.as_ref())
                .map(|identity| identity.id.clone())
        }))
    }

    fn replay_durable(&self, contents: &str) -> Result<Self, SessionError> {
        let head = self.current_history_head()?;
        if history::is_record_history(contents.as_bytes()) {
            let graph = history::HistoryGraph::decode(contents.as_bytes())?;
            return match head {
                Some(head) => graph.revision_session(&head),
                None => graph.completed_session(None),
            }
            .map_err(SessionError::Io);
        }
        let shared = Self::shared_history_suffix(contents)
            .map_err(SessionError::Format)?
            .ok_or_else(|| SessionError::Format("shared history missing".into()))?;
        crate::session_history::replay(
            shared,
            // Legacy writers retain exclusive admission, so maintenance must
            // also notice an uncoordinated older client's latest append.
            // Only v3 above has independent committed runtime contexts.
            crate::session_history::Selection::Latest,
        )
    }

    fn revision_of_contents(&self, bytes: Option<&[u8]>) -> Result<String, SessionError> {
        if let Some(bytes) = bytes {
            let contents = crate::session_history::decode(bytes)?;
            return self.replay_durable(contents)?.context_digest();
        }

        Ok(durable_bytes_revision(bytes))
    }

    pub(crate) fn context_digest(&self) -> Result<String, SessionError> {
        let JsonValue::Object(mut context) = self.to_json()? else {
            unreachable!("session is an object")
        };
        // Billing and bookkeeping can change independently of this context.
        for key in [
            "maintenance_usage",
            "updated_at_ms",
            "last_compaction_report",
        ] {
            context.remove(key);
        }
        Ok(durable_bytes_revision(Some(
            JsonValue::Object(context).render().as_bytes(),
        )))
    }

    /// The filesystem this session persists through, as a shareable handle.
    ///
    /// The session is where a host's filesystem already reaches the engine, so
    /// anything the engine needs to read on the session's behalf — the todo list
    /// carried across a compaction, say — asks here rather than growing a second
    /// way in.
    #[must_use]
    pub fn fs_handle(&self) -> Arc<dyn FsBackend> {
        self.persistence
            .as_ref()
            .map(|p| Arc::clone(&p.fs))
            .unwrap_or_else(|| Arc::new(StdFsBackend))
    }

    /// Return the filesystem backend attached to the persistence layer,
    /// falling back to a static `StdFsBackend` when no persistence is set.
    fn backend(&self) -> &dyn FsBackend {
        static DEFAULT: StdFsBackend = StdFsBackend;
        self.persistence
            .as_ref()
            .map(|p| p.fs.as_ref() as &dyn FsBackend)
            .unwrap_or(&DEFAULT)
    }

    /// The offload dir for a given session file + id:
    /// `<sessions-dir>/tool-results/<session-id>/`. The single SSOT for the
    /// offloaded-tool-results location — shared by the writer
    /// ([`Self::tool_results_dir`]) and session-deletion GC so both always
    /// agree on the subtree to create/read/remove.
    #[inline]
    #[must_use]
    pub fn tool_results_dir_for(session_file: &Path, session_id: &str) -> Option<PathBuf> {
        Self::attachment_dir_for(session_file, session_id, "tool-results")
    }

    /// This session's `tool-results/<session-id>/` dir (a grandchild of the
    /// shared sessions dir). `None` for an in-memory session. Namespacing by
    /// session-id is required, not cosmetic: managed session files live flat
    /// in one sessions dir (`<sessions>/<session-id>.jsonl`), so a plain
    /// sibling dir would let one session's tool_use ids collide with another's
    /// and leave nothing to GC per session.
    #[must_use]
    pub fn tool_results_dir(&self) -> Option<PathBuf> {
        Self::tool_results_dir_for(self.persistence_path()?, &self.session_id)
    }

    /// Publish a fork after its inherited full tool outputs are available.
    /// Both sides use their own backend, including forks between stores.
    pub fn save_fork_to_path(&self, parent: &Self, path: &Path) -> Result<(), SessionError> {
        if let Some(source) = parent.tool_results_dir() {
            let source = source.to_string_lossy();
            if parent.backend().exists(&source)? {
                let target =
                    Self::tool_results_dir_for(path, &self.session_id).ok_or_else(|| {
                        SessionError::Format("fork has no attachment directory".into())
                    })?;
                self.backend().create_dir_all(&target.to_string_lossy())?;
                for entry in parent.backend().readdir(&source)? {
                    if entry.is_dir
                        || entry.name.is_empty()
                        || Self::sanitize_offload_id(&entry.name) != entry.name
                    {
                        return Err(SessionError::Format("invalid tool output entry".into()));
                    }
                    let bytes = parent.backend().read(&format!("{source}/{}", entry.name))?;
                    let destination = target.join(&entry.name);
                    self.backend()
                        .write_atomic(&destination.to_string_lossy(), &bytes)?;
                }
            }
        }
        self.save_to_path(path)
    }

    /// Raw response attachments use the same session namespace as tool blobs.
    /// The main transcript never includes these protocol frames as messages.
    #[inline]
    #[must_use]
    pub fn response_captures_dir_for(session_file: &Path, session_id: &str) -> Option<PathBuf> {
        Self::attachment_dir_for(session_file, session_id, "responses")
    }

    /// One namespace rule for current session directories and legacy flat files.
    #[inline]
    fn attachment_dir_for(session_file: &Path, session_id: &str, name: &str) -> Option<PathBuf> {
        let parent = session_file.parent()?;
        if session_file.file_name().and_then(|name| name.to_str())
            == Some(crate::session_control::TRANSCRIPT_FILE)
        {
            Some(parent.join(name))
        } else {
            Some(
                parent
                    .join(name)
                    .join(Self::sanitize_offload_id(session_id)),
            )
        }
    }

    /// Offload a full tool-result blob to `<session-dir>/tool-results/<id>` via
    /// the session's `FsBackend` — backend-agnostic: it lands on the local FS
    /// under `StdFsBackend` and on the nexus VFS under `KernelFsBackend`, no
    /// caller change. `id` is the (unique) tool_use id; it is sanitized to a
    /// safe filename. Written once. Returns the stored path and byte length.
    /// Map an arbitrary tool_use id to a filesystem-safe filename: every
    /// char outside `[A-Za-z0-9_-]` becomes `_`. Applied symmetrically by the
    /// offload write ([`Self::offload_tool_result`]) and the read-more path
    /// (`read_tool_output`) so both resolve the same file — and a hostile id
    /// can neither escape the tool-results dir nor reintroduce a separator.
    #[must_use]
    pub fn sanitize_offload_id(id: &str) -> String {
        id.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    pub fn offload_tool_result(&self, id: &str, full: &[u8]) -> std::io::Result<(String, u64)> {
        let dir = self.tool_results_dir().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "session has no persistence path")
        })?;
        let dir_str = dir.to_string_lossy().into_owned();
        let backend = self.backend();
        let writer = self
            .writer_for(
                self.persistence_path()
                    .ok_or_else(|| std::io::Error::other("session has no persistence"))?,
            )
            .map_err(std::io::Error::other)?;
        let _write = writer.write().map_err(std::io::Error::other)?;
        backend.create_dir_all(&dir_str)?;
        let path_str = dir
            .join(self.tool_output_id(id))
            .to_string_lossy()
            .into_owned();
        backend.write_atomic(&path_str, full)?;
        Ok((path_str, full.len() as u64))
    }

    /// Opaque attachment key scoped to the originating assistant message.
    /// Legacy transcripts retain their existing attachment names.
    #[must_use]
    pub fn tool_output_id(&self, call_id: &str) -> String {
        self.messages
            .iter()
            .rev()
            .find_map(|message| {
                let identity = message.history.as_ref()?;
                message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == call_id))
                    .then(|| {
                        let mut hash = Sha256::new();
                        hash.update(identity.id.as_bytes());
                        hash.update([0]);
                        hash.update(call_id.as_bytes());
                        format!("output-{:x}", hash.finalize())
                    })
            })
            .unwrap_or_else(|| Self::sanitize_offload_id(call_id))
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<(), SessionError> {
        let path = path.as_ref();
        let writer = self.writer_for(path)?;
        let _write = writer.write().map_err(SessionError::Format)?;
        self.save_snapshot(path, &writer)
    }

    // Caller holds the writer operation guard, including bootstrap/compaction.
    fn save_snapshot(
        &self,
        path: &Path,
        writer: &crate::session_writer::SessionWriter,
    ) -> Result<(), SessionError> {
        if writer
            .save_history(self, self.view_revision)
            .map_err(SessionError::Format)?
        {
            return Ok(());
        }
        let frame = self.history_checkpoint(path)?;
        if self.checkpoint_unchanged(path, &frame)? {
            return Ok(());
        }
        let backend = self.backend();
        let path_str = path.to_string_lossy();
        backend.create_append_log(&path_str, TRANSCRIPT_RETENTION_BYTES)?;
        backend.append(&path_str, format!("{}\n", frame.render()).as_bytes())?;
        self.remember_committed_messages(path)?;
        self.publish_checkpoint_cursor(path, &frame)?;
        Ok(())
    }

    /// Archive source history before atomically committing a compacted view.
    /// Both writes use the session backend, including hosted VFS sessions.
    pub fn save_compacted_to_path(
        &self,
        original: &Session,
        path: impl AsRef<Path>,
    ) -> Result<(), SessionError> {
        self.save_compacted_to_path_checked(original, path, None)
    }

    /// Check the source revision before making either the archive or the
    /// replacement. This detects changes observed at commit preparation; it
    /// serializes native writes, but cannot fence remote backend writers.
    pub fn save_compacted_to_path_checked(
        &self,
        original: &Session,
        path: impl AsRef<Path>,
        expected_revision: Option<&str>,
    ) -> Result<(), SessionError> {
        self.save_replacement_to_path_checked(original, path.as_ref(), expected_revision, "compact")
    }

    /// Preserve rejected signed history before installing a thinking-only repair.
    pub fn save_thinking_repaired_to_path_checked(
        &self,
        original: &Session,
        path: impl AsRef<Path>,
        expected_revision: Option<&str>,
    ) -> Result<(), SessionError> {
        self.save_replacement_to_path_checked(
            original,
            path.as_ref(),
            expected_revision,
            "thinking-repair",
        )
    }

    fn save_replacement_to_path_checked(
        &self,
        original: &Session,
        path: &Path,
        expected_revision: Option<&str>,
        purpose: &str,
    ) -> Result<(), SessionError> {
        let path = path.as_ref();
        let writer = self.writer_for(path)?;
        let _write = writer.write().map_err(SessionError::Format)?;
        let durable = match self.backend().read_snapshot(&path.to_string_lossy()) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(expected) = expected_revision {
            if expected != original.revision_of_contents(durable.as_deref())? {
                return Err(SessionError::SourceChanged);
            }
        }
        let archive = format!(
            "{}.before-{purpose}-{}-{}",
            path.display(),
            current_time_millis(),
            uuid::Uuid::new_v4()
        );
        // Preserve the durable source byte-for-byte. In-memory metadata can
        // already contain a staged prompt snapshot for a legacy transcript.
        let snapshot = match durable {
            Some(bytes) => String::from_utf8(bytes).map_err(|_| {
                SessionError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "session transcript is not valid UTF-8",
                ))
            })?,
            None => original.render_jsonl_snapshot()?,
        };
        write_atomic_with(self.backend(), &archive, &snapshot)?;
        self.save_snapshot(path, &writer)
    }

    /// Rewrite the persisted transcript from the in-memory state.
    ///
    /// `push_message` appends incrementally, so any operation that *replaces*
    /// `messages` (compaction, discarding an unanswered prompt) must call this
    /// or the next load resurrects the pre-replacement history from disk.
    /// A no-op for sessions without a persistence path.
    pub fn rewrite_persisted(&self) -> Result<(), SessionError> {
        match self.persistence_path() {
            Some(path) => self.save_to_path(path.to_path_buf()),
            None => Ok(()),
        }
    }

    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        Self::load_from_path_with(&StdFsBackend, path)
    }

    /// Load a session from disk using a custom filesystem backend.
    pub fn load_from_path_with(
        fs: &dyn FsBackend,
        path: impl AsRef<Path>,
    ) -> Result<Self, SessionError> {
        let path = path.as_ref();
        let bytes = fs.read_snapshot(&path.to_string_lossy())?;
        let contents = crate::session_history::decode(&bytes)?;
        let session =
            Self::from_persisted_contents(contents)?.with_persistence_path(path.to_path_buf());
        Ok(session)
    }

    /// Refresh this runtime's durable view without opening another writer.
    /// New history/checkpoint caches belong to the fresh view; ownership stays
    /// shared with this context's snapshots and completion journal. A record
    /// writer restores its own continuation, not the latest sibling's branch.
    #[inline]
    pub(crate) fn reload_persisted_source(&self) -> Result<Self, SessionError> {
        let persistence = self
            .persistence
            .as_ref()
            .ok_or_else(|| SessionError::Format("session has no transcript".into()))?;
        let writer = match &*persistence
            .writer
            .lock()
            .map_err(|_| SessionError::Format("session writer poisoned".into()))?
        {
            SessionWriterState::Closed => {
                return Err(SessionError::Format("session_closed".into()));
            }
            SessionWriterState::Active(writer) => Some(Arc::clone(writer)),
            SessionWriterState::Unopened => None,
        };
        let _write = writer
            .as_ref()
            .map(|writer| writer.write().map_err(SessionError::Format))
            .transpose()?;
        let history = writer
            .as_ref()
            .map(|writer| {
                writer
                    .refresh_history_session()
                    .map_err(SessionError::Format)
            })
            .transpose()?
            .flatten();
        let mut restored = match history {
            Some(session) => session.with_persistence_path(&persistence.path),
            None => Self::load_from_path_with(persistence.fs.as_ref(), &persistence.path)?,
        }
        .with_fs_backend(Arc::clone(&persistence.fs));
        if restored.session_id != self.session_id
            || restored.workspace_root != self.workspace_root
            || restored.identity != self.identity
        {
            return Err(SessionError::SourceChanged);
        }
        let restored_persistence = restored
            .persistence
            .as_mut()
            .expect("restored path attached");
        restored_persistence.writer = Arc::clone(&persistence.writer);
        Ok(restored)
    }

    /// Replay either persisted format with the repairs used by every load.
    pub(crate) fn from_persisted_contents(contents: &str) -> Result<Self, SessionError> {
        let is_record_history = history::is_record_history(contents.as_bytes());
        let mut session = if is_record_history {
            history::HistoryGraph::decode(contents.as_bytes())?.completed_session(None)?
        } else {
            match JsonValue::parse(contents) {
                Ok(value) if Self::is_snapshot(&value) => Self::from_json(&value)?,
                Err(_) | Ok(_) => Self::from_jsonl(contents)?,
            }
        };
        // Recover interrupted calls without changing existing result records:
        // local consumers such as /undo still need their structured payloads.
        session.pending_tool_repairs = session.sanitize_orphan_tool_uses();
        if session.pending_tool_repairs > 0 && !is_record_history {
            crate::session_history::rebase(&mut session.messages);
            session.view_revision = session.view_revision.wrapping_add(1);
        } else {
            crate::session_history::identify_legacy(&session.session_id, &mut session.messages);
        }

        Ok(session)
    }

    fn is_snapshot(value: &JsonValue) -> bool {
        value
            .as_object()
            .is_some_and(|object| object.contains_key("messages") && !object.contains_key("type"))
    }

    /// Preserve the legacy projection as an initial, neutral checkpoint.
    /// Concurrent stream migrations retain any newer continuation already
    /// appended while another runtime was reading these source bytes.
    pub(crate) fn upgrade_legacy_transcript(contents: &str) -> Result<String, SessionError> {
        let mut session = match JsonValue::parse(contents) {
            Ok(value) if Self::is_snapshot(&value) => Self::from_json(&value)?,
            _ => Self::from_legacy_jsonl(contents)?,
        };
        crate::session_history::identify_legacy(&session.session_id, &mut session.messages);
        let JsonValue::Object(mut checkpoint) = session.history_checkpoint(Path::new(""))? else {
            unreachable!("history checkpoint is an object")
        };
        checkpoint.insert("replaces_context".into(), JsonValue::Bool(false));
        Ok(format!("{}\n", JsonValue::Object(checkpoint).render()))
    }

    fn ensure_context_persisted(&self) -> Result<(), SessionError> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let initialized = persistence
            .checkpoint
            .lock()
            .map_err(|_| SessionError::Format("checkpoint receipt poisoned".into()))?
            .is_some();
        if initialized {
            return Ok(());
        }
        let writer = self.writer_for(&persistence.path)?;
        if writer.has_history().map_err(SessionError::Format)? {
            return Ok(());
        }
        let _write = writer.write().map_err(SessionError::Format)?;
        let initialized = persistence
            .checkpoint
            .lock()
            .map_err(|_| SessionError::Format("checkpoint receipt poisoned".into()))?
            .is_some();
        if !initialized {
            self.save_snapshot(&persistence.path, &writer)?;
        }
        Ok(())
    }

    /// Repair unfinished tool batches before a new turn uses this history.
    /// Persist the repair without rewriting existing tool result records.
    pub fn repair_orphan_tool_uses(&mut self) -> Result<usize, SessionError> {
        let repairs = ToolHistoryRepairs::plan(&self.messages);
        if repairs.missing.is_empty() && self.pending_tool_repairs == 0 {
            self.ensure_context_persisted()?;
            return Ok(0);
        }
        let has_missing = !repairs.missing.is_empty();
        let original = has_missing.then(|| self.messages.clone());
        let repaired = self.pending_tool_repairs + repairs.apply_missing(&mut self.messages);
        if has_missing {
            crate::session_history::rebase(&mut self.messages);
            self.view_revision = self.view_revision.wrapping_add(1);
        }
        if let Err(error) = self.rewrite_persisted() {
            if let Some(original) = original {
                self.messages = original;
            }
            return Err(error);
        }
        self.pending_tool_repairs = 0;
        Ok(repaired)
    }

    fn sanitize_orphan_tool_uses(&mut self) -> usize {
        ToolHistoryRepairs::plan(&self.messages).apply_missing(&mut self.messages)
    }

    pub fn push_message(&mut self, mut message: ConversationMessage) -> Result<(), SessionError> {
        self.activate_persistence()?;
        if self
            .messages
            .last()
            .is_some_and(|message| message.history.is_none())
        {
            crate::session_history::identify_legacy(&self.session_id, &mut self.messages);
        }
        self.ensure_context_persisted()?;
        crate::session_history::identify_append(&mut message, &self.messages)
            .map_err(SessionError::Format)?;
        if let Some(path) = self.persistence_path() {
            let writer = self.writer_for(path)?;
            if let Some(identity) = &mut message.history {
                Arc::make_mut(identity).parent_id =
                    writer.message_head().map_err(SessionError::Format)?;
            }
        }

        self.touch();
        self.messages.push(message);
        let persist_result = {
            let message_ref = self.messages.last().ok_or_else(|| {
                SessionError::Format("message was just pushed but missing".to_string())
            })?;
            self.append_persisted_message(message_ref)
        };
        if let Err(error) = persist_result {
            self.messages.pop();
            return Err(error);
        }
        self.prompt_snapshot_dirty = false;
        Ok(())
    }

    /// Extend an in-flight assistant message without rewriting session history.
    /// The append record is durable before a streamed tool can perform effects.
    /// Completed-turn snapshots fold these updates back into normal messages.
    pub(crate) fn update_assistant_message(
        &mut self,
        index: usize,
        mut message: ConversationMessage,
    ) -> Result<(), SessionError> {
        let previous = self
            .messages
            .get(index)
            .ok_or_else(|| SessionError::Format("assistant update index out of range".into()))?;
        if previous.role != MessageRole::Assistant || message.role != MessageRole::Assistant {
            return Err(SessionError::Format(
                "stream update requires an assistant message".into(),
            ));
        }
        message.history.clone_from(&previous.history);
        if previous == &message {
            return Ok(());
        }
        let record_index = i64::try_from(index)
            .map_err(|_| SessionError::Format("assistant index overflow".into()))?;
        let previous = std::mem::replace(&mut self.messages[index], message);
        if let Some(path) = self.persistence_path() {
            let writer = self.writer_for(path)?;
            let _write = writer.write().map_err(SessionError::Format)?;
            match writer.update_history(&self.messages[index]) {
                Ok(true) => {
                    self.touch();
                    return Ok(());
                }
                Ok(false) => {}
                Err(error) => {
                    self.messages[index] = previous;
                    return Err(SessionError::Format(error));
                }
            }
        }
        let mut record = BTreeMap::new();
        record.insert("type".into(), JsonValue::String("assistant_update".into()));
        if let Some(identity) = &self.messages[index].history {
            record.insert("message_id".into(), JsonValue::String(identity.id.clone()));
        } else {
            record.insert("index".into(), JsonValue::Number(record_index));
        }
        record.insert("message".into(), self.messages[index].to_json());
        if let Err(error) = self.append_persisted_record(&JsonValue::Object(record)) {
            self.messages[index] = previous;
            return Err(error);
        }
        self.touch();
        Ok(())
    }

    pub fn push_user_text(&mut self, text: impl Into<String>) -> Result<(), SessionError> {
        self.push_message(ConversationMessage::user_text(text))
    }

    pub(crate) fn record_turn_duration(&mut self, duration_ms: u64) -> Result<(), SessionError> {
        let Some(index) = self
            .messages
            .iter()
            .rposition(|message| message.role == MessageRole::Assistant)
        else {
            return Ok(());
        };
        let mut message = self.messages[index].clone();
        message.duration_ms = Some(duration_ms);
        self.update_assistant_message(index, message)
    }

    /// Push a user message containing multiple content blocks (e.g. text + images).
    pub fn push_user_blocks(&mut self, blocks: Vec<ContentBlock>) -> Result<(), SessionError> {
        self.push_message(ConversationMessage {
            history: None,
            role: MessageRole::User,
            blocks,
            usage: None,
            model: None,
            duration_ms: None,
        })
    }

    pub fn record_compaction(&mut self, summary: impl Into<String>, removed_message_count: usize) {
        self.record_compaction_with_usage(summary, removed_message_count, None, BTreeSet::new());
    }

    pub fn record_compaction_with_usage(
        &mut self,
        summary: impl Into<String>,
        removed_message_count: usize,
        usage: Option<TokenUsage>,
        pre_compact_discovered_tools: BTreeSet<String>,
    ) {
        self.touch();
        let count = self.compaction.as_ref().map_or(1, |value| value.count + 1);
        self.compaction = Some(SessionCompaction {
            count,
            removed_message_count,
            summary: summary.into(),
            usage,
            pre_compact_discovered_tools,
        });
        self.record_context_replacement();
    }

    /// Select a replacement context while preserving retained message identities.
    /// Its next checkpoint records the complete selection as message references.
    pub(crate) fn record_context_replacement(&mut self) {
        let mut parent = None;
        for message in &mut self.messages {
            let identity = message.history.get_or_insert_with(|| {
                Arc::new(crate::session_history::MessageIdentity::new(parent.clone()))
            });
            parent = Some(identity.id.clone());
        }
        crate::session_history::identify_legacy(&self.session_id, &mut self.messages);
        self.view_revision = self.view_revision.wrapping_add(1);
    }

    #[must_use]
    pub fn fork(&self, branch_name: Option<String>) -> Self {
        let now = current_time_millis();
        let mut fork = Self {
            version: self.version,
            session_id: generate_session_id(),
            created_at_ms: now,
            updated_at_ms: now,
            messages: self.messages.clone(),
            compaction: self.compaction.clone(),
            maintenance_usage: self.maintenance_usage.clone(),
            compaction_achievement: self.compaction_achievement.clone(),
            last_compaction_report: self.last_compaction_report.clone(),
            fork: Some(SessionFork {
                parent_session_id: self.session_id.clone(),
                branch_name: normalize_optional_string(branch_name),
            }),
            workspace_root: self.workspace_root.clone(),
            identity: self.identity.clone(),
            prompt_history: self.prompt_history.clone(),
            last_health_check_ms: self.last_health_check_ms,
            model: self.model.clone(),
            mode: self.mode,
            prompt_snapshot: self.prompt_snapshot.clone(),
            prompt_snapshot_dirty: false,
            pending_tool_repairs: 0,
            persistence: None,
            restored_history_head: None,
            view_revision: 0,
        };
        crate::session_history::rebase(&mut fork.messages);

        fork
    }

    fn context_metadata(&self) -> Result<JsonValue, SessionError> {
        let JsonValue::Object(mut object) = self.meta_record()? else {
            unreachable!("metadata is an object")
        };
        object.remove("type");
        if let Some(compaction) = &self.compaction {
            object.insert("compaction".into(), compaction.to_json()?);
        }
        if !self.prompt_history.is_empty() {
            object.insert(
                "prompt_history".into(),
                JsonValue::Array(
                    self.prompt_history
                        .iter()
                        .map(SessionPromptEntry::to_jsonl_record)
                        .collect(),
                ),
            );
        }
        Ok(JsonValue::Object(object))
    }

    pub fn to_json(&self) -> Result<JsonValue, SessionError> {
        let JsonValue::Object(mut object) = self.context_metadata()? else {
            unreachable!("metadata is an object")
        };
        object.insert(
            "messages".into(),
            JsonValue::Array(
                self.messages
                    .iter()
                    .map(ConversationMessage::to_json)
                    .collect(),
            ),
        );
        Ok(JsonValue::Object(object))
    }

    pub(super) fn with_history_head(mut self, head: Option<String>) -> Self {
        self.restored_history_head = head;
        self
    }

    pub fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("session must be an object".to_string()))?;
        let version = object
            .get("version")
            .and_then(JsonValue::as_i64)
            .ok_or_else(|| SessionError::Format("missing version".to_string()))?;
        let version = u32::try_from(version)
            .map_err(|_| SessionError::Format("version out of range".to_string()))?;
        let messages = object
            .get("messages")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| SessionError::Format("missing messages".to_string()))?
            .iter()
            .map(ConversationMessage::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let now = current_time_millis();
        let session_id = object
            .get("session_id")
            .and_then(JsonValue::as_str)
            .map_or_else(generate_session_id, ToOwned::to_owned);
        let created_at_ms = object
            .get("created_at_ms")
            .map(|value| required_u64_from_value(value, "created_at_ms"))
            .transpose()?
            .unwrap_or(now);
        let updated_at_ms = object
            .get("updated_at_ms")
            .map(|value| required_u64_from_value(value, "updated_at_ms"))
            .transpose()?
            .unwrap_or(created_at_ms);
        let compaction = object
            .get("compaction")
            .map(SessionCompaction::from_json)
            .transpose()?;
        let maintenance_usage = maintenance_usage_from_json(object)?;
        let compaction_achievement = object
            .get("compaction_achievement")
            .map(CompactionAchievement::from_json)
            .transpose()?;
        let last_compaction_report = compaction_report_from_json(object)?;
        let fork = object.get("fork").map(SessionFork::from_json).transpose()?;
        let identity = object
            .get("identity")
            .map(HostedSessionIdentity::from_json)
            .transpose()?;
        let workspace_root = object
            .get("workspace_root")
            .and_then(JsonValue::as_str)
            .map(PathBuf::from);
        let prompt_history = object
            .get("prompt_history")
            .and_then(JsonValue::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(SessionPromptEntry::from_json_opt)
                    .collect()
            })
            .unwrap_or_default();
        let prompt_snapshot = object
            .get("prompt_snapshot")
            .map(SessionPromptSnapshot::from_json)
            .transpose()?;
        let model = object
            .get("model")
            .and_then(JsonValue::as_str)
            .map(String::from);
        let mode = object
            .get("mode")
            .and_then(JsonValue::as_str)
            .and_then(SessionMode::from_str)
            .unwrap_or_default();
        Ok(Self {
            version,
            session_id,
            created_at_ms,
            updated_at_ms,
            messages,
            compaction,
            maintenance_usage,
            compaction_achievement,
            last_compaction_report,
            fork,
            workspace_root,
            identity,
            prompt_history,
            last_health_check_ms: None,
            model,
            mode,
            prompt_snapshot,
            prompt_snapshot_dirty: false,
            pending_tool_repairs: 0,
            persistence: None,
            restored_history_head: None,
            view_revision: 0,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "keep ordered JSONL replay and its accumulated state together"
    )]
    fn from_jsonl(contents: &str) -> Result<Self, SessionError> {
        if let Some(shared) = Self::shared_history_suffix(contents).map_err(SessionError::Format)? {
            crate::session_history::replay(shared, crate::session_history::Selection::Latest)
        } else {
            Self::from_legacy_jsonl(contents)
        }
    }

    pub(crate) fn shared_history_suffix(contents: &str) -> Result<Option<&str>, String> {
        let mut offset = 0;
        for line in contents.split_inclusive('\n') {
            if line.contains("history_checkpoint") {
                let value = match JsonValue::parse(line) {
                    Ok(value) => value,
                    Err(_) if !line.ends_with('\n') => return Ok(None),
                    Err(error) => return Err(error.to_string()),
                };
                if value
                    .as_object()
                    .and_then(|object| object.get("type"))
                    .and_then(JsonValue::as_str)
                    == Some("history_checkpoint")
                {
                    return Ok(Some(&contents[offset..]));
                }
            }
            offset += line.len();
        }
        Ok(None)
    }

    pub(crate) fn load_history_at(
        fs: &dyn FsBackend,
        path: &Path,
        head: Option<&str>,
    ) -> Result<Self, SessionError> {
        let bytes = fs.read_snapshot(&path.to_string_lossy())?;
        if history::is_record_history(&bytes) {
            let revision = head.ok_or_else(|| {
                SessionError::Format("shared history context revision missing".into())
            })?;
            return history::HistoryGraph::decode(&bytes)?
                .revision_session(revision)
                .map_err(SessionError::Io);
        }
        let contents = crate::session_history::decode(&bytes)?;
        let shared = Self::shared_history_suffix(contents)
            .map_err(SessionError::Format)?
            .ok_or_else(|| SessionError::Format("shared history missing".into()))?;
        crate::session_history::replay(shared, crate::session_history::Selection::Head(head))
    }

    #[allow(clippy::too_many_lines, reason = "legacy JSONL compatibility replay")]
    fn from_legacy_jsonl(contents: &str) -> Result<Self, SessionError> {
        let mut version = SESSION_VERSION;
        let mut session_id = None;
        let mut created_at_ms = None;
        let mut updated_at_ms = None;
        let mut messages = Vec::new();
        let mut compaction = None;
        let mut maintenance_usage = Vec::new();
        let mut compaction_achievement = None;
        let mut last_compaction_report = None;
        let mut fork = None;
        let mut workspace_root = None;
        let mut identity = None;
        let mut model = None;
        let mut mode = SessionMode::Interactive;
        let mut prompt_snapshot = None;
        let mut prompt_history = Vec::new();

        for record in crate::session_history::records(contents) {
            let (line_number, value) = record?;
            let object = value.as_object().ok_or_else(|| {
                SessionError::Format(format!(
                    "JSONL record at line {} must be an object",
                    line_number
                ))
            })?;
            match object
                .get("type")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| {
                    SessionError::Format(format!(
                        "JSONL record at line {} missing type",
                        line_number
                    ))
                })? {
                "session_meta" => {
                    // Metadata only starts full snapshots, never incremental
                    // updates. Native streams retain older snapshots in their
                    // log, so the most recent one replaces the accumulated view.
                    messages.clear();
                    prompt_history.clear();
                    compaction = None;
                    maintenance_usage = maintenance_usage_from_json(object)?;
                    compaction_achievement = object
                        .get("compaction_achievement")
                        .map(CompactionAchievement::from_json)
                        .transpose()?;
                    last_compaction_report = compaction_report_from_json(object)?;
                    version = required_u32(object, "version")?;
                    session_id = Some(required_string(object, "session_id")?);
                    created_at_ms = Some(required_u64(object, "created_at_ms")?);
                    updated_at_ms = Some(required_u64(object, "updated_at_ms")?);
                    fork = object.get("fork").map(SessionFork::from_json).transpose()?;
                    identity = object
                        .get("identity")
                        .map(HostedSessionIdentity::from_json)
                        .transpose()?;
                    workspace_root = object
                        .get("workspace_root")
                        .and_then(JsonValue::as_str)
                        .map(PathBuf::from);
                    prompt_snapshot = object
                        .get("prompt_snapshot")
                        .map(SessionPromptSnapshot::from_json)
                        .transpose()?;
                    model = object
                        .get("model")
                        .and_then(JsonValue::as_str)
                        .map(String::from);
                    mode = object
                        .get("mode")
                        .and_then(JsonValue::as_str)
                        .and_then(SessionMode::from_str)
                        .unwrap_or_default();
                }
                "message" => {
                    if let Some(snapshot) = object.get("prompt_snapshot") {
                        prompt_snapshot = Some(SessionPromptSnapshot::from_json(snapshot)?);
                    }
                    let message_value = object.get("message").ok_or_else(|| {
                        SessionError::Format(format!(
                            "JSONL record at line {} missing message",
                            line_number
                        ))
                    })?;
                    messages.push(ConversationMessage::from_json(message_value)?);
                }
                "assistant_update" => {
                    let index = usize::try_from(required_u64(object, "index")?).map_err(|_| {
                        SessionError::Format("assistant update index overflow".into())
                    })?;
                    let message =
                        ConversationMessage::from_json(object.get("message").ok_or_else(
                            || SessionError::Format("assistant update missing message".into()),
                        )?)?;
                    let slot = messages.get_mut(index).ok_or_else(|| {
                        SessionError::Format("assistant update index out of range".into())
                    })?;
                    if slot.role != MessageRole::Assistant || message.role != MessageRole::Assistant
                    {
                        return Err(SessionError::Format(
                            "assistant update target is not assistant".into(),
                        ));
                    }
                    *slot = message;
                }
                "compaction" => {
                    compaction = Some(SessionCompaction::from_json(&JsonValue::Object(
                        object.clone(),
                    ))?);
                }
                "prompt_history" => {
                    if let Some(entry) =
                        SessionPromptEntry::from_json_opt(&JsonValue::Object(object.clone()))
                    {
                        prompt_history.push(entry);
                    }
                }
                other => {
                    return Err(SessionError::Format(format!(
                        "unsupported JSONL record type at line {}: {other}",
                        line_number
                    )))
                }
            }
        }

        if session_id.is_none() && !contents.trim().is_empty() {
            return Err(SessionError::Format("session metadata missing".into()));
        }
        let now = current_time_millis();
        Ok(Self {
            version,
            session_id: session_id.unwrap_or_else(generate_session_id),
            created_at_ms: created_at_ms.unwrap_or(now),
            updated_at_ms: updated_at_ms.unwrap_or(created_at_ms.unwrap_or(now)),
            messages,
            compaction,
            maintenance_usage,
            compaction_achievement,
            last_compaction_report,
            fork,
            workspace_root,
            identity,
            prompt_history,
            last_health_check_ms: None,
            model,
            mode,
            prompt_snapshot,
            prompt_snapshot_dirty: false,
            pending_tool_repairs: 0,
            persistence: None,
            restored_history_head: None,
            view_revision: 0,
        })
    }

    /// Record a user prompt with the current wall-clock timestamp.
    ///
    /// The entry is appended to the in-memory history and, when a persistence
    /// path is configured, incrementally written to the JSONL session file.
    pub fn push_prompt_entry(&mut self, text: impl Into<String>) -> Result<(), SessionError> {
        let timestamp_ms = current_time_millis();
        let entry = SessionPromptEntry {
            timestamp_ms,
            text: text.into(),
        };
        self.prompt_history.push(entry);
        self.rewrite_persisted()
    }

    /// Build a self-contained history checkpoint for snapshots and exports.
    fn snapshot_lines(&self) -> Result<Vec<String>, SessionError> {
        Ok(vec![self.history_checkpoint(Path::new(""))?.render()])
    }

    fn history_checkpoint(&self, path: &Path) -> Result<JsonValue, SessionError> {
        let owned = self
            .messages
            .iter()
            .any(|message| message.history.is_none())
            .then(|| {
                let mut messages = self.messages.clone();
                crate::session_history::identify_legacy(&self.session_id, &mut messages);
                messages
            });
        let messages = owned.as_deref().unwrap_or(&self.messages);
        let persistence = self
            .persistence
            .as_ref()
            .filter(|persistence| persistence.path == path);
        let committed = persistence
            .map(|persistence| {
                persistence
                    .committed_messages
                    .lock()
                    .map_err(|_| SessionError::Format("history commit cache poisoned".into()))
            })
            .transpose()?;
        let pending = messages
            .iter()
            .filter(|message| {
                !committed.as_ref().is_some_and(|ids| {
                    message
                        .history
                        .as_ref()
                        .is_some_and(|identity| ids.contains(&identity.id))
                })
            })
            .map(ConversationMessage::to_json)
            .collect();
        drop(committed);
        let mut parent = persistence.and_then(|_| self.restored_history_head.clone());
        let mut needs_view = true;
        if let Some(persistence) = persistence {
            if let SessionWriterState::Active(writer) = &*persistence
                .writer
                .lock()
                .map_err(|_| SessionError::Format("session writer poisoned".into()))?
            {
                parent = writer.history_head().map_err(SessionError::Format)?;
            }
            needs_view = persistence
                .checkpoint
                .lock()
                .map_err(|_| SessionError::Format("checkpoint receipt poisoned".into()))?
                .as_ref()
                .is_none_or(|receipt| receipt.view_revision != self.view_revision);
        }
        let mut record = BTreeMap::from([
            (
                "replaces_context".into(),
                JsonValue::Bool(needs_view && (self.view_revision > 0 || persistence.is_none())),
            ),
            (
                "history_version".into(),
                JsonValue::Number(crate::session_history::HISTORY_VERSION),
            ),
            (
                "type".into(),
                JsonValue::String("history_checkpoint".into()),
            ),
            (
                "id".into(),
                JsonValue::String(uuid::Uuid::new_v4().to_string()),
            ),
            (
                "parent_id".into(),
                parent.map_or(JsonValue::Null, JsonValue::String),
            ),
            ("meta".into(), self.context_metadata()?),
            ("messages".into(), JsonValue::Array(pending)),
        ]);
        if needs_view {
            record.insert(
                "view".into(),
                JsonValue::Array(
                    messages
                        .iter()
                        .filter_map(|message| {
                            message
                                .history
                                .as_ref()
                                .map(|identity| JsonValue::String(identity.id.clone()))
                        })
                        .collect(),
                ),
            );
        }
        Ok(JsonValue::Object(record))
    }

    fn checkpoint_unchanged(&self, path: &Path, frame: &JsonValue) -> Result<bool, SessionError> {
        let Some(persistence) = self
            .persistence
            .as_ref()
            .filter(|persistence| persistence.path == path)
        else {
            return Ok(false);
        };
        let Some(frame) = frame.as_object() else {
            return Ok(false);
        };
        let Some(receipt) = persistence
            .checkpoint
            .lock()
            .map_err(|_| SessionError::Format("checkpoint receipt poisoned".into()))?
            .clone()
        else {
            return Ok(false);
        };
        let metadata_hash: [u8; 32] = Sha256::digest(
            frame
                .get("meta")
                .ok_or_else(|| SessionError::Format("checkpoint metadata missing".into()))?
                .render()
                .as_bytes(),
        )
        .into();
        Ok(frame.get("view").is_none()
            && frame
                .get("messages")
                .and_then(JsonValue::as_array)
                .is_some_and(|messages| messages.is_empty())
            && frame.get("parent_id").and_then(JsonValue::as_str) == Some(receipt.head.as_str())
            && metadata_hash == receipt.metadata_hash)
    }

    fn remember_committed_messages(&self, path: &Path) -> Result<(), SessionError> {
        if let Some(persistence) = self
            .persistence
            .as_ref()
            .filter(|persistence| persistence.path == path)
        {
            let mut committed = persistence
                .committed_messages
                .lock()
                .map_err(|_| SessionError::Format("history commit cache poisoned".into()))?;
            committed.extend(self.messages.iter().filter_map(|message| {
                message.history.as_ref().map(|identity| identity.id.clone())
            }));
            drop(committed);
        }
        Ok(())
    }

    fn publish_checkpoint_cursor(
        &self,
        path: &Path,
        frame: &JsonValue,
    ) -> Result<(), SessionError> {
        let Some(persistence) = self
            .persistence
            .as_ref()
            .filter(|persistence| persistence.path == path)
        else {
            return Ok(());
        };
        let frame = frame
            .as_object()
            .ok_or_else(|| SessionError::Format("checkpoint must be an object".into()))?;
        let head = frame
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| SessionError::Format("checkpoint identity missing".into()))?;
        if let SessionWriterState::Active(writer) = &*persistence
            .writer
            .lock()
            .map_err(|_| SessionError::Format("session writer poisoned".into()))?
        {
            writer
                .observe_head(Some(head.to_owned()))
                .map_err(SessionError::Format)?;
        }
        let metadata_hash: [u8; 32] = Sha256::digest(
            frame
                .get("meta")
                .ok_or_else(|| SessionError::Format("checkpoint metadata missing".into()))?
                .render()
                .as_bytes(),
        )
        .into();
        *persistence
            .checkpoint
            .lock()
            .map_err(|_| SessionError::Format("checkpoint receipt poisoned".into()))? =
            Some(CheckpointReceipt {
                head: head.to_owned(),
                view_revision: self.view_revision,
                metadata_hash,
            });
        Ok(())
    }

    fn render_jsonl_snapshot(&self) -> Result<String, SessionError> {
        let mut rendered = self.snapshot_lines()?.join("\n");
        rendered.push('\n');
        Ok(rendered)
    }

    /// Persist a new session before its first turn using the backend's native
    /// append-log format. Existing non-empty transcripts must be loaded instead.
    pub fn initialize_persistence(&self) -> Result<(), SessionError> {
        let Some(path) = self.persistence_path() else {
            return Ok(());
        };
        let path_str = path.to_string_lossy();
        if self.backend().exists(&path_str)? && self.backend().stat(&path_str)?.len > 0 {
            return Err(SessionError::Format(
                "session transcript already exists".into(),
            ));
        }
        let writer = self.writer_for(path)?;
        let _write = writer.write().map_err(SessionError::Format)?;
        self.save_snapshot(path, &writer)
    }

    /// Seal a terminal turn only after its actual tool outcomes are durable.
    pub(crate) fn finish_turn(&self, turn_id: &str) -> Result<(), SessionError> {
        let Some(path) = self.persistence_path() else {
            return Ok(());
        };
        let writer = self.writer_for(path)?;
        let _write = writer.write().map_err(SessionError::Format)?;
        self.save_snapshot(path, &writer)?;
        writer.finish_history(turn_id).map_err(SessionError::Format)
    }

    fn append_persisted_message(&self, message: &ConversationMessage) -> Result<(), SessionError> {
        if let Some(path) = self.persistence_path() {
            let writer = self.writer_for(path)?;
            let _write = writer.write().map_err(SessionError::Format)?;
            if writer
                .append_history(message, self)
                .map_err(SessionError::Format)?
            {
                return Ok(());
            }
        }
        let mut record = message_record(message);
        if self.prompt_snapshot_dirty {
            if let (JsonValue::Object(object), Some(snapshot)) =
                (&mut record, &self.prompt_snapshot)
            {
                object.insert("prompt_snapshot".into(), snapshot.to_json());
            }
        }
        self.append_persisted_record(&record)?;
        if let (Some(persistence), Some(identity)) = (&self.persistence, &message.history) {
            persistence
                .committed_messages
                .lock()
                .map_err(|_| SessionError::Format("history commit cache poisoned".into()))?
                .insert(identity.id.clone());
        }
        Ok(())
    }

    fn append_persisted_record(&self, record: &JsonValue) -> Result<(), SessionError> {
        let Some(path) = self.persistence_path() else {
            return Ok(());
        };
        let writer = self.writer_for(path)?;
        let _write = writer.write().map_err(SessionError::Format)?;
        let path_str = path.to_string_lossy();
        let backend = self.backend();
        let needs_bootstrap = !backend.exists(&path_str)?
            || backend.stat(&path_str).map(|m| m.len == 0).unwrap_or(true);
        if needs_bootstrap {
            return self.save_snapshot(path, &writer);
        }
        backend.append(&path_str, format!("{}\n", record.render()).as_bytes())?;
        if let Some(id) = record
            .as_object()
            .and_then(|record| record.get("message"))
            .and_then(JsonValue::as_object)
            .and_then(|message| message.get("history"))
            .and_then(JsonValue::as_object)
            .and_then(|history| history.get("id"))
            .and_then(JsonValue::as_str)
        {
            if record
                .as_object()
                .and_then(|record| record.get("type"))
                .and_then(JsonValue::as_str)
                == Some("message")
            {
                writer
                    .observe_head(Some(id.to_owned()))
                    .map_err(SessionError::Format)?;
            }
        }

        Ok(())
    }

    fn meta_record(&self) -> Result<JsonValue, SessionError> {
        let mut object = BTreeMap::new();
        object.insert(
            "type".to_string(),
            JsonValue::String("session_meta".to_string()),
        );
        object.insert(
            "version".to_string(),
            JsonValue::Number(i64::from(self.version)),
        );
        object.insert(
            "session_id".to_string(),
            JsonValue::String(self.session_id.clone()),
        );
        object.insert(
            "created_at_ms".to_string(),
            JsonValue::Number(i64_from_u64(self.created_at_ms, "created_at_ms")?),
        );
        object.insert(
            "updated_at_ms".to_string(),
            JsonValue::Number(i64_from_u64(self.updated_at_ms, "updated_at_ms")?),
        );
        if let Some(fork) = &self.fork {
            object.insert("fork".to_string(), fork.to_json());
        }
        if let Some(snapshot) = &self.prompt_snapshot {
            object.insert("prompt_snapshot".into(), snapshot.to_json());
        }
        if let Some(identity) = &self.identity {
            object.insert("identity".to_string(), identity.to_json());
        }
        if let Some(workspace_root) = &self.workspace_root {
            object.insert(
                "workspace_root".to_string(),
                JsonValue::String(workspace_root_to_string(workspace_root)?),
            );
        }
        if let Some(model) = &self.model {
            object.insert("model".to_string(), JsonValue::String(model.clone()));
        }
        if self.mode != SessionMode::Interactive {
            object.insert(
                "mode".to_string(),
                JsonValue::String(self.mode.as_str().to_string()),
            );
        }
        self.insert_maintenance_metadata(&mut object)?;
        Ok(JsonValue::Object(object))
    }

    fn insert_maintenance_metadata(
        &self,
        object: &mut BTreeMap<String, JsonValue>,
    ) -> Result<(), SessionError> {
        if !self.maintenance_usage.is_empty() {
            object.insert(
                "maintenance_usage".into(),
                JsonValue::Array(
                    self.maintenance_usage
                        .iter()
                        .map(MaintenanceUsageReceipt::to_json)
                        .collect(),
                ),
            );
        }
        if let Some(achievement) = &self.compaction_achievement {
            object.insert("compaction_achievement".into(), achievement.to_json()?);
        }
        if let Some(report) = &self.last_compaction_report {
            let rendered = serde_json::to_string(report)
                .map_err(|error| SessionError::Format(error.to_string()))?;
            object.insert(
                "last_compaction_report".into(),
                JsonValue::parse(&rendered)?,
            );
        }
        Ok(())
    }

    /// Merge receipts by run and HTTP attempt. Return only newly known bills
    /// for the live usage tracker; replay reconstructs them from this ledger.
    pub fn merge_maintenance_usage(
        &mut self,
        receipts: &[MaintenanceUsageReceipt],
    ) -> Vec<TokenUsage> {
        let newly_known = merge_maintenance_receipts(&mut self.maintenance_usage, receipts);
        if !receipts.is_empty() {
            self.touch();
        }
        newly_known
    }

    /// Persist billing and achievement metadata without claiming another
    /// runtime's legacy message writer. A failed write keeps all known receipts
    /// in memory for the caller to report or retry.
    pub fn persist_maintenance_metadata(&self) -> Result<(), SessionError> {
        self.persist_maintenance_metadata_checked(None)
    }

    /// Guard metadata snapshot writes as well as candidate commits: failed,
    /// skipped and cancelled runs must not overwrite externally changed history.
    pub fn persist_maintenance_metadata_checked(
        &self,
        expected_revision: Option<&str>,
    ) -> Result<(), SessionError> {
        let writer = self
            .persistence_path()
            .map(|path| self.maintenance_writer_for(path))
            .transpose()?;
        let _write = writer
            .as_ref()
            .map(|(writer, _)| writer.write())
            .transpose()
            .map_err(SessionError::Format)?;
        if let Some(expected) = expected_revision {
            if self.durable_revision()?.as_deref() != Some(expected) {
                return Err(SessionError::SourceChanged);
            }
        }
        if let Some((_, true)) = &writer {
            let (durable, _) = self.durable_history_revision()?;
            if durable.as_deref() != Some(self.context_digest()?.as_str()) {
                return Err(SessionError::SourceChanged);
            }
        }
        let (Some(path), Some((writer, _))) = (self.persistence_path(), writer.as_ref()) else {
            return Ok(());
        };
        self.save_snapshot(path, writer)
    }

    fn touch(&mut self) {
        self.updated_at_ms = current_time_millis();
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

fn durable_bytes_revision(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(
        || "missing".into(),
        |bytes| format!("{:x}", Sha256::digest(bytes)),
    )
}

impl ConversationMessage {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            history: None,
            role: MessageRole::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            usage: None,
            model: None,
            duration_ms: None,
        }
    }

    #[must_use]
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        Self {
            history: None,
            role: MessageRole::Assistant,
            blocks,
            usage: None,
            model: None,
            duration_ms: None,
        }
    }

    /// Close an unanswered turn before a new external request, matching CC.
    #[must_use]
    pub(crate) fn unanswered_turn_boundary() -> Self {
        Self::assistant(vec![ContentBlock::Text {
            text: UNANSWERED_TURN_TEXT.into(),
        }])
    }

    /// An internal turn marker belongs in API history, not transcript display.
    #[must_use]
    pub fn is_unanswered_turn_boundary(&self) -> bool {
        self.role == MessageRole::Assistant
            && self.model.is_none()
            && self.usage.is_none()
            && matches!(self.blocks.as_slice(), [ContentBlock::Text { text }] if text == UNANSWERED_TURN_TEXT)
    }

    #[must_use]
    pub fn assistant_with_usage(blocks: Vec<ContentBlock>, usage: Option<TokenUsage>) -> Self {
        Self {
            history: None,
            role: MessageRole::Assistant,
            blocks,
            usage,
            model: None,
            duration_ms: None,
        }
    }

    #[must_use]
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        output: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            history: None,
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: tool_use_id.into(),
                tool_name: tool_name.into(),
                output: output.into(),
                is_error,
            }],
            usage: None,
            model: None,
            duration_ms: None,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        if let Some(history) = &self.history {
            object.insert("history".into(), history.to_json());
        }
        object.insert(
            "role".to_string(),
            JsonValue::String(
                match self.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                }
                .to_string(),
            ),
        );
        object.insert(
            "blocks".to_string(),
            JsonValue::Array(self.blocks.iter().map(ContentBlock::to_json).collect()),
        );
        if let Some(usage) = self.usage {
            object.insert("usage".to_string(), usage_to_json(usage));
        }
        if let Some(model) = &self.model {
            object.insert("model".to_string(), JsonValue::String(model.clone()));
        }
        if let Some(duration_ms) = self.duration_ms {
            object.insert(
                "duration_ms".to_string(),
                JsonValue::Number(i64::try_from(duration_ms).unwrap_or(i64::MAX)),
            );
        }
        JsonValue::Object(object)
    }

    pub(super) fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("message must be an object".to_string()))?;
        let role = match object
            .get("role")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| SessionError::Format("missing role".to_string()))?
        {
            "system" => MessageRole::System,
            "user" => MessageRole::User,
            "assistant" => MessageRole::Assistant,
            "tool" => MessageRole::Tool,
            other => {
                return Err(SessionError::Format(format!(
                    "unsupported message role: {other}"
                )))
            }
        };
        let blocks = object
            .get("blocks")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| SessionError::Format("missing blocks".to_string()))?
            .iter()
            .map(ContentBlock::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let usage = object.get("usage").map(usage_from_json).transpose()?;
        let model = object
            .get("model")
            .and_then(JsonValue::as_str)
            .map(String::from);
        let duration_ms = object
            .get("duration_ms")
            .and_then(JsonValue::as_i64)
            .and_then(|value| u64::try_from(value).ok());
        Ok(Self {
            history: object
                .get("history")
                .map(crate::session_history::MessageIdentity::from_json)
                .transpose()?
                .map(Arc::new),
            role,
            blocks,
            usage,
            model,
            duration_ms,
        })
    }
}

impl ContentBlock {
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        match self {
            Self::Text { text } => {
                object.insert("type".to_string(), JsonValue::String("text".to_string()));
                object.insert("text".to_string(), JsonValue::String(text.clone()));
            }
            Self::Image { data, mime_type } => {
                object.insert("type".to_string(), JsonValue::String("image".to_string()));
                object.insert("data".to_string(), JsonValue::String(data.clone()));
                object.insert(
                    "mime_type".to_string(),
                    JsonValue::String(mime_type.clone()),
                );
            }
            Self::Thinking {
                thinking,
                signature,
            } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("thinking".to_string()),
                );
                object.insert("thinking".to_string(), JsonValue::String(thinking.clone()));
                if let Some(signature) = signature {
                    object.insert(
                        "signature".to_string(),
                        JsonValue::String(signature.clone()),
                    );
                }
            }
            Self::RedactedThinking { data } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("redacted_thinking".to_string()),
                );
                object.insert("data".to_string(), JsonValue::String(data.clone()));
            }
            Self::ToolUse {
                id,
                name,
                input,
                thought_signature,
            } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("tool_use".to_string()),
                );
                object.insert("id".to_string(), JsonValue::String(id.clone()));
                object.insert("name".to_string(), JsonValue::String(name.clone()));
                // CC journals tool inputs as objects. Continue reading legacy
                // string records and preserve malformed historical input text.
                let parsed = JsonValue::parse(input)
                    .ok()
                    .filter(|value| matches!(value, JsonValue::Object(_)));
                object.insert(
                    "input".to_string(),
                    parsed.unwrap_or_else(|| JsonValue::String(input.clone())),
                );
                if let Some(sig) = thought_signature {
                    object.insert(
                        "thought_signature".to_string(),
                        JsonValue::String(sig.clone()),
                    );
                }
            }
            Self::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => {
                object.insert(
                    "type".to_string(),
                    JsonValue::String("tool_result".to_string()),
                );
                object.insert(
                    "tool_use_id".to_string(),
                    JsonValue::String(tool_use_id.clone()),
                );
                object.insert(
                    "tool_name".to_string(),
                    JsonValue::String(tool_name.clone()),
                );
                object.insert("output".to_string(), JsonValue::String(output.clone()));
                object.insert("is_error".to_string(), JsonValue::Bool(*is_error));
            }
        }
        JsonValue::Object(object)
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("block must be an object".to_string()))?;
        match object
            .get("type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| SessionError::Format("missing block type".to_string()))?
        {
            "text" => Ok(Self::Text {
                text: required_string(object, "text")?,
            }),
            "image" => Ok(Self::Image {
                data: required_string(object, "data")?,
                mime_type: required_string(object, "mime_type")?,
            }),
            "thinking" => Ok(Self::Thinking {
                thinking: required_string(object, "thinking")?,
                signature: object
                    .get("signature")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
            }),
            "redacted_thinking" => Ok(Self::RedactedThinking {
                data: required_string(object, "data")?,
            }),
            "tool_use" => Ok(Self::ToolUse {
                id: required_string(object, "id")?,
                name: required_string(object, "name")?,
                input: match object.get("input") {
                    Some(JsonValue::Object(input)) => JsonValue::Object(input.clone()).render(),
                    _ => required_string(object, "input")?,
                },
                thought_signature: object
                    .get("thought_signature")
                    .and_then(JsonValue::as_str)
                    .map(String::from),
            }),
            "tool_result" => Ok(Self::ToolResult {
                tool_use_id: required_string(object, "tool_use_id")?,
                tool_name: required_string(object, "tool_name")?,
                output: required_string(object, "output")?,
                is_error: object
                    .get("is_error")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| SessionError::Format("missing is_error".to_string()))?,
            }),
            other => Err(SessionError::Format(format!(
                "unsupported block type: {other}"
            ))),
        }
    }
}

impl SessionCompaction {
    pub fn to_json(&self) -> Result<JsonValue, SessionError> {
        let mut object = BTreeMap::new();
        object.insert(
            "count".to_string(),
            JsonValue::Number(i64::from(self.count)),
        );
        object.insert(
            "removed_message_count".to_string(),
            JsonValue::Number(i64_from_usize(
                self.removed_message_count,
                "removed_message_count",
            )?),
        );
        object.insert(
            "summary".to_string(),
            JsonValue::String(self.summary.clone()),
        );
        if let Some(usage) = self.usage {
            object.insert("usage".to_string(), usage_to_json(usage));
        }
        if !self.pre_compact_discovered_tools.is_empty() {
            let arr: Vec<JsonValue> = self
                .pre_compact_discovered_tools
                .iter()
                .map(|s| JsonValue::String(s.clone()))
                .collect();
            object.insert(
                "pre_compact_discovered_tools".to_string(),
                JsonValue::Array(arr),
            );
        }
        Ok(JsonValue::Object(object))
    }

    pub fn to_jsonl_record(&self) -> Result<JsonValue, SessionError> {
        let mut object = BTreeMap::new();
        object.insert(
            "type".to_string(),
            JsonValue::String("compaction".to_string()),
        );
        object.insert(
            "count".to_string(),
            JsonValue::Number(i64::from(self.count)),
        );
        object.insert(
            "removed_message_count".to_string(),
            JsonValue::Number(i64_from_usize(
                self.removed_message_count,
                "removed_message_count",
            )?),
        );
        object.insert(
            "summary".to_string(),
            JsonValue::String(self.summary.clone()),
        );
        if let Some(usage) = self.usage {
            object.insert("usage".to_string(), usage_to_json(usage));
        }
        if !self.pre_compact_discovered_tools.is_empty() {
            let arr: Vec<JsonValue> = self
                .pre_compact_discovered_tools
                .iter()
                .map(|s| JsonValue::String(s.clone()))
                .collect();
            object.insert(
                "pre_compact_discovered_tools".to_string(),
                JsonValue::Array(arr),
            );
        }
        Ok(JsonValue::Object(object))
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("compaction must be an object".to_string()))?;
        let pre_compact_discovered_tools = object
            .get("pre_compact_discovered_tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            count: required_u32(object, "count")?,
            removed_message_count: required_usize(object, "removed_message_count")?,
            summary: required_string(object, "summary")?,
            usage: object.get("usage").map(usage_from_json).transpose()?,
            pre_compact_discovered_tools,
        })
    }
}

impl SessionFork {
    #[must_use]
    pub fn to_json(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        object.insert(
            "parent_session_id".to_string(),
            JsonValue::String(self.parent_session_id.clone()),
        );
        if let Some(branch_name) = &self.branch_name {
            object.insert(
                "branch_name".to_string(),
                JsonValue::String(branch_name.clone()),
            );
        }
        JsonValue::Object(object)
    }

    fn from_json(value: &JsonValue) -> Result<Self, SessionError> {
        let object = value
            .as_object()
            .ok_or_else(|| SessionError::Format("fork metadata must be an object".to_string()))?;
        Ok(Self {
            parent_session_id: required_string(object, "parent_session_id")?,
            branch_name: object
                .get("branch_name")
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned),
        })
    }
}

impl SessionPromptEntry {
    #[must_use]
    pub fn to_jsonl_record(&self) -> JsonValue {
        let mut object = BTreeMap::new();
        object.insert(
            "type".to_string(),
            JsonValue::String("prompt_history".to_string()),
        );
        object.insert(
            "timestamp_ms".to_string(),
            JsonValue::Number(i64::try_from(self.timestamp_ms).unwrap_or(i64::MAX)),
        );
        object.insert("text".to_string(), JsonValue::String(self.text.clone()));
        JsonValue::Object(object)
    }

    fn from_json_opt(value: &JsonValue) -> Option<Self> {
        let object = value.as_object()?;
        let timestamp_ms = object
            .get("timestamp_ms")
            .and_then(JsonValue::as_i64)
            .and_then(|value| u64::try_from(value).ok())?;
        let text = object.get("text").and_then(JsonValue::as_str)?.to_string();
        Some(Self { timestamp_ms, text })
    }
}

fn message_record(message: &ConversationMessage) -> JsonValue {
    let mut object = BTreeMap::new();
    object.insert("type".to_string(), JsonValue::String("message".to_string()));
    object.insert("message".to_string(), message.to_json());
    JsonValue::Object(object)
}

fn usage_to_json(usage: TokenUsage) -> JsonValue {
    let mut object = BTreeMap::new();
    if let Some(total) = usage.reported_total_tokens {
        object.insert(
            "reported_total_tokens".into(),
            JsonValue::Number(i64::from(total)),
        );
    }
    object.insert(
        "input_tokens".to_string(),
        JsonValue::Number(i64::from(usage.input_tokens)),
    );
    object.insert(
        "output_tokens".to_string(),
        JsonValue::Number(i64::from(usage.output_tokens)),
    );
    object.insert(
        "cache_creation_input_tokens".to_string(),
        JsonValue::Number(i64::from(usage.cache_creation_input_tokens)),
    );
    object.insert(
        "cache_read_input_tokens".to_string(),
        JsonValue::Number(i64::from(usage.cache_read_input_tokens)),
    );
    if let Some(cost_units) = usage.cost_units.and_then(|value| i64::try_from(value).ok()) {
        object.insert("cost_units".to_string(), JsonValue::Number(cost_units));
    }
    if let Some(cost_currency) = usage.cost_currency {
        object.insert(
            "cost_currency".to_string(),
            JsonValue::String(cost_currency.as_str().to_string()),
        );
    }
    JsonValue::Object(object)
}

fn usage_from_json(value: &JsonValue) -> Result<TokenUsage, SessionError> {
    let object = value
        .as_object()
        .ok_or_else(|| SessionError::Format("usage must be an object".to_string()))?;
    Ok(TokenUsage {
        reported_total_tokens: object
            .get("reported_total_tokens")
            .filter(|value| !matches!(value, JsonValue::Null))
            .map(|_| required_u32(object, "reported_total_tokens"))
            .transpose()?,
        input_tokens: required_u32(object, "input_tokens")?,
        output_tokens: required_u32(object, "output_tokens")?,
        cache_creation_input_tokens: required_u32(object, "cache_creation_input_tokens")?,
        cache_read_input_tokens: required_u32(object, "cache_read_input_tokens")?,
        cost_units: optional_u64(object, "cost_units")?,
        cost_currency: parse_usage_cost_currency(
            optional_string(object, "cost_currency")?.as_deref(),
        ),
    })
}

fn optional_string(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<String>, SessionError> {
    object
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| SessionError::Format(format!("{key} must be a string")))
        })
        .transpose()
}

fn required_string(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<String, SessionError> {
    object
        .get(key)
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))
}

fn required_u32(object: &BTreeMap<String, JsonValue>, key: &str) -> Result<u32, SessionError> {
    let value = object
        .get(key)
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))?;
    u32::try_from(value).map_err(|_| SessionError::Format(format!("{key} out of range")))
}

fn required_u64(object: &BTreeMap<String, JsonValue>, key: &str) -> Result<u64, SessionError> {
    let value = object
        .get(key)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))?;
    required_u64_from_value(value, key)
}

fn optional_u64(
    object: &BTreeMap<String, JsonValue>,
    key: &str,
) -> Result<Option<u64>, SessionError> {
    object
        .get(key)
        .map(|value| required_u64_from_value(value, key))
        .transpose()
}

fn required_u64_from_value(value: &JsonValue, key: &str) -> Result<u64, SessionError> {
    let value = value
        .as_i64()
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))?;
    u64::try_from(value).map_err(|_| SessionError::Format(format!("{key} out of range")))
}

fn required_usize(object: &BTreeMap<String, JsonValue>, key: &str) -> Result<usize, SessionError> {
    let value = object
        .get(key)
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| SessionError::Format(format!("missing {key}")))?;
    usize::try_from(value).map_err(|_| SessionError::Format(format!("{key} out of range")))
}

fn i64_from_u64(value: u64, key: &str) -> Result<i64, SessionError> {
    i64::try_from(value)
        .map_err(|_| SessionError::Format(format!("{key} out of range for JSON number")))
}

fn i64_from_usize(value: usize, key: &str) -> Result<i64, SessionError> {
    i64::try_from(value)
        .map_err(|_| SessionError::Format(format!("{key} out of range for JSON number")))
}

fn workspace_root_to_string(path: &Path) -> Result<String, SessionError> {
    path.to_str().map(ToOwned::to_owned).ok_or_else(|| {
        SessionError::Format(format!(
            "workspace_root is not valid UTF-8: {}",
            path.display()
        ))
    })
}

/// Return a model-safe view while preserving structured source records for
/// replay, undo and persistence. Healthy history is borrowed unchanged.
#[must_use]
pub fn model_tool_history(messages: &[ConversationMessage]) -> Cow<'_, [ConversationMessage]> {
    let repairs = ToolHistoryRepairs::plan(messages);
    if repairs.is_empty() {
        return Cow::Borrowed(messages);
    }
    let mut history = messages.to_vec();
    repairs.apply(&mut history);
    Cow::Owned(history)
}

/// A single reconciliation plan serves persisted, imported and active history.
/// Planning borrows the valid prefix; only damaged histories allocate repairs.
#[derive(Default)]
struct ToolHistoryRepairs {
    missing: Vec<(usize, Vec<ConversationMessage>)>,
    unpaired: Vec<(usize, usize)>,
}

impl ToolHistoryRepairs {
    fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.unpaired.is_empty()
    }

    fn plan(messages: &[ConversationMessage]) -> Self {
        let mut repairs = Self::default();
        let mut pending: Vec<(&str, &str, Option<&str>)> = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            // Anthropic-format history and fork prefixes put results in User
            // messages, while our executor writes Tool messages. Both are the
            // same batch. Plain user input ends a batch, even after a cancel.
            let is_result_message = matches!(message.role, MessageRole::Tool | MessageRole::User)
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
            if !is_result_message {
                repairs.finish_batch(index, &mut pending);
            }
            if message.role == MessageRole::Assistant {
                pending.extend(message.blocks.iter().filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, .. } => Some((
                        id.as_str(),
                        name.as_str(),
                        crate::session_history::tool_origin(message, id),
                    )),
                    _ => None,
                }));
            }
            for (block_index, block) in message.blocks.iter().enumerate() {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                    if let Some(position) = pending
                        .iter()
                        .position(|(id, _, origin)| {
                            *id == tool_use_id
                                && crate::session_history::tool_origin(message, tool_use_id)
                                    .is_none_or(|expected| *origin == Some(expected))
                        })
                        .filter(|_| is_result_message)
                    {
                        pending.remove(position);
                    } else {
                        repairs.unpaired.push((index, block_index));
                    }
                }
            }
        }
        repairs.finish_batch(messages.len(), &mut pending);
        repairs
    }

    fn finish_batch(&mut self, before: usize, pending: &mut Vec<(&str, &str, Option<&str>)>) {
        if pending.is_empty() {
            return;
        }
        self.missing.push((before, pending.drain(..).map(|(id, name, _)| {
            ConversationMessage::tool_result(id, name,
                "[Tool execution was interrupted before a final result was received. Side effects may already have occurred. Inspect the current state before retrying.]", true)
        }).collect()));
    }

    fn apply(self, messages: &mut Vec<ConversationMessage>) -> usize {
        let count = self.unpaired.len()
            + self
                .missing
                .iter()
                .map(|(_, batch)| batch.len())
                .sum::<usize>();
        for (message, block) in self.unpaired {
            let ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } = &messages[message].blocks[block]
            else {
                unreachable!("repair plan refers to a tool result")
            };
            // Keep output, provenance and failure state as historical context.
            // Re-running a tool or inventing its arguments could repeat writes.
            messages[message].blocks[block] = ContentBlock::Text {
                text: format!("[Unpaired historical tool result: {tool_name}, id={tool_use_id}, is_error={is_error}]\n{output}"),
            };
        }
        Self {
            missing: self.missing,
            unpaired: Vec::new(),
        }
        .apply_missing(messages);
        count
    }

    /// Only interrupted calls change durable history. Unpaired result records
    /// retain their type and metadata; the model projection renders them as text.
    fn apply_missing(self, messages: &mut Vec<ConversationMessage>) -> usize {
        let count = self.missing.iter().map(|(_, batch)| batch.len()).sum();
        for (before, batch) in self.missing.into_iter().rev() {
            messages.splice(before..before, batch);
        }
        count
    }
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn current_time_millis() -> u64 {
    let wall_clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default();

    let mut candidate = wall_clock;
    loop {
        let previous = LAST_TIMESTAMP_MS.load(Ordering::Relaxed);
        if candidate <= previous {
            candidate = previous.saturating_add(1);
        }
        match LAST_TIMESTAMP_MS.compare_exchange(
            previous,
            candidate,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return candidate,
            Err(actual) => candidate = actual.saturating_add(1),
        }
    }
}

fn generate_session_id() -> String {
    // Durable objects can share a flat store across independent processes and
    // nodes. Wall clocks and process-local counters cannot identify that object.
    format!("session-{}", uuid::Uuid::new_v4())
}

fn write_atomic_with(
    backend: &dyn FsBackend,
    path: &str,
    contents: &str,
) -> Result<(), SessionError> {
    if let Some(parent) = Path::new(path).parent() {
        let _ = backend.create_dir_all(&parent.to_string_lossy());
    }
    backend.write_atomic(path, contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        current_time_millis, ContentBlock, ConversationMessage, MessageRole, Session, SessionFork,
    };
    use crate::json::JsonValue;
    use crate::usage::{TokenUsage, UsageCostCurrency, UsageTracker};
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn session_timestamps_are_monotonic_under_tight_loops() {
        let first = current_time_millis();
        let second = current_time_millis();
        let third = current_time_millis();

        assert!(first < second);
        assert!(second < third);
    }

    #[test]
    fn thinking_block_signature_survives_persistence() {
        // A resumed session replays its history. If the signature is lost on
        // disk, `convert_messages` drops the thinking block and the resumed
        // session silently rebuilds its whole cached prefix on every tool
        // round-trip — the failure this field exists to prevent.
        let block = ContentBlock::Thinking {
            thinking: "weighing the options".to_string(),
            signature: Some("sig-abc".to_string()),
        };

        let restored = ContentBlock::from_json(&block.to_json()).expect("thinking block parses");

        assert_eq!(restored, block);
    }

    #[test]
    fn redacted_thinking_block_survives_persistence() {
        // Same stake as the signed block above: a resumed session has to replay
        // the ciphertext it was given, or the prefix is rebuilt every round-trip.
        let block = ContentBlock::RedactedThinking {
            data: "\"EroBCkYIBBgCKkBw…opaque…\"".to_string(),
        };

        let restored = ContentBlock::from_json(&block.to_json()).expect("redacted block parses");

        assert_eq!(restored, block);
    }

    #[test]
    fn offload_tool_result_writes_full_blob_and_returns_path_and_size() {
        let session_file = temp_session_path("offload");
        let session = Session::new().with_persistence_path(session_file.clone());

        let id = "tool_use_ABC-123";
        let payload = "x".repeat(50_000);
        let (path, size) = session
            .offload_tool_result(id, payload.as_bytes())
            .expect("offload should write");

        assert_eq!(size, payload.len() as u64);

        // The blob lands under the session's own tool-results/<session-id>/ dir;
        // the id here is already filesystem-safe so it round-trips unchanged.
        let dir = session.tool_results_dir().expect("tool-results dir");
        assert_eq!(PathBuf::from(&path), dir.join(id));

        let written = fs::read(&path).expect("offloaded file should exist");
        assert_eq!(written, payload.as_bytes());

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
        let _ = fs::remove_file(&session_file);
    }

    #[test]
    fn offload_tool_result_sanitizes_ids_to_stay_in_dir() {
        let session_file = temp_session_path("offload-sanitize");
        let session = Session::new().with_persistence_path(session_file.clone());

        let (path, _size) = session
            .offload_tool_result("../../etc/passwd", b"data")
            .expect("offload should write");

        // Every char outside [A-Za-z0-9_-] becomes '_', so a hostile id can
        // neither escape the tool-results dir nor reintroduce separators.
        let dir = session.tool_results_dir().expect("tool-results dir");
        let written = PathBuf::from(&path);
        assert!(
            written.starts_with(&dir),
            "offloaded path must stay under tool-results"
        );
        let name = written.file_name().unwrap().to_string_lossy();
        assert!(
            !name.contains('/') && !name.contains('\\') && !name.contains(".."),
            "sanitized id must not contain path separators or .."
        );

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
        let _ = fs::remove_file(&session_file);
    }

    #[test]
    fn persists_and_restores_session_jsonl() {
        let mut session = Session::new();
        session
            .push_user_text("hello")
            .expect("user message should append");
        session
            .push_message(ConversationMessage::assistant_with_usage(
                vec![
                    ContentBlock::Text {
                        text: "thinking".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tool-1".to_string(),
                        name: "bash".to_string(),
                        input: "echo hi".to_string(),
                        thought_signature: None,
                    },
                ],
                Some(TokenUsage {
                    reported_total_tokens: None,
                    input_tokens: 10,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 2,
                    cost_units: Some(43_700),
                    cost_currency: Some(UsageCostCurrency::SudoPoint),
                }),
            ))
            .expect("assistant message should append");
        session
            .push_message(ConversationMessage::tool_result(
                "tool-1", "bash", "hi", false,
            ))
            .expect("tool result should append");

        let path = temp_session_path("jsonl");
        session.save_to_path(&path).expect("session should save");
        let restored = Session::load_from_path(&path).expect("session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        assert_eq!(restored, session);
        assert_eq!(restored.messages[2].role, MessageRole::Tool);
        assert_eq!(
            restored.messages[1].usage.expect("usage").total_tokens(),
            17
        );
        assert_eq!(
            restored.messages[1].usage.expect("usage").cost_units,
            Some(43_700)
        );
        assert_eq!(
            restored.messages[1].usage.expect("usage").cost_currency,
            Some(UsageCostCurrency::SudoPoint)
        );
        assert_eq!(
            UsageTracker::from_session(&restored)
                .cumulative_usage()
                .cost_units,
            Some(43_700)
        );
        assert_eq!(restored.session_id, session.session_id);
    }

    #[test]
    fn sanitizes_orphan_tool_use_on_load() {
        // Assistant emits a tool_use; the tool call is interrupted so the very
        // next message is plain user text with no tool_result. The Anthropic API
        // rejects this on resume — load must backfill a synthetic tool_result.
        let mut session = Session::new();
        session.push_user_text("go").expect("user append");
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: "tool-x".to_string(),
                    name: "Agent".to_string(),
                    input: "{}".to_string(),
                    thought_signature: None,
                },
            ]))
            .expect("assistant append");
        session.push_user_text("stop").expect("user append");

        let path = temp_session_path("orphan");
        session.save_to_path(&path).expect("session should save");
        let restored = Session::load_from_path(&path).expect("session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        let pos = restored
            .messages
            .iter()
            .position(|m| {
                m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "tool-x"))
            })
            .expect("tool_use present");
        let next = restored
            .messages
            .get(pos + 1)
            .expect("message after tool_use");
        assert!(
            next.blocks.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, is_error, .. }
                    if tool_use_id == "tool-x" && *is_error
            )),
            "a synthetic error tool_result must immediately follow the orphan tool_use"
        );
    }

    #[test]
    fn repairs_partial_batch_before_later_user_and_is_idempotent() {
        let mut session = Session::new();
        let path = temp_session_path("partial-batch");
        session = session.with_persistence_path(path.clone());
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: "first".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                    thought_signature: None,
                },
                ContentBlock::ToolUse {
                    id: "second".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                    thought_signature: None,
                },
            ]))
            .expect("assistant append");
        session
            .push_message(ConversationMessage::tool_result(
                "first", "bash", "ok", false,
            ))
            .expect("first result append");
        session.push_user_text("still there?").expect("user append");

        assert_eq!(session.repair_orphan_tool_uses().expect("repair"), 1);
        assert_eq!(session.repair_orphan_tool_uses().expect("second repair"), 0);
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[3].role, MessageRole::User);
        assert!(matches!(
            &session.messages[2].blocks[0],
            ContentBlock::ToolResult { tool_use_id, is_error, output, .. }
                if tool_use_id == "second" && *is_error
                    && output.contains("Side effects may already have occurred")
                    && output.contains("before retrying")
        ));
        let persisted = fs::read_to_string(&path).expect("repair persisted");
        assert!(persisted.contains("\"tool_use_id\":\"second\""));
        let restored = Session::load_from_path(&path).expect("reload repaired session");
        assert_eq!(restored.messages, session.messages);
        fs::remove_file(&path).expect("remove test session");
    }

    #[test]
    fn sanitize_leaves_well_formed_history_unchanged() {
        // A complete tool_use → tool_result pair must not gain a duplicate.
        let mut session = Session::new();
        session.push_user_text("hi").expect("user append");
        session
            .push_message(ConversationMessage::assistant(vec![
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                    thought_signature: None,
                },
            ]))
            .expect("assistant append");
        session
            .push_message(ConversationMessage::tool_result("t1", "bash", "ok", false))
            .expect("tool result append");

        let before = session.messages.len();
        let inserted = session.sanitize_orphan_tool_uses();
        assert_eq!(inserted, 0, "well-formed history needs no repair");
        assert_eq!(session.messages.len(), before);
    }

    #[test]
    fn loads_jsonl_usage_without_cost_fields() {
        let path = write_temp_session_file(
            "legacy-usage",
            concat!(
                "{\"type\":\"session_meta\",\"version\":1,\"session_id\":\"legacy-usage\",\"created_at_ms\":1,\"updated_at_ms\":1}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"blocks\":[{\"type\":\"text\",\"text\":\"old\"}],\"usage\":{\"input_tokens\":10,\"output_tokens\":4,\"cache_creation_input_tokens\":1,\"cache_read_input_tokens\":2}}}\n"
            ),
        );

        let restored = Session::load_from_path(&path).expect("legacy usage session should load");
        fs::remove_file(&path).expect("temp file should be removable");
        let usage = restored.messages[0].usage.expect("usage");

        assert_eq!(usage.total_tokens(), 17);
        assert_eq!(usage.cost_units, None);
        assert_eq!(usage.cost_currency, None);
    }

    #[test]
    fn loads_legacy_session_json_object() {
        let path = temp_session_path("legacy");
        let legacy = JsonValue::Object(
            [
                ("version".to_string(), JsonValue::Number(1)),
                (
                    "messages".to_string(),
                    JsonValue::Array(vec![ConversationMessage::user_text("legacy").to_json()]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        fs::write(&path, legacy.render()).expect("legacy file should write");

        let restored = Session::load_from_path(&path).expect("legacy session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        assert_eq!(restored.messages.len(), 1);
        assert_eq!(
            restored.messages[0].blocks,
            ConversationMessage::user_text("legacy").blocks
        );
        assert!(!restored.session_id.is_empty());
    }

    #[test]
    fn appends_messages_to_persisted_jsonl_session() {
        let path = temp_session_path("append");
        let mut session = Session::new().with_persistence_path(path.clone());
        session
            .save_to_path(&path)
            .expect("initial save should succeed");
        session
            .push_user_text("hi")
            .expect("user append should succeed");
        session
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "hello".to_string(),
            }]))
            .expect("assistant append should succeed");

        let restored = Session::load_from_path(&path).expect("session should replay from jsonl");
        fs::remove_file(&path).expect("temp file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0], session.messages[0]);
    }

    #[test]
    fn persists_compaction_metadata() {
        let path = temp_session_path("compaction");
        let mut session = Session::new();
        session
            .push_user_text("before")
            .expect("message should append");
        session.record_compaction_with_usage(
            "summarized earlier work",
            4,
            Some(TokenUsage {
                reported_total_tokens: None,
                input_tokens: 10,
                output_tokens: 4,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 2,
                cost_units: Some(43_700),
                cost_currency: Some(UsageCostCurrency::SudoPoint),
            }),
            BTreeSet::new(),
        );
        session.save_to_path(&path).expect("session should save");

        let restored = Session::load_from_path(&path).expect("session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        let compaction = restored.compaction.expect("compaction metadata");
        assert_eq!(compaction.count, 1);
        assert_eq!(compaction.removed_message_count, 4);
        assert!(compaction.summary.contains("summarized"));
        let usage = compaction.usage.expect("compaction usage");
        assert_eq!(usage.total_tokens(), 17);
        assert_eq!(usage.cost_units, Some(43_700));
        assert_eq!(usage.cost_currency, Some(UsageCostCurrency::SudoPoint));
    }

    #[test]
    fn loads_legacy_compaction_without_usage() {
        let path = write_temp_session_file(
            "legacy-compaction",
            concat!(
                "{\"type\":\"session_meta\",\"version\":1,\"session_id\":\"legacy-compaction\",\"created_at_ms\":1,\"updated_at_ms\":1}\n",
                "{\"type\":\"compaction\",\"count\":1,\"removed_message_count\":4,\"summary\":\"summarized earlier work\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"after\"}]}}\n"
            ),
        );

        let restored =
            Session::load_from_path(&path).expect("legacy compaction session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        let compaction = restored.compaction.expect("compaction metadata");
        assert_eq!(compaction.count, 1);
        assert_eq!(compaction.removed_message_count, 4);
        assert_eq!(compaction.usage, None);
    }

    #[test]
    fn forks_sessions_with_branch_metadata_and_persists_it() {
        let path = temp_session_path("fork");
        let mut session = Session::new();
        session
            .push_user_text("before fork")
            .expect("message should append");

        let forked = session
            .fork(Some("investigation".to_string()))
            .with_persistence_path(path.clone());
        forked
            .save_to_path(&path)
            .expect("forked session should save");

        let restored = Session::load_from_path(&path).expect("forked session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        assert_ne!(restored.session_id, session.session_id);
        assert_eq!(
            restored.fork,
            Some(SessionFork {
                parent_session_id: session.session_id,
                branch_name: Some("investigation".to_string()),
            })
        );
        assert_eq!(restored.messages, forked.messages);
    }

    #[test]
    fn rejects_jsonl_record_without_type() {
        // given
        let path = write_temp_session_file(
            "missing-type",
            r#"{"message":{"role":"user","blocks":[{"type":"text","text":"hello"}]}}"#,
        );

        // when
        let error = Session::load_from_path(&path)
            .expect_err("session should reject JSONL records without a type");

        // then
        assert!(error.to_string().contains("missing type"));
        fs::remove_file(path).expect("temp file should be removable");
    }

    #[test]
    fn rejects_jsonl_message_record_without_message_payload() {
        // given
        let path = write_temp_session_file("missing-message", r#"{"type":"message"}"#);

        // when
        let error = Session::load_from_path(&path)
            .expect_err("session should reject JSONL message records without message payload");

        // then
        assert!(error.to_string().contains("missing message"));
        fs::remove_file(path).expect("temp file should be removable");
    }

    #[test]
    fn rejects_jsonl_record_with_unknown_type() {
        // given
        let path = write_temp_session_file("unknown-type", r#"{"type":"mystery"}"#);

        // when
        let error = Session::load_from_path(&path)
            .expect_err("session should reject unknown JSONL record types");

        // then
        assert!(error.to_string().contains("unsupported JSONL record type"));
        fs::remove_file(path).expect("temp file should be removable");
    }

    #[test]
    fn rejects_legacy_session_json_without_messages() {
        // given
        let session = JsonValue::Object(
            [("version".to_string(), JsonValue::Number(1))]
                .into_iter()
                .collect(),
        );

        // when
        let error = Session::from_json(&session)
            .expect_err("legacy session objects should require messages");

        // then
        assert!(error.to_string().contains("missing messages"));
    }

    #[test]
    fn normalizes_blank_fork_branch_name_to_none() {
        // given
        let session = Session::new();

        // when
        let forked = session.fork(Some("   ".to_string()));

        // then
        assert_eq!(forked.fork.expect("fork metadata").branch_name, None);
    }

    #[test]
    fn rejects_unknown_content_block_type() {
        // given
        let block = JsonValue::Object(
            [("type".to_string(), JsonValue::String("unknown".to_string()))]
                .into_iter()
                .collect(),
        );

        // when
        let error = ContentBlock::from_json(&block)
            .expect_err("content blocks should reject unknown types");

        // then
        assert!(error.to_string().contains("unsupported block type"));
    }

    #[test]
    fn persists_workspace_root_round_trip_and_forks_inherit_it() {
        // given
        let path = temp_session_path("workspace-root");
        let workspace_root = PathBuf::from("/tmp/b4-phantom-diag");
        let mut session = Session::new().with_workspace_root(workspace_root.clone());
        session
            .push_user_text("write to the right cwd")
            .expect("user message should append");

        // when
        session
            .save_to_path(&path)
            .expect("workspace-bound session should save");
        let restored = Session::load_from_path(&path).expect("session should load");
        let forked = restored.fork(Some("phantom-diag".to_string()));
        fs::remove_file(&path).expect("temp file should be removable");

        // then
        assert_eq!(restored.workspace_root(), Some(workspace_root.as_path()));
        assert_eq!(forked.workspace_root(), Some(workspace_root.as_path()));
    }

    #[test]
    fn per_message_model_round_trips_through_jsonl() {
        // given
        let path = temp_session_path("per-message-model");
        let mut session = Session::new();
        let mut assistant_msg = ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "response".to_string(),
        }]);
        assistant_msg.model = Some("claude-sonnet-4-6".to_string());
        session
            .push_user_text("hello")
            .expect("user message should append");
        session
            .push_message(assistant_msg)
            .expect("assistant message should append");

        // when
        session.save_to_path(&path).expect("session should save");
        let restored = Session::load_from_path(&path).expect("session should load");
        fs::remove_file(&path).expect("temp file should be removable");

        // then
        assert_eq!(
            restored.messages[1].model.as_deref(),
            Some("claude-sonnet-4-6"),
            "assistant message model should survive JSONL round-trip"
        );
        assert_eq!(
            restored.messages[0].model, None,
            "user message model should be None"
        );
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-session-{label}-{nanos}.json"))
    }

    fn write_temp_session_file(label: &str, contents: &str) -> PathBuf {
        let path = temp_session_path(label);
        fs::write(&path, format!("{contents}\n")).expect("temp session file should write");
        path
    }
}

/// Per-worktree session isolation: returns a session directory namespaced
/// by the workspace fingerprint of the given working directory.
/// This prevents parallel `scode serve` instances from colliding.
/// Called by external consumers (e.g. sudocodehip) to enumerate sessions for a CWD.
#[allow(dead_code)]
pub fn workspace_sessions_dir(cwd: &std::path::Path) -> Result<std::path::PathBuf, SessionError> {
    let store = crate::session_control::SessionStore::from_cwd(cwd)
        .map_err(|e| SessionError::Io(std::io::Error::other(e.to_string())))?;
    Ok(store.sessions_dir().to_path_buf())
}

#[cfg(test)]
mod workspace_sessions_dir_tests {
    use super::*;
    use std::fs;

    #[test]
    fn workspace_sessions_dir_returns_fingerprinted_path_for_valid_cwd() {
        let tmp = std::env::temp_dir().join("scode-session-dir-test");
        fs::create_dir_all(&tmp).expect("create temp dir");

        let result = workspace_sessions_dir(&tmp);
        assert!(
            result.is_ok(),
            "workspace_sessions_dir should succeed for a valid CWD, got: {result:?}"
        );
        let dir = result.unwrap();
        // The returned path should be non-empty and end with a hash component
        assert!(!dir.as_os_str().is_empty());
        // Two calls with the same CWD should produce identical paths (deterministic)
        let result2 = workspace_sessions_dir(&tmp).unwrap();
        assert_eq!(dir, result2, "workspace_sessions_dir must be deterministic");

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn workspace_sessions_dir_differs_for_different_cwds() {
        let tmp_a = std::env::temp_dir().join("scode-session-dir-a");
        let tmp_b = std::env::temp_dir().join("scode-session-dir-b");
        fs::create_dir_all(&tmp_a).expect("create dir a");
        fs::create_dir_all(&tmp_b).expect("create dir b");

        let dir_a = workspace_sessions_dir(&tmp_a).expect("dir a");
        let dir_b = workspace_sessions_dir(&tmp_b).expect("dir b");
        assert_ne!(
            dir_a, dir_b,
            "different CWDs must produce different session dirs"
        );

        fs::remove_dir_all(&tmp_a).ok();
        fs::remove_dir_all(&tmp_b).ok();
    }
}
