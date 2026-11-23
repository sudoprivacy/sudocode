//! Sends whose delivery could not be confirmed, kept on this host until a
//! retry settles them.
//!
//! A framed append that times out can still commit, so its sender cannot tell
//! whether the peer has the message, and during the outage it cannot look. A
//! send queued here is retried with its original `send_id` once the link is
//! back; a receiver drops a send it has already delivered, so retrying one that
//! did land costs a duplicate frame and nothing more. Host-local files rather
//! than the mailbox backend, because the backend is what is unreachable.

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::agent_mailbox::MailboxEnvelope;

/// Appends after which a queued send is abandoned, the original included.
pub const MAX_ATTEMPTS: u32 = 50;
/// Age after which a queued send is abandoned.
pub const MAX_AGE_SECS: u64 = 24 * 60 * 60;
/// Sends one agent may have queued at once.
pub const MAX_QUEUED: usize = 256;

/// One send waiting for a retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedSend {
    pub envelope: MailboxEnvelope,
    /// Appends made for this send so far, the original included.
    pub attempts: u32,
    /// When the original append was made, in Unix seconds.
    pub first_attempt_secs: u64,
}

impl QueuedSend {
    #[must_use]
    pub fn send_id(&self) -> &str {
        self.envelope.send_id.as_deref().unwrap_or_default()
    }

    /// Whether this send has had all the attempts and time it gets.
    #[must_use]
    pub fn past_limits(&self, now_secs: u64) -> bool {
        self.attempts >= MAX_ATTEMPTS
            || now_secs.saturating_sub(self.first_attempt_secs) >= MAX_AGE_SECS
    }
}

/// How a queued send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendOutcome {
    Delivered,
    Abandoned,
}

/// A queued send that has ended, kept until it is reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettledSend {
    pub to: String,
    pub send_id: String,
    pub outcome: SendOutcome,
    pub attempts: u32,
}

impl std::fmt::Display for SettledSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            to,
            send_id,
            attempts,
            ..
        } = self;
        match self.outcome {
            // Found before any retry: the original append committed late.
            SendOutcome::Delivered if *attempts == 1 => write!(
                f,
                "Earlier queued send {send_id} to {to}: delivered, its original attempt landed"
            ),
            SendOutcome::Delivered => write!(
                f,
                "Earlier queued send {send_id} to {to}: delivered after retry ({attempts} attempts)"
            ),
            SendOutcome::Abandoned => write!(
                f,
                "Earlier queued send {send_id} to {to}: abandoned after {attempts} attempts — \
                 resend if still needed"
            ),
        }
    }
}

/// One agent's queued sends, `<dir>/<send_id>.json`, and the ones that have
/// ended and are not reported yet, `<dir>/settled/<send_id>.json`.
#[derive(Debug, Clone)]
pub struct SendOutbox {
    dir: PathBuf,
}

impl SendOutbox {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `agent`'s outbox under a config home, beside the same-machine pair root
    /// ([`crate::mailbox::local_pair_root_in`]). `namespace` names where the
    /// agent's conversations live, so a workspace mailbox and a node mailbox
    /// that share an agent name never retry each other's sends.
    #[must_use]
    pub fn in_config_home(config_home: &Path, namespace: &str, agent: &str) -> Self {
        use sha2::{Digest, Sha256};
        use std::fmt::Write as _;
        let digest = Sha256::digest(namespace.as_bytes());
        let key = digest[..8].iter().fold(String::new(), |mut key, byte| {
            let _ = write!(key, "{byte:02x}");
            key
        });
        Self::new(config_home.join("a2a-outbox").join(key).join(agent))
    }

    /// Queue `envelope` for retry. A send already queued keeps its record.
    ///
    /// # Errors
    /// The envelope's `send_id` cannot name a file, the outbox already holds
    /// [`MAX_QUEUED`] sends, or the record cannot be written.
    pub fn queue(&self, envelope: &MailboxEnvelope) -> Result<(), String> {
        let send_id = envelope.send_id.as_deref().unwrap_or_default();
        // The id names a host file, and a caller may have chosen it.
        if send_id.is_empty()
            || send_id.len() > 128
            || !send_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(format!("send id {send_id:?} cannot name a queued send"));
        }
        let path = self.entry_path(send_id);
        if path.exists() {
            return Ok(());
        }
        let queued = json_files(&self.dir)?.len();
        if queued >= MAX_QUEUED {
            return Err(format!(
                "the outbox at {} already holds {queued} unsent messages, its limit",
                self.dir.display()
            ));
        }
        write_json(
            &path,
            &QueuedSend {
                envelope: envelope.clone(),
                attempts: 1,
                first_attempt_secs: crate::agent_mailbox::now_secs(),
            },
        )
    }

    /// Every queued send, oldest first. A missing outbox is an empty one, so
    /// asking costs one directory read when nothing is queued.
    ///
    /// # Errors
    /// The outbox exists but cannot be listed.
    pub fn list(&self) -> Result<Vec<QueuedSend>, String> {
        let mut queued = Vec::new();
        for path in json_files(&self.dir)? {
            match read_json::<QueuedSend>(&path) {
                Ok(entry) => queued.push(entry),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "queued send unreadable; skipped");
                }
            }
        }
        queued.sort_by(|a, b| {
            (a.first_attempt_secs, a.envelope.timestamp, a.send_id()).cmp(&(
                b.first_attempt_secs,
                b.envelope.timestamp,
                b.send_id(),
            ))
        });
        Ok(queued)
    }

    /// Record one more failed attempt for `entry`, abandoning it once it is
    /// past its limits. Returns whether it was abandoned.
    ///
    /// # Errors
    /// The record cannot be written.
    pub fn bump(&self, entry: &QueuedSend, now_secs: u64) -> Result<bool, String> {
        let bumped = QueuedSend {
            attempts: entry.attempts.saturating_add(1),
            ..entry.clone()
        };
        if bumped.past_limits(now_secs) {
            self.settle(&bumped, SendOutcome::Abandoned)?;
            return Ok(true);
        }
        write_json(&self.entry_path(bumped.send_id()), &bumped)?;
        Ok(false)
    }

    /// End `entry` with `outcome` after its `attempts`: record the outcome for
    /// reporting, then take the send off the queue.
    ///
    /// # Errors
    /// Either file operation fails.
    pub fn settle(&self, entry: &QueuedSend, outcome: SendOutcome) -> Result<(), String> {
        let settled = SettledSend {
            to: entry.envelope.to.clone(),
            send_id: entry.send_id().to_string(),
            outcome,
            attempts: entry.attempts,
        };
        write_json(
            &self.settled_dir().join(format!("{}.json", settled.send_id)),
            &settled,
        )?;
        let path = self.entry_path(entry.send_id());
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove queued send {}: {error}", path.display())),
        }
    }

    /// The settled sends not reported yet, each returned once.
    #[must_use]
    pub fn take_settled(&self) -> Vec<SettledSend> {
        let paths = json_files(&self.settled_dir()).unwrap_or_else(|error| {
            tracing::warn!(%error, "settled sends unreadable");
            Vec::new()
        });
        let mut settled = Vec::new();
        for path in paths {
            match read_json::<SettledSend>(&path) {
                Ok(outcome) => settled.push(outcome),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "settled send unreadable");
                }
            }
            if let Err(error) = std::fs::remove_file(&path) {
                tracing::warn!(path = %path.display(), %error, "settled send not cleared");
            }
        }
        settled.sort_by(|a, b| (&a.to, &a.send_id).cmp(&(&b.to, &b.send_id)));
        settled
    }

    fn entry_path(&self, send_id: &str) -> PathBuf {
        self.dir.join(format!("{send_id}.json"))
    }

    fn settled_dir(&self) -> PathBuf {
        self.dir.join("settled")
    }
}

/// The `.json` files directly in `dir`; none when it does not exist.
fn json_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("list {}: {error}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("list {}: {error}", dir.display()))?;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_file())
            && path.extension().is_some_and(|ext| ext == "json")
        {
            files.push(path);
        }
    }
    Ok(files)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

/// Written beside its final name and renamed into place, so a crash leaves the
/// old record or the new one, never half of one.
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let failed = |error: &dyn std::fmt::Display| format!("write {}: {error}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| failed(&error))?;
    }
    let bytes = serde_json::to_vec(value).map_err(|error| failed(&error))?;
    let staged = path.with_extension("json.tmp");
    std::fs::write(&staged, bytes)
        .and_then(|()| std::fs::rename(&staged, path))
        .map_err(|error| failed(&error))
}
