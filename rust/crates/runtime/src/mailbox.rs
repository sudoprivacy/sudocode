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

/// The leaf of an A2A inbox path, re-exported so scode spells it the same way
/// the daemon's mailbox-stamping policy does.
///
/// The SSOT is `a2a`, in the infra crate — the daemon decides which paths it
/// stamps, so scode reading the constant rather than retyping it is what keeps
/// the two in agreement. Re-exported HERE, and used from here, so there is one
/// hop rather than each module reaching for `a2a` on its own.
pub use a2a::CHAT_WITH_ME_SUFFIX;

/// Directory an A2A inbox lives under.
///
/// scode's convention, not nexus's: nexus supplies the zone prefix, routing and
/// replication, and is deliberately agnostic about what the names under it
/// mean. So this constant belongs to scode and lives with the convention that
/// uses it.
pub const A2A_INBOX_BASE: &str = "/agents";

/// Directory a local JSONL inbox lives under, relative to the workspace root.
///
/// Owned by [`crate::agent_mailbox`] — it builds every path under this — and
/// re-exported here so the convention reads as one thing in one place.
pub use crate::agent_mailbox::LOCAL_INBOX_DIR;

/// How agent names map to inbox paths.
///
/// The one place a mailbox path shape is defined. There were two such enums —
/// this and a `Mailbox` in `spawn_task` for the co-host loop — each with its
/// own path builder for shapes that overlapped, which is how the
/// `/chat-with-me` leaf ended up spelled four different ways.
#[derive(Debug, Clone)]
pub enum InboxConvention {
    /// Local JSONL: `{root}/.sudocode-inbox/{name}.jsonl`.
    LocalJsonl { root: String },
    /// Nexus A2A DT_STREAM, one inbox per recipient:
    /// `/agents/{name}/chat-with-me`. Raft-replicated, so two agents on
    /// different nodes converse with no bridge or relay between them.
    NexusA2a,
    /// One stream both parties read AND write, each filtering out its own
    /// writes — the managed-agent `/proc/{pid}/chat-with-me` model.
    ///
    /// Node-local by construction: the path is not keyed by recipient, so
    /// there is no per-agent inbox for a reply to be replicated to. Every name
    /// resolves to the same path, which is what makes "where do I reply to
    /// this sender" answer itself.
    SharedStream { path: String },
}

impl InboxConvention {
    /// Resolve the inbox path for an agent name.
    ///
    /// One function for both directions a caller needs: pass your own name for
    /// the inbox you read, pass a sender's for the inbox you reply into. There
    /// is nothing else to a "reply path".
    #[must_use]
    #[inline]
    pub fn inbox_path(&self, name: &str) -> String {
        match self {
            InboxConvention::LocalJsonl { root } => {
                format!("{root}/{LOCAL_INBOX_DIR}/{name}.jsonl")
            }
            InboxConvention::NexusA2a => {
                format!("{A2A_INBOX_BASE}/{name}{CHAT_WITH_ME_SUFFIX}")
            }
            // Every name resolves to the one stream — so replying to a sender
            // and reading your own inbox are the same path, by design.
            InboxConvention::SharedStream { path } => path.clone(),
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

    /// Whether the backend frames appends at `path` itself (a DT_STREAM record
    /// per append) rather than leaving framing to us (a JSONL line per append).
    ///
    /// Asked per call, deliberately — do NOT cache it on the struct. For
    /// `KernelFsBackend` this is a real `sys_stat`, so the answer CHANGES the
    /// moment [`Self::ensure_inbox`] creates the stream: resolved once at
    /// construction it would be `false` forever, and a provisioned DT_STREAM
    /// inbox would be written and read as JSONL. The call is cheap — a string
    /// test for the VFS backend, a constant `false` for the file one.
    #[inline]
    fn backend_frames(&self, path: &str) -> bool {
        self.backend.is_append_stream(path).unwrap_or(false)
    }

    /// A nexus A2A mailbox for `agent` over an already-dialled client.
    ///
    /// The one place that says what a nexus A2A mailbox is made of. Pairing a
    /// backend with a convention by hand at each call site is how they end up
    /// mismatched — a `LocalJsonl` convention over a VFS backend writes lines
    /// nothing tails, and the mistake is silent.
    #[must_use]
    pub fn over_nexus(
        client: Arc<nexus_vfs_client::NexusVfsClient>,
        agent: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Self {
        Self::new(
            Arc::new(crate::fs_backend::NexusVfsFsBackend::from_arc(
                client,
                auth_token.into(),
            )),
            agent.into(),
            InboxConvention::NexusA2a,
        )
    }

    #[must_use]
    #[inline]
    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    #[must_use]
    #[inline]
    pub fn inbox_path(&self, agent: &str) -> String {
        self.convention.inbox_path(agent)
    }

    #[must_use]
    #[inline]
    pub fn own_inbox_path(&self) -> String {
        self.convention.inbox_path(&self.self_id)
    }

    /// Provision this agent's inbox (idempotent).
    ///
    /// A terminal `scode` is not a managed agent, so nothing registers an
    /// inbox on its behalf — it has to ensure its own exists before a receiver
    /// can read it. The capacity comes from the a2a SSOT
    /// ([`crate::agent_mailbox::DEFAULT_STREAM_CAPACITY`]) so an inbox created
    /// standalone is byte-identical to one a co-host created.
    ///
    /// For a DT_STREAM backend the stream already exists once the path resolves
    /// as one, so this returns early; a file backend creates the append log.
    pub fn ensure_inbox(&self) -> Result<(), String> {
        let path = self.own_inbox_path();
        let is_stream = self.backend_frames(&path);
        if is_stream {
            return Ok(());
        }
        self.backend
            .create_append_log(&path, crate::agent_mailbox::DEFAULT_STREAM_CAPACITY)
            .map_err(|e| format!("ensure inbox {path}: {e}"))
    }

    /// Send a message to a recipient's inbox.
    ///
    /// The `from` we write is ADVISORY. Under auth-on the daemon's
    /// `MailboxStampingHook` overwrites it with the authenticated caller's
    /// identity, so it cannot be forged; under auth-off it is used as-is.
    /// Nothing above this layer should treat it as proof of origin.
    ///
    /// A DT_STREAM backend frames the append itself, so the envelope goes
    /// through the backend. JSONL has no framing of its own — a message is a
    /// line — and that branch goes through
    /// [`crate::agent_mailbox::append_envelope`], which is the one writer of
    /// this format.
    ///
    /// It used to be two: this method built the line itself (timestamp,
    /// recipient, serialize, newline, create the directory) while
    /// `coordinator_notification` called `append_envelope`, so the same format
    /// had two implementations that could drift apart. One of them also holds a
    /// write lock the other did not — `write_all` may in principle issue
    /// several `write` calls for one buffer, and the reader skips a line it
    /// cannot parse, so an interleave would be a silently dropped message. I
    /// could NOT reproduce that: for a regular file an `O_APPEND` write is
    /// effectively atomic, and six concurrent senders pushing 256 KiB lines
    /// interleaved nothing on Windows. Treat the lock as defensive rather than
    /// load-bearing; the reason for going through one writer is that the format
    /// has one definition.
    pub fn send(&self, mut envelope: MailboxEnvelope) -> Result<(), String> {
        if envelope.from.is_empty() {
            envelope.from = self.self_id.clone();
        }
        let path = self.convention.inbox_path(&envelope.to);

        if self.backend_frames(&path) {
            return self
                .backend
                .append(&path, &envelope.to_bytes())
                .map_err(|e| format!("mailbox send to {path}: {e}"));
        }

        match &self.convention {
            InboxConvention::LocalJsonl { root } => {
                let recipient = envelope.to.clone();
                crate::agent_mailbox::append_envelope(
                    std::path::Path::new(root),
                    &recipient,
                    envelope,
                )
                .map(|_| ())
            }
            // A non-stream path under a stream convention means the inbox was
            // never provisioned. Say so rather than writing a line into a
            // location nothing tails.
            InboxConvention::NexusA2a | InboxConvention::SharedStream { .. } => Err(format!(
                "mailbox send to {path}: not an append stream — inbox not provisioned"
            )),
        }
    }

    /// Read new messages from own inbox starting at `cursor`.
    ///
    /// Returns `(messages, next_cursor)`. The caller persists `next_cursor`
    /// across calls. `block_ms == 0` is a pure non-blocking drain — a
    /// seek-to-tail or a one-shot collect. `block_ms > 0` makes the FIRST read
    /// a blocking tail read and then drains whatever else is buffered without
    /// blocking, so a burst surfaces in one call.
    ///
    /// ## Why a blocking read rather than a watch
    ///
    /// On a DT_STREAM backend the server parks up to `block_ms` on the
    /// stream's per-path condvar and wakes sub-millisecond on the next write —
    /// node-local or a peer's replicated append — so an idle receiver costs one
    /// parked RPC instead of a `sleep` loop.
    ///
    /// Both a blocking read and `sys_watch` do wake for the WAL mailbox: the
    /// apply observer signals the file-watch AND the stream condvar. The
    /// blocking read wins because it is ONE round trip that returns the frame
    /// AT the cursor, where `sys_watch` reports only "something changed" and
    /// still needs a follow-up read — two round trips and no cursor precision.
    /// A blocking read is the cursor-aware tail primitive this mailbox is built
    /// on; `sys_watch` is the generic inotify-style path-change notifier.
    ///
    /// `StdFsBackend` has no such primitive and falls back to a bounded size
    /// poll, which is the one place this waits by polling.
    ///
    /// Skips our OWN writes (`from == self_id`) so a shared read/write stream
    /// never echoes back to us, and skips senderless or empty-body frames.
    pub fn poll(&self, cursor: u64, block_ms: u64) -> Result<(Vec<MailboxEnvelope>, u64), String> {
        let path = self.own_inbox_path();
        let is_stream = self.backend_frames(&path);
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
        let is_stream = self.backend_frames(&path);
        if is_stream {
            let (envs, _cursor) = self.poll_stream(&path, 0, 0)?;
            Ok(envs)
        } else {
            crate::agent_mailbox::read_all_from_path(&path)
        }
    }

    /// List the recipients that have an inbox under this convention.
    ///
    /// Only the local JSONL convention can answer from the paths alone, because
    /// only there is a recipient a directory entry. `NexusA2a` inboxes are
    /// discovered through the agent registry rather than by listing `/agents`,
    /// and a `SharedStream` has no per-recipient path to enumerate at all — one
    /// stream, every name resolving to it.
    ///
    /// Empty is therefore "this convention does not enumerate", not "no
    /// recipients". A caller that needs agent discovery wants the registry.
    pub fn list_recipients(&self) -> Result<Vec<String>, String> {
        match &self.convention {
            InboxConvention::LocalJsonl { root } => {
                crate::agent_mailbox::list_recipients(std::path::Path::new(root))
            }
            InboxConvention::NexusA2a | InboxConvention::SharedStream { .. } => Ok(vec![]),
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

/// A receiver's read position, persisted so it survives the process.
///
/// Not an optimisation. Two agents handing off asynchronously otherwise lose
/// every message that arrives while the receiver is between processes: the
/// sender was told it was delivered, it sits durably in the inbox, and a
/// receiver that seeks to the tail on every start never looks back at it. That
/// was observed live in the Win↔Mac duet.
///
/// The other failure is the opposite one, so a FIRST run still seeks to the
/// tail: a receiver that has never read this inbox was not party to what came
/// before, and replaying it is the #81 re-reply storm.
#[derive(Debug, Clone)]
pub struct InboxCursor {
    path: std::path::PathBuf,
}

impl InboxCursor {
    /// Keep the cursor in `path`. Sanitise `name` into a filename with
    /// [`Self::file_name`] when deriving one from an agent name.
    #[must_use]
    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    /// `<prefix><name>` with everything outside `[A-Za-z0-9_-]` folded to `_`,
    /// so an agent name is safe to use as a filename on every platform.
    #[must_use]
    pub fn file_name(prefix: &str, name: &str) -> String {
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{prefix}{safe}")
    }

    fn load(&self) -> Option<u64> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
    }

    fn save(&self, offset: u64) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.path, offset.to_string());
    }
}

/// Spawn the background inbox receiver: park on the tail, hand each new
/// envelope to `sink`, persist the cursor.
///
/// The ONE receive loop. It used to be two — this one over gRPC for A2A and a
/// second for the local JSONL inbox — and because the loop was duplicated the
/// two copies drifted: the local one kept its cursor in a local variable
/// starting at 0, so every process start replayed the whole inbox. The loop
/// does not vary by transport, only the `mailbox` handed to it does, so there
/// is nothing for a second copy to do except diverge.
///
/// Event-driven, not polling, wherever the backend can be: each iteration
/// parks inside [`Mailbox::poll`] until the backend reports new data or
/// `block_ms` elapses. A DT_STREAM backend parks on a condvar the kernel
/// signals (including from a peer's replicated append); `StdFsBackend` has no
/// such primitive and falls back to a bounded size poll, which is the one
/// place this is a poll rather than a wait.
/// `sink` returns whether the consumer has TAKEN RESPONSIBILITY for the
/// envelope — not merely that it was handed over. The cursor does not advance
/// past an envelope that was not accepted, so a consumer that blocks until it
/// has the message applies back-pressure to the receiver instead of letting
/// messages pile up in a queue the cursor has already been advanced past.
///
/// That distinction is the whole reason this is a `bool`. Saving the cursor
/// after a `sink` that only enqueues means a crash loses everything still in
/// the queue, silently, with the sender already told "delivered" — the loss
/// this cursor exists to prevent, reintroduced one layer up.
pub fn spawn_inbox_poller(
    mailbox: Arc<Mailbox>,
    cursor_store: InboxCursor,
    block_ms: u64,
    label: &'static str,
    abort: crate::HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("{label}-inbox-poller"))
        .spawn(move || {
            let mut cursor = match cursor_store.load() {
                Some(saved) => saved,
                // Never read before: seek to the tail rather than replay.
                //
                // The accepted cost is that a send landing DURING this seek is
                // positioned past and never delivered. That window exists only
                // on a receiver's first-ever run, and the alternative — start
                // at 0 — replays a backlog this receiver was never party to,
                // which is the #81 re-reply storm. A caller that must not miss
                // a first message should create the inbox before advertising
                // the agent, not widen this window.
                None => match mailbox.poll(0, 0) {
                    Ok((_history, tail)) => {
                        cursor_store.save(tail);
                        tail
                    }
                    Err(e) => {
                        eprintln!("[{label}] initial inbox seek failed: {e}");
                        0
                    }
                },
            };
            while !abort.is_aborted() {
                match mailbox.poll(cursor, block_ms) {
                    Ok((msgs, next)) => {
                        let accepted = msgs.iter().all(|m| sink(m));
                        // Persist only on real forward progress, and only when
                        // the consumer took every envelope in the batch: an
                        // idle deadline return would otherwise rewrite the same
                        // offset every iteration, and advancing past a rejected
                        // envelope drops it.
                        //
                        // A batch is all-or-nothing because the cursor is one
                        // offset — there is no way to say "past the second but
                        // not the third". Re-delivering an accepted envelope is
                        // the tolerable half of that: at-least-once, which is
                        // the right side to err on for mail.
                        if accepted && next > cursor {
                            cursor = next;
                            cursor_store.save(cursor);
                        }
                    }
                    Err(e) => {
                        eprintln!("[{label}] inbox poll failed: {e}");
                        // Avoid a hot error loop when the backend is sick; the
                        // blocking read itself paces the happy path.
                        std::thread::sleep(std::time::Duration::from_millis(block_ms.max(1)));
                    }
                }
            }
        })
        .expect("spawn inbox poller thread")
}

/// [`spawn_inbox_poller`] over the workspace's local JSONL inbox.
///
/// Owns only the two things that are local-specific: the backend/convention
/// pair, and where the cursor lives — a dotfile beside the inbox it tracks, so
/// it is scoped to the workspace and swept with it.
pub fn spawn_local_poller(
    workspace_root: std::path::PathBuf,
    self_id: String,
    abort: crate::HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + 'static,
) -> std::thread::JoinHandle<()> {
    let cursor_store = InboxCursor::at(
        crate::agent_mailbox::mailbox_dir(&workspace_root)
            .join(InboxCursor::file_name(".cursor-", &self_id)),
    );
    let mailbox = Arc::new(Mailbox::new(
        Arc::new(crate::fs_backend::StdFsBackend),
        self_id,
        InboxConvention::LocalJsonl {
            root: workspace_root.to_string_lossy().into_owned(),
        },
    ));
    spawn_inbox_poller(
        mailbox,
        cursor_store,
        LOCAL_POLL_BLOCK_MS,
        "local-inbox",
        abort,
        sink,
    )
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

    fn note(from: &str, to: &str, body: &str) -> MailboxEnvelope {
        MailboxEnvelope {
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        }
    }

    fn wait_until(label: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {label}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A message the consumer did not take must be re-delivered, not skipped.
    ///
    /// This is what makes the durable cursor mean anything above the poller. A
    /// consumer that only ENQUEUES — hands the envelope to a channel and
    /// returns — lets the cursor advance past messages still sitting in that
    /// queue, so a crash loses them silently with the sender already told
    /// "delivered". Returning `false` until it has actually taken the message
    /// keeps the cursor and the consumer in step.
    #[test]
    fn an_unaccepted_message_is_redelivered() {
        let ws = temp_workspace("reject-redeliver");
        let ws_path = std::path::PathBuf::from(&ws);
        let peer = local_mailbox(&ws, "peer");
        let cursor_file = crate::agent_mailbox::mailbox_dir(&ws_path).join(".cursor-me");

        // Refuse the first delivery of each body, accept the second.
        let attempts: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let abort = crate::HookAbortSignal::new();
        let sink_attempts = Arc::clone(&attempts);
        let poller =
            spawn_local_poller(ws_path.clone(), "me".to_string(), abort.clone(), move |m| {
                let mut log = sink_attempts.lock().unwrap();
                let first_time = !log.contains(&m.body);
                log.push(m.body.clone());
                !first_time
            });
        wait_until("the poller to record where it starts", || {
            cursor_file.exists()
        });

        peer.send(note("peer", "me", "needs-two-tries"))
            .expect("send");
        wait_until("the refused message to come back", || {
            attempts.lock().unwrap().len() >= 2
        });
        abort.abort();
        poller.join().expect("poller joins");

        let log = attempts.lock().unwrap().clone();
        assert!(
            log.len() >= 2 && log.iter().all(|b| b == "needs-two-tries"),
            "the refused envelope must be offered again, got {log:?}"
        );

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// The receiver must neither replay a backlog it was never party to nor
    /// lose what arrived while it was not running.
    ///
    /// Both halves in one test because they are the two ways to get this wrong
    /// and a fix for either one alone reintroduces the other. Replaying the
    /// backlog is the #81 re-reply storm; losing the offline message is the
    /// Win↔Mac duet handoff, where the sender was told "delivered", the
    /// envelope sat durably in the inbox, and no later reader looked back.
    ///
    /// This is the regression the duplicated receive loop caused: the local
    /// copy kept its cursor in a local variable starting at 0.
    #[test]
    fn local_poller_resumes_from_its_cursor_instead_of_replaying() {
        let ws = temp_workspace("poller-resume");
        let ws_path = std::path::PathBuf::from(&ws);
        let peer = local_mailbox(&ws, "peer");

        // A backlog that predates any receiver.
        peer.send(note("peer", "me", "backlog-1")).expect("send");
        peer.send(note("peer", "me", "backlog-2")).expect("send");

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cursor_file = crate::agent_mailbox::mailbox_dir(&ws_path).join(".cursor-me");

        // First run: never read this inbox before, so seek to the tail.
        let abort = crate::HookAbortSignal::new();
        let sink_seen = Arc::clone(&seen);
        let first =
            spawn_local_poller(ws_path.clone(), "me".to_string(), abort.clone(), move |m| {
                sink_seen.lock().unwrap().push(m.body.clone());
                true
            });
        wait_until("the first run to record its cursor", || {
            cursor_file.exists()
        });
        assert!(
            seen.lock().unwrap().is_empty(),
            "a first run must not replay the backlog, got {:?}",
            seen.lock().unwrap()
        );

        // Delivered while it is listening.
        peer.send(note("peer", "me", "live-1")).expect("send");
        wait_until("live-1", || seen.lock().unwrap().len() == 1);
        abort.abort();
        first.join().expect("first poller joins");

        // Arrives with nobody listening — must survive the gap.
        peer.send(note("peer", "me", "offline-1")).expect("send");

        let abort2 = crate::HookAbortSignal::new();
        let sink_seen = Arc::clone(&seen);
        let second = spawn_local_poller(
            ws_path.clone(),
            "me".to_string(),
            abort2.clone(),
            move |m| {
                sink_seen.lock().unwrap().push(m.body.clone());
                true
            },
        );
        wait_until("offline-1 after the restart", || {
            seen.lock().unwrap().len() == 2
        });
        abort2.abort();
        second.join().expect("second poller joins");

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["live-1".to_string(), "offline-1".to_string()],
            "exactly the two messages addressed to a running-or-restarted receiver, \
             in order, with no backlog replay"
        );

        let _ = std::fs::remove_dir_all(&ws);
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
                true
            },
        );

        // Wait for the poller to have recorded where it starts reading before
        // sending anything. A first-ever run seeks to the tail — the
        // alternative is replaying a backlog it was never party to (#81) — so a
        // send that lands DURING that seek is positioned past and never
        // delivered. This test is about a message arriving while the receiver
        // is listening, which means it has to establish "listening" first.
        wait_until("the poller to record where it starts", || {
            crate::agent_mailbox::mailbox_dir(std::path::Path::new(&ws))
                .join(".cursor-team-lead")
                .exists()
        });

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
