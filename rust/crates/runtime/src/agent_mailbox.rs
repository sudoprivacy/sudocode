//! Filesystem-backed per-agent mailbox for the SendMessage inter-agent
//! coordination surface.
//!
//! Ported semantics from `sudoprivacy/claude-code`'s
//! `utils/teammateMailbox.ts` — the flag-off default path. Each
//! recipient has one append-only JSONL file at
//! `<workspace>/.sudocode-inbox/<recipient>.jsonl`. The receiving
//! agent (e.g. a task launched by `Agent(run_in_background=true)`) is
//! expected to read new lines from its own inbox and process them at
//! its next tool round. This crate only writes; consumption lives
//! wherever the receiving agent loop lives.
//!
//! The mailbox directory is intentionally under the workspace root
//! (not `~/.nexus/sudocode`) so that per-project state stays with the
//! project and is naturally cleaned when the workspace is discarded.
//!
//! ## Envelope shape (mirrors CC-fork)
//!
//! Each JSONL line is a `MailboxEnvelope`:
//!
//! ```json
//! {
//!   "from": "team-lead",
//!   "to": "researcher",
//!   "text": "look into the failing test",
//!   "summary": "investigate flaky test",
//!   "timestamp": 1234567890,
//!   "color": null,
//!   "kind": "message"
//! }
//! ```
//!
//! For structured messages (`shutdown_request`,
//! `shutdown_response`, `plan_approval_response`), `text` is the
//! JSON-encoded structured body and `kind` is that message type.
//! Recipients parse `kind` before deciding how to interpret `text`.
//! This shape lets a single JSONL sink handle both plain text and
//! structured envelopes without a schema fork.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A single mailbox envelope. `serde` derives ensure the JSONL
/// wire-format is stable across producer/consumer versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEnvelope {
    pub from: String,
    pub to: String,
    /// Message body. For `kind == "message"` this is user-facing text.
    /// For structured `kind` values it is the JSON-encoded body of the
    /// structured message (parsed by the recipient).
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Unix seconds. `now_secs()` at write time.
    pub timestamp: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Envelope kind: `message` (default) | `shutdown_request` |
    /// `shutdown_response` | `plan_approval_response`.
    pub kind: String,
    /// Correlator for shutdown/plan-approval request/response pairs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

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

/// Process-global lock so concurrent SendMessage calls into the same
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
