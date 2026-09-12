//! Unified mailbox abstraction over [`crate::fs_backend::FsBackend`].
//!
//! Provides transport-agnostic send/receive for inter-agent messaging.
//! The `FsBackend` determines framing:
//! - DT_STREAM backends (kernel, nexus-vfs): each `append` is one framed
//!   write, each `tail_read` returns one frame.
//! - File backends (StdFs): JSONL — newline-delimited JSON per `append`,
//!   `tail_read` returns raw bytes split by newline on read.
//!
//! Callers construct a `Mailbox` with a backend and an [`InboxConvention`]
//! that maps agent names to inbox paths. Everything above this layer is
//! backend-agnostic.

use std::sync::Arc;

use crate::agent_mailbox::MailboxEnvelope;
use crate::fs_backend::FsBackend;

/// How agent names map to inbox paths.
#[derive(Debug, Clone)]
pub enum InboxConvention {
    /// Local JSONL: `{root}/.sudocode-inbox/{name}.jsonl`.
    LocalJsonl { root: String },
    /// Nexus A2A DT_STREAM: `/agents/{name}/chat-with-me`.
    NexusA2a,
}

impl InboxConvention {
    /// Resolve the inbox path for an agent name.
    #[must_use]
    pub fn inbox_path(&self, name: &str) -> String {
        match self {
            InboxConvention::LocalJsonl { root } => {
                format!("{root}/.sudocode-inbox/{name}.jsonl")
            }
            InboxConvention::NexusA2a => {
                format!("/agents/{name}/chat-with-me")
            }
        }
    }
}

/// Transport-agnostic agent mailbox.
///
/// Wraps an [`FsBackend`] + path convention, providing send/receive that
/// works identically whether the underlying transport is a local JSONL
/// file or a nexus DT_STREAM.
pub struct Mailbox {
    backend: Arc<dyn FsBackend>,
    self_id: String,
    convention: InboxConvention,
}

impl Mailbox {
    pub fn new(backend: Arc<dyn FsBackend>, self_id: String, convention: InboxConvention) -> Self {
        Self {
            backend,
            self_id,
            convention,
        }
    }

    #[must_use]
    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    #[must_use]
    pub fn inbox_path(&self, agent: &str) -> String {
        self.convention.inbox_path(agent)
    }

    #[must_use]
    pub fn own_inbox_path(&self) -> String {
        self.convention.inbox_path(&self.self_id)
    }

    /// Provision this agent's inbox (idempotent). For DT_STREAM backends
    /// this creates the stream; for file backends this is a no-op (the
    /// file is created lazily on first write).
    pub fn ensure_inbox(&self) -> Result<(), String> {
        let path = self.own_inbox_path();
        let is_stream = self.backend.is_append_stream(&path).unwrap_or(false);
        if is_stream {
            return Ok(());
        }
        self.backend
            .create_append_log(&path, crate::agent_mailbox::DEFAULT_STREAM_CAPACITY)
            .map_err(|e| format!("ensure inbox {path}: {e}"))
    }

    /// Send a message to a recipient's inbox.
    pub fn send(&self, mut envelope: MailboxEnvelope) -> Result<(), String> {
        if envelope.from.is_empty() {
            envelope.from = self.self_id.clone();
        }
        let path = self.convention.inbox_path(&envelope.to);

        let is_stream = self.backend.is_append_stream(&path).unwrap_or(false);
        let data = if is_stream {
            envelope.to_bytes()
        } else {
            if envelope.timestamp == 0 {
                envelope.timestamp = now_secs();
            }
            let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or(".");
            let _ = std::fs::create_dir_all(parent);
            let mut line = serde_json::to_vec(&envelope).unwrap_or_default();
            line.push(b'\n');
            line
        };
        self.backend
            .append(&path, &data)
            .map_err(|e| format!("mailbox send to {path}: {e}"))
    }

    /// Read new messages from own inbox starting at `cursor`.
    ///
    /// Returns `(messages, next_cursor)`. The caller persists `next_cursor`
    /// across calls. `block_ms > 0` makes the read block until new data
    /// arrives or the timeout elapses.
    ///
    /// Messages from `self_id` are filtered out (no echo).
    pub fn poll(&self, cursor: u64, block_ms: u64) -> Result<(Vec<MailboxEnvelope>, u64), String> {
        let path = self.own_inbox_path();
        let is_stream = self.backend.is_append_stream(&path).unwrap_or(false);
        if is_stream {
            self.poll_stream(&path, cursor, block_ms)
        } else {
            self.poll_jsonl(&path, cursor, block_ms)
        }
    }

    /// DT_STREAM: one frame per `tail_read`, drain burst non-blocking
    /// after the first blocking read (same pattern as `nexus_mailbox::poll_new`).
    fn poll_stream(
        &self,
        path: &str,
        mut cursor: u64,
        block_ms: u64,
    ) -> Result<(Vec<MailboxEnvelope>, u64), String> {
        let mut out = Vec::new();
        let mut first = true;
        loop {
            let blocking = first && block_ms > 0;
            first = false;
            let (data, next, eof) = self
                .backend
                .tail_read(path, cursor, if blocking { block_ms } else { 0 })
                .map_err(|e| format!("mailbox poll {path}@{cursor}: {e}"))?;
            if eof {
                break;
            }
            if let Some(env) = MailboxEnvelope::from_bytes(&data) {
                if !env.from.is_empty() && env.from != self.self_id && !env.body.is_empty() {
                    out.push(env);
                }
            }
            if next <= cursor {
                break;
            }
            cursor = next;
        }
        Ok((out, cursor))
    }

    /// JSONL: `tail_read` returns raw bytes, split by newline.
    fn poll_jsonl(
        &self,
        path: &str,
        cursor: u64,
        block_ms: u64,
    ) -> Result<(Vec<MailboxEnvelope>, u64), String> {
        let (data, next, _eof) = self
            .backend
            .tail_read(path, cursor, block_ms)
            .map_err(|e| format!("mailbox poll {path}@{cursor}: {e}"))?;
        if data.is_empty() {
            return Ok((vec![], next));
        }
        let text = String::from_utf8_lossy(&data);
        let out: Vec<MailboxEnvelope> = text
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return None;
                }
                let env = MailboxEnvelope::from_bytes(trimmed.as_bytes())?;
                if env.from.is_empty() || env.from == self.self_id || env.body.is_empty() {
                    return None;
                }
                Some(env)
            })
            .collect();
        Ok((out, next))
    }

    /// Read ALL messages for a recipient (batch read). Primarily for the
    /// coordinator's multi-turn loop which reads the entire inbox between
    /// turns.
    pub fn read_all(&self, recipient: &str) -> Result<Vec<MailboxEnvelope>, String> {
        let path = self.convention.inbox_path(recipient);
        let is_stream = self.backend.is_append_stream(&path).unwrap_or(false);
        if is_stream {
            let (envs, _cursor) = self.poll_stream(&path, 0, 0)?;
            Ok(envs)
        } else {
            crate::agent_mailbox::read_all_from_path(&path)
        }
    }

    /// List all recipients that have an inbox. Only meaningful for the
    /// local JSONL convention (DT_STREAM inboxes are discovered via the
    /// agent registry, not directory listing).
    pub fn list_recipients(&self) -> Result<Vec<String>, String> {
        match &self.convention {
            InboxConvention::LocalJsonl { root } => {
                crate::agent_mailbox::list_recipients(std::path::Path::new(root))
            }
            InboxConvention::NexusA2a => Ok(vec![]),
        }
    }

    /// Build a [`MailboxSender`] closure from this mailbox — the
    /// type-erased send capability handed to tool executors.
    #[must_use]
    pub fn sender(self: &Arc<Self>) -> crate::spawn_task::MailboxSender {
        let mb = Arc::clone(self);
        Arc::new(move |to: &str, body: &str| {
            let env = MailboxEnvelope {
                from: mb.self_id.clone(),
                to: to.to_string(),
                body: body.to_string(),
                summary: None,
                timestamp: 0,
                color: None,
                kind: String::new(),
                request_id: None,
            };
            mb.send(env)
        })
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const LOCAL_POLL_BLOCK_MS: u64 = 1000;

/// Spawn a background thread that polls a local JSONL inbox for
/// incoming peer messages and invokes `sink` for each one.
///
/// The thread blocks up to 1s per iteration waiting for new data,
/// then loops. File-not-found is handled gracefully (the file may
/// not exist until a sub-agent first writes to it).
pub fn spawn_local_poller(
    workspace_root: std::path::PathBuf,
    self_id: String,
    abort: crate::HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("local-inbox-poller".into())
        .spawn(move || {
            let backend: Arc<dyn crate::fs_backend::FsBackend> =
                Arc::new(crate::fs_backend::StdFsBackend);
            let mailbox = Mailbox::new(
                backend,
                self_id,
                InboxConvention::LocalJsonl {
                    root: workspace_root.to_string_lossy().into_owned(),
                },
            );
            let mut cursor = 0u64;
            while !abort.is_aborted() {
                match mailbox.poll(cursor, LOCAL_POLL_BLOCK_MS) {
                    Ok((msgs, next)) => {
                        for m in &msgs {
                            sink(m);
                        }
                        if next > cursor {
                            cursor = next;
                        }
                    }
                    Err(e) => {
                        eprintln!("[local-inbox] poll failed: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(LOCAL_POLL_BLOCK_MS));
                    }
                }
            }
        })
        .expect("spawn local-inbox-poller thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_backend::StdFsBackend;

    fn temp_workspace(label: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "mailbox-unified-{label}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path.to_string_lossy().to_string()
    }

    fn local_mailbox(root: &str, self_id: &str) -> Mailbox {
        Mailbox::new(
            Arc::new(StdFsBackend),
            self_id.to_string(),
            InboxConvention::LocalJsonl {
                root: root.to_string(),
            },
        )
    }

    #[test]
    fn send_and_poll_local_jsonl() {
        let ws = temp_workspace("send-poll");
        let mb = local_mailbox(&ws, "team-lead");

        mb.send(MailboxEnvelope {
            from: "team-lead".to_string(),
            to: "worker".to_string(),
            body: "hello worker".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();

        let worker_mb = local_mailbox(&ws, "worker");
        let (msgs, _cursor) = worker_mb.poll(0, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "team-lead");
        assert_eq!(msgs[0].body, "hello worker");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn poll_filters_self_writes() {
        let ws = temp_workspace("self-filter");
        let mb = local_mailbox(&ws, "agent-a");

        mb.send(MailboxEnvelope {
            from: "agent-a".to_string(),
            to: "agent-a".to_string(),
            body: "self-write".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();
        mb.send(MailboxEnvelope {
            from: "agent-b".to_string(),
            to: "agent-a".to_string(),
            body: "from peer".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();

        let (msgs, _) = mb.poll(0, 0).unwrap();
        assert_eq!(msgs.len(), 1, "self-writes must be filtered");
        assert_eq!(msgs[0].body, "from peer");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn read_all_returns_all_messages() {
        let ws = temp_workspace("read-all");
        let mb = local_mailbox(&ws, "coordinator");

        for i in 0..3 {
            mb.send(MailboxEnvelope {
                from: format!("agent-{i}"),
                to: "worker".to_string(),
                body: format!("msg-{i}"),
                summary: None,
                timestamp: 0,
                color: None,
                kind: String::new(),
                request_id: None,
            })
            .unwrap();
        }

        let envs = mb.read_all("worker").unwrap();
        assert_eq!(envs.len(), 3);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn list_recipients_local() {
        let ws = temp_workspace("list-recip");
        let mb = local_mailbox(&ws, "coordinator");

        mb.send(MailboxEnvelope {
            from: "coordinator".to_string(),
            to: "alpha".to_string(),
            body: "hi".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();
        mb.send(MailboxEnvelope {
            from: "coordinator".to_string(),
            to: "beta".to_string(),
            body: "hi".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();

        let names = mb.list_recipients().unwrap();
        assert_eq!(names, vec!["alpha", "beta"]);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn inbox_path_conventions() {
        let local = InboxConvention::LocalJsonl {
            root: "/workspace".to_string(),
        };
        assert_eq!(
            local.inbox_path("worker"),
            "/workspace/.sudocode-inbox/worker.jsonl"
        );

        let nexus = InboxConvention::NexusA2a;
        assert_eq!(nexus.inbox_path("win-ai"), "/agents/win-ai/chat-with-me");
    }

    #[test]
    fn spawn_local_poller_delivers_messages() {
        let ws = temp_workspace("local-poller");
        let (tx, rx) = std::sync::mpsc::channel::<MailboxEnvelope>();
        let abort = crate::HookAbortSignal::new();
        let abort_clone = abort.clone();

        let _handle = super::spawn_local_poller(
            std::path::PathBuf::from(&ws),
            "team-lead".to_string(),
            abort_clone,
            move |msg| {
                let _ = tx.send(msg.clone());
            },
        );

        // Write a message from a sub-agent to team-lead's inbox.
        let mb = local_mailbox(&ws, "sub-agent-1");
        mb.send(MailboxEnvelope {
            from: "sub-agent-1".to_string(),
            to: "team-lead".to_string(),
            body: "task complete".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();

        let msg = rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert_eq!(msg.from, "sub-agent-1");
        assert_eq!(msg.body, "task complete");

        abort.abort();
        let _ = std::fs::remove_dir_all(&ws);
    }
}
