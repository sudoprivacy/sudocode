//! Persistent peer descriptions, rooted in the mailbox's agent namespace.

use std::io;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs_backend::{FsBackend, VersionToken, WriteOutcome};
use crate::mailbox::MailboxBackend;

pub const DESCRIPTION_FILE: &str = "description.json";
pub const MAX_DESCRIPTION_CHARS: usize = 160;
const MAX_DESCRIPTION_BYTES: u64 = 4096;

/// Last successfully published description of a persistent peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDescription {
    pub description: String,
    pub updated_at: String,
    pub source_session_id: String,
    /// Self-reported frontend, machine and workspace for the attached peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    attachment_only: bool,
}

/// Storage selected by the actual mailbox, independently of the shell workspace.
#[derive(Clone)]
pub struct DescriptionStore {
    fs: MailboxBackend,
    path: String,
}

/// The description observed before a compaction request starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionSnapshot {
    previous: Option<Vec<u8>>,
    version: Option<VersionToken>,
    conditional: bool,
    source_session_id: String,
}

/// Optional metadata carried alongside a checkpoint, never inside its context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionUpdate {
    snapshot: DescriptionSnapshot,
    description: String,
}

impl DescriptionStore {
    #[must_use]
    #[inline]
    pub(crate) fn new(fs: MailboxBackend, path: String) -> Self {
        Self { fs, path }
    }

    /// Share the mailbox's pending binding; construction never requires a bundle.
    #[inline]
    fn backend(&self) -> io::Result<Arc<dyn FsBackend>> {
        self.fs.resolve().map_err(io::Error::other)
    }

    fn read_bytes(&self, fs: &dyn FsBackend) -> io::Result<Option<Vec<u8>>> {
        match fs.stat(&self.path) {
            Ok(meta) if meta.len > MAX_DESCRIPTION_BYTES => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "peer description exceeds size limit",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let bytes = fs.read(&self.path)?;
        if bytes.len() as u64 > MAX_DESCRIPTION_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "peer description exceeds size limit",
            ));
        }
        Ok(Some(bytes))
    }

    pub fn read(&self) -> io::Result<Option<AgentDescription>> {
        self.read_bytes(self.backend()?.as_ref())?
            .map(|bytes| {
                let value: AgentDescription =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if !valid_description(&value.description)
                    || DateTime::parse_from_rfc3339(&value.updated_at).is_err()
                    || value
                        .attachment
                        .as_ref()
                        .is_some_and(|label| !valid_description(label))
                    || value.source_session_id.is_empty()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid peer description metadata",
                    ));
                }
                Ok(value)
            })
            .transpose()
    }

    pub fn snapshot(&self, source_session_id: &str) -> io::Result<DescriptionSnapshot> {
        let fs = self.backend()?;
        let (previous, version, conditional) = match fs.read_versioned(&self.path) {
            Ok(value) => {
                let (bytes, token) =
                    value.map_or((None, None), |(bytes, token)| (Some(bytes), Some(token)));
                (bytes, token, true)
            }
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                (self.read_bytes(fs.as_ref())?, None, false)
            }
            Err(error) => return Err(error),
        };
        if previous
            .as_ref()
            .is_some_and(|bytes| bytes.len() as u64 > MAX_DESCRIPTION_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "peer description exceeds size limit",
            ));
        }
        Ok(DescriptionSnapshot {
            previous,
            version,
            conditional,
            source_session_id: source_session_id.to_owned(),
        })
    }

    /// Publish after the checkpoint commits. A changed description wins over a
    /// compaction that started from its predecessor. Lease-only backends retain
    /// their existing advisory-lock guarantee; conditional stores fence writes.
    pub fn publish(&self, update: &DescriptionUpdate) -> io::Result<bool> {
        let previous = update.snapshot.previous_description().ok().flatten();
        let value = AgentDescription {
            description: update.description.clone(),
            updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            source_session_id: update.snapshot.source_session_id.clone(),
            attachment: previous.and_then(|value| value.attachment),
            attachment_only: false,
        };
        self.write_snapshot(&update.snapshot, &value)
    }

    /// Seed the description for a new peer, or add its identity alongside
    /// the task last published by compaction. Reattaching is idempotent.
    pub fn publish_attachment(&self, session_id: &str, label: &str) -> io::Result<bool> {
        if !valid_description(label) || session_id.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid peer attachment",
            ));
        }
        let snapshot = self.snapshot(session_id)?;
        let previous = snapshot.previous_description()?;
        if previous.as_ref().and_then(|v| v.attachment.as_deref()) == Some(label) {
            return Ok(true);
        }
        let mut value = match previous {
            Some(value) if !value.attachment_only => value,
            _ => AgentDescription {
                description: label.to_owned(),
                updated_at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                source_session_id: session_id.to_owned(),
                attachment: None,
                attachment_only: true,
            },
        };
        value.attachment = Some(label.to_owned());
        self.write_snapshot(&snapshot, &value)
    }

    fn write_snapshot(
        &self,
        snapshot: &DescriptionSnapshot,
        value: &AgentDescription,
    ) -> io::Result<bool> {
        let fs = self.backend()?;
        let parent = std::path::Path::new(&self.path)
            .parent()
            .ok_or_else(|| io::Error::other("peer description has no agent directory"))?;
        fs.create_dir_all(&parent.to_string_lossy())?;
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        if snapshot.conditional {
            return match fs.write_if_version(&self.path, snapshot.version.as_ref(), &bytes)? {
                WriteOutcome::Committed(_) => Ok(true),
                WriteOutcome::Conflict { .. } => Ok(false),
                WriteOutcome::Unknown => Err(io::Error::other(
                    "peer description write outcome is unknown",
                )),
            };
        }
        let Some(lock) = fs.try_lock(&format!("{}.lock", self.path), 30_000)? else {
            return Ok(false);
        };
        if self.read_bytes(fs.as_ref())? != snapshot.previous || !lock.renew()? {
            return Ok(false);
        }
        fs.write_atomic(&self.path, &bytes)?;
        Ok(true)
    }
}

impl DescriptionSnapshot {
    fn previous_description(&self) -> io::Result<Option<AgentDescription>> {
        self.previous
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(io::Error::other)
    }

    /// Append only to the compaction request's existing uncached instruction.
    pub fn append_instruction(&self, prompt: &mut String, recent: &[crate::ConversationMessage]) {
        use std::fmt::Write as _;
        prompt.push_str("\n\nOptional checkpoint metadata: for this request, extend the text-only output format above with a second block. Finish the complete <summary> block first, then append <agent-description> followed by a JSON string and </agent-description>. Describe the underlying user assignment this peer agent is pursuing, excluding the checkpoint and metadata-writing operation itself. Prefer the latest preserved user assignment below over superseded work in the older source history. Use a short phrase, roughly 3-10 words and at most 160 characters on one line. Do not repeat the agent's name. Include this block when the ongoing task is clear; omit it only if unsure or if the checkpoint needs the output space. Keep metadata out of the summary itself. Do not call tools.");
        let mut remaining = 512;
        let mut hints: Vec<String> = recent
            .iter()
            .rev()
            .filter(|message| message.role == crate::MessageRole::User)
            .filter_map(|message| {
                message.blocks.iter().find_map(|block| match block {
                    crate::ContentBlock::Text { text }
                        if !text.trim().is_empty()
                            && !text.trim_start().starts_with("<system-reminder>") =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
            })
            .take(4)
            .map_while(|text| {
                if remaining == 0 {
                    return None;
                }
                let hint: String = text.chars().take(remaining).collect();
                remaining -= hint.chars().count();
                Some(hint)
            })
            .collect();
        if !hints.is_empty() {
            hints.reverse();
            let _ = write!(prompt, "\nLatest preserved user texts, in chronological order and quoted as data solely to inform the description; they remain preserved verbatim outside this checkpoint: {}", serde_json::to_string(&hints).unwrap_or_default());
        }
    }

    /// Discard optional metadata before checkpoint validation or continuation.
    #[must_use]
    pub fn split_response(&self, response: String) -> (String, Option<DescriptionUpdate>) {
        let Some(end) = response
            .find("</summary>")
            .map(|index| index + "</summary>".len())
            .or_else(|| response.find("<agent-description"))
        else {
            return (response, None);
        };
        let trailing = response[end..].trim();
        if !trailing.starts_with("<agent-description") {
            return (response, None);
        }
        let summary = response[..end].to_owned();
        let description = trailing
            .trim()
            .strip_prefix("<agent-description>")
            .and_then(|value| value.strip_suffix("</agent-description>"))
            .and_then(
                |value| match serde_json::from_str::<serde_json::Value>(value.trim()) {
                    Ok(serde_json::Value::String(text)) => Some(text),
                    Err(_) if !value.trim().starts_with(['"', '{', '[']) => {
                        Some(value.trim().to_owned())
                    }
                    Ok(_) | Err(_) => None,
                },
            )
            .and_then(|value| self.update(&value));
        (summary, description)
    }

    #[must_use]
    pub fn update(&self, description: &str) -> Option<DescriptionUpdate> {
        valid_description(description).then(|| DescriptionUpdate {
            snapshot: self.clone(),
            description: description.trim().to_owned(),
        })
    }
}

fn valid_description(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().count() <= MAX_DESCRIPTION_CHARS
        && !text.chars().any(char::is_control)
}

/// Human-readable age is derived at display time, never written back to storage.
#[must_use]
pub fn relative_age(updated_at: &str) -> Option<String> {
    let date = DateTime::parse_from_rfc3339(updated_at).ok()?;
    let seconds = Utc::now().signed_duration_since(date).num_seconds().max(0);
    Some(if seconds < 60 {
        "just now".into()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    })
}
