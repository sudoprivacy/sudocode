//! Unified agent mailbox envelope + filesystem-backed local mailbox.
//!
//! [`MailboxEnvelope`] is the SINGLE envelope type for all inter-agent
//! messaging — both the local JSONL mailbox (`.sudocode-inbox/*.jsonl`,
//! used by coordinator sub-agents) and the nexus DT_STREAM A2A path
//! (`/agents/<name>/chat-with-me`, used for cross-machine messaging).
//! The a2a substrate's `from`-stamping hook operates on raw JSON and
//! only touches `from`, so the extra fields are transparent to it.
//!
//! ## Wire compatibility
//!
//! The canonical field name for the message body is `body` (matching
//! the nexus a2a convention). The `text` alias is accepted on read for
//! backward compat with existing local JSONL data written before the
//! unification.
//!
//! All fields beyond `{from, to, body}` carry `#[serde(default)]` and
//! `skip_serializing_if`, so:
//! - An envelope written by the old 3-field nexus path deserialises
//!   cleanly (extras default to zero/None/empty).
//! - An envelope written with extras is ignored by old readers that
//!   use `a2a::MailboxEnvelope` (which silently drops unknown fields).
//!
//! ## Local JSONL mailbox
//!
//! Each recipient has one append-only JSONL file at
//! `<workspace>/.sudocode-inbox/<recipient>.jsonl`.
//!
//! For structured messages (`shutdown_request`,
//! `shutdown_response`, `plan_approval_response`), `body` is the
//! JSON-encoded structured payload and `kind` is that message type.
//! Recipients parse `kind` before deciding how to interpret `body`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Unified mailbox envelope — the ONE envelope type for all inter-agent
/// messaging (local JSONL + nexus DT_STREAM A2A).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxEnvelope {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(default)]
    pub to: String,
    /// Message body. For `kind == "message"` this is user-facing text.
    /// For structured `kind` values it is the JSON-encoded payload.
    /// Accepts `"text"` on read for backward compat with old local JSONL.
    #[serde(default, alias = "text")]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Unix seconds. `now_secs()` at write time for local JSONL;
    /// 0 when read from nexus (the stream carries its own ordering).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Envelope kind: `message` (default) | `shutdown_request` |
    /// `shutdown_response` | `plan_approval_response` | `task_notification`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Correlator for shutdown/plan-approval request/response pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

impl MailboxEnvelope {
    /// Serialise to JSON bytes (the nexus DT_STREAM wire format).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Parse from JSON bytes. Returns `None` on non-JSON content.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Default DT_STREAM capacity for mailbox streams (matches
/// `a2a::mailbox_stamping_policy::MAILBOX_STREAM_CAPACITY`).
pub const DEFAULT_STREAM_CAPACITY: u64 = 65_536;

/// Serialization-friendly kind constants — recipients match on these
/// strings.
pub mod kinds {
    pub const MESSAGE: &str = "message";
    pub const SHUTDOWN_REQUEST: &str = "shutdown_request";
    pub const SHUTDOWN_RESPONSE: &str = "shutdown_response";
    pub const PLAN_APPROVAL_RESPONSE: &str = "plan_approval_response";
    /// Coordinator-mode push notification — a sub-agent's terminal
    /// state was reached and it emitted a `<task-notification>` XML
    /// block into the coordinator's inbox. The coordinator's REPL
    /// drains these between turns and prepends them to the next
    /// user prompt so the model sees them mid-conversation.
    pub const TASK_NOTIFICATION: &str = "task_notification";
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Process-global lock so concurrent `send` calls into the same
/// recipient's mailbox never interleave partial JSON lines. The lock
/// covers only the "open, append, flush, close" critical section —
/// contention is negligible in practice because most agents write to
/// distinct recipients.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Resolve the mailbox directory for a workspace root. Callers must
/// ensure the directory exists before writing; [`append_envelope`]
/// creates it lazily.
#[must_use]
pub fn mailbox_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".sudocode-inbox")
}

/// Resolve the mailbox file for a recipient. The recipient string is
/// used verbatim as the filename stem — callers must sanitize
/// forbidden filesystem characters if the recipient name might contain
/// path separators. In practice recipient names come from agent
/// registries whose IDs are already `[a-zA-Z0-9_-]+`.
#[must_use]
pub fn mailbox_path(workspace_root: &Path, recipient: &str) -> PathBuf {
    mailbox_dir(workspace_root).join(format!("{recipient}.jsonl"))
}

/// Append one envelope to the recipient's mailbox. Creates the parent
/// directory and file as needed.
///
/// # Errors
///
/// Returns a `String` error when the mailbox directory can't be
/// created, the file can't be opened for append, or the JSON encoding
/// / write fails. The critical section is guarded by [`WRITE_LOCK`]
/// so concurrent calls to the same file cannot produce partial lines.
pub fn append_envelope(
    workspace_root: &Path,
    recipient: &str,
    mut envelope: MailboxEnvelope,
) -> Result<PathBuf, String> {
    if envelope.timestamp == 0 {
        envelope.timestamp = now_secs();
    }
    if envelope.to.is_empty() {
        envelope.to = recipient.to_string();
    }
    let dir = mailbox_dir(workspace_root);
    fs::create_dir_all(&dir).map_err(|e| format!("create mailbox dir: {e}"))?;
    let path = mailbox_path(workspace_root, recipient);
    let mut line =
        serde_json::to_string(&envelope).map_err(|e| format!("serialize envelope: {e}"))?;
    line.push('\n');
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| "mailbox write lock poisoned".to_string())?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open mailbox {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("write mailbox {}: {e}", path.display()))?;
    Ok(path)
}

/// Read the recipient's mailbox as a Vec<MailboxEnvelope>. Skips
/// lines that fail to parse — the receiver keeps making progress if
/// a malformed line ever gets committed by a buggy writer.
///
/// # Errors
///
/// Returns a `String` error only when the file exists but can't be
/// opened (permissions, IO). A missing mailbox is treated as an empty
/// vec — this is the fresh-workspace case.
pub fn read_all(workspace_root: &Path, recipient: &str) -> Result<Vec<MailboxEnvelope>, String> {
    let path = mailbox_path(workspace_root, recipient);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text =
        fs::read_to_string(&path).map_err(|e| format!("read mailbox {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(env) = serde_json::from_str::<MailboxEnvelope>(trimmed) {
            out.push(env);
        }
    }
    Ok(out)
}

/// Read a mailbox JSONL file by path (used by [`crate::mailbox::Mailbox`]).
pub fn read_all_from_path(path: &str) -> Result<Vec<MailboxEnvelope>, String> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(p).map_err(|e| format!("read mailbox {path}: {e}"))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(env) = serde_json::from_str::<MailboxEnvelope>(trimmed) {
            out.push(env);
        }
    }
    Ok(out)
}

/// Convenience: enumerate every recipient that currently has a
/// mailbox. Used by the broadcast path to skip self.
///
/// # Errors
///
/// Returns a `String` error when the mailbox dir exists but can't be
/// read. A missing dir is treated as no recipients (fresh workspace).
pub fn list_recipients(workspace_root: &Path) -> Result<Vec<String>, String> {
    let dir = mailbox_dir(workspace_root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("read mailbox dir: {e}"))? {
        let entry = entry.map_err(|e| format!("read mailbox dir entry: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}
