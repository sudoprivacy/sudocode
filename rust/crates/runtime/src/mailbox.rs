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

/// Whether `path` is a mailbox a send APPENDS to — a conversation's transcript,
/// or the node-local `chat-with-me` pipe.
///
/// Re-exported from the a2a substrate so that a backend deciding "is this
/// framed?" and a test asserting the same thing cannot each answer it
/// separately. They did: both spelled it `ends_with(CHAT_WITH_ME_SUFFIX)`, and
/// when the mailbox moved to `…/transcript` both silently said "not a stream"
/// — sending a JSONL line to the local filesystem instead of a record to the
/// daemon, and reporting success.
pub use a2a::is_mailbox_path;
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

/// The shared root under which same-machine standalone `scode` processes keep
/// their mailboxes, so two started in different folders can address each other.
///
/// The standalone analog of a Nexus zone: a stable per-machine prefix that is
/// NOT the workspace (workspace-rooted inboxes can't see across folders). Lives
/// under the config home (`~/.nexus/sudocode/local-mailbox`), the same durable,
/// cross-workspace location the config itself uses.
#[must_use]
pub fn local_pair_root() -> std::path::PathBuf {
    local_pair_root_in(&crate::config::default_config_home())
}

/// The pair root under an explicit config home. The SSOT `local_pair_root`
/// delegates to this with `default_config_home()`; tests that pin a config home
/// compute the same path without depending on process env.
#[must_use]
pub fn local_pair_root_in(config_home: &std::path::Path) -> std::path::PathBuf {
    config_home.join("local-mailbox")
}

/// This process's mailbox identity — the name peers address and the inbox the
/// receiver polls. SSOT for "who am I on the mailbox": both the `send` path and
/// the REPL receiver read this, so they cannot disagree.
///
/// Resolution: the `agentName` setting if set (a clean short name the user
/// picks for pairing), else a name derived from the workspace path. The
/// derivation folds the FULL path, not just the basename, so two different
/// projects that happen to share a basename (two `app/` directories) do not
/// collide inside the shared [`local_pair_root`].
#[must_use]
pub fn local_agent_name(configured: Option<&str>, workspace_root: &std::path::Path) -> String {
    if let Some(name) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    let basename = workspace_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "scode".to_string());
    // Disambiguate same-basename folders with a short hash of the full path.
    let full = workspace_root.to_string_lossy();
    if full.is_empty() {
        return basename;
    }
    let mut hash: u64 = 1469598103934665603; // FNV-1a offset basis
    for byte in full.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{basename}-{:06x}", hash & 0xff_ffff)
}

/// Where a pair's conversation lives.
///
/// A conversation is addressed by its PARTICIPANTS, not by a recipient: the id
/// is derived from the unordered pair ([`a2a::conversation_id`]), so both sides
/// compute the same path with no coordination and a 1:1 pair has exactly one
/// thread. That is why every method here takes two names where the old
/// per-recipient inbox took one.
///
/// `root` is the access/isolation prefix — the standalone analog of a Nexus
/// zone:
/// - `""` over nexus: daemon-absolute (the kernel prepends the real zone).
/// - a shared per-machine dir for standalone same-machine pairs
///   ([`local_pair_root`]), so two folders can converse.
/// - the workspace root for coordinator↔sub-agent, so a sub-agent belongs to
///   its parent scode and different scodes do not cross-talk.
///
/// Framing is the backend's concern, not the path's: `StdFsBackend` writes
/// newline-delimited JSON at these paths, a DT_STREAM backend writes frames.
///
/// This used to be an enum whose second variant, `SharedStream`, described the
/// co-host's single stream that both parties read and write while filtering
/// their own writes. That IS a conversation — the variant was modelling the
/// general case as a special one. It is gone. The node-local
/// `/proc/{pid}/chat-with-me` pipe it served keeps its fixed path (nexus-vfs's
/// `proc_entry` creates it and that contract did not change), but the path is
/// now held directly by the co-host mailbox instead of masquerading as a
/// naming convention.
#[derive(Debug, Clone)]
pub struct InboxConvention {
    root: String,
}

impl InboxConvention {
    /// A convention rooted at `root` (empty for the nexus daemon-absolute case).
    #[must_use]
    pub fn new(root: impl Into<String>) -> Self {
        Self { root: root.into() }
    }

    /// Root-safe join: an empty root yields a daemon-absolute path, a non-empty
    /// one yields `{root}/…` with exactly one separator — never `//…`.
    #[inline]
    fn rooted(&self, absolute: &str) -> String {
        format!("{}{absolute}", self.root.trim_end_matches('/'))
    }

    /// The directory holding one conversation — its transcript and its reader
    /// registers.
    ///
    /// Order-free for the same reason [`Self::transcript_path`] is: the id is
    /// derived from the pair, never allocated to it.
    #[must_use]
    pub fn conversation_root(&self, a: &str, b: &str) -> String {
        self.rooted(&format!(
            "{}/{}",
            a2a::CONVERSATIONS_BASE,
            a2a::conversation_id(a, b)
        ))
    }

    /// The append-only transcript `a` and `b` both append to.
    ///
    /// Order-free: `transcript_path(a, b) == transcript_path(b, a)`, which is
    /// what lets each side derive it from its own point of view and still land
    /// on one shared log.
    #[must_use]
    pub fn transcript_path(&self, a: &str, b: &str) -> String {
        self.rooted(&a2a::conversation_transcript_path(&a2a::conversation_id(
            a, b,
        )))
    }

    /// `reader`'s read-position register in the conversation between `a` and `b`.
    #[must_use]
    pub fn reader_path(&self, a: &str, b: &str, reader: &str) -> String {
        self.rooted(&a2a::conversation_reader_path(
            &a2a::conversation_id(a, b),
            reader,
        ))
    }

    /// `agent`'s chat-list entry for its conversation with `peer` — the DT_LINK
    /// that makes `readdir` on the agent's presence list its conversations.
    ///
    /// The leaf is the PEER's name, not the cid: a BLAKE3 digest is one-way, so
    /// a cid-named entry would tell a receiver that a conversation exists
    /// without telling it with whom — and it could then derive no transcript
    /// path from the listing at all.
    #[must_use]
    pub fn chat_list_path(&self, agent: &str, peer: &str) -> String {
        self.rooted(&a2a::agent_conversation_link_path(agent, peer))
    }

    /// The directory every agent's presence hangs under — the namespace a
    /// broadcast enumerates.
    #[must_use]
    pub fn agents_dir(&self) -> String {
        self.rooted(a2a::A2A_INBOX_BASE)
    }

    /// The directory holding `agent`'s chat-list links, for enumeration.
    #[must_use]
    pub fn chat_list_dir(&self, agent: &str) -> String {
        format!(
            "{}/{agent}{}",
            self.agents_dir(),
            a2a::AGENT_CONVERSATIONS_SEGMENT
        )
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
    /// Recipients whose inbox this mailbox has already created.
    ///
    /// Creation is idempotent, so this is a cost decision rather than a
    /// correctness one: it keeps the steady state at one round trip per send
    /// instead of two. Safe to remember in a way the framing decision is NOT —
    /// a stream that exists stays a stream, and an agent inbox is a durable
    /// identity nothing deletes, whereas `is_append_stream` changes its answer
    /// the moment a stream is created.
    provisioned: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl Mailbox {
    pub fn new(backend: Arc<dyn FsBackend>, self_id: String, convention: InboxConvention) -> Self {
        Self {
            backend,
            self_id,
            convention,
            provisioned: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// A mailbox addressing the daemon's own path space: `self_id`'s
    /// conversations at absolute `/conversations/...`, over whatever backend
    /// reaches that daemon.
    ///
    /// Deliberately NOT named for a caller. Both ways of running an agent land
    /// here — co-hosted in the daemon over the in-process VFS, and standalone
    /// over the gRPC backend — because they differ in their BACKEND and in
    /// nothing else. Naming it for one of them is how the other grows a second
    /// spelling of the same thing.
    ///
    /// Having one constructor is also what keeps the pairing right. Assembling
    /// a backend and a convention by hand at each call site is how the two end
    /// up mismatched, and the mistake is silent: a rooted convention over a
    /// daemon backend writes to a path nothing tails.
    #[must_use]
    pub fn daemon_absolute(backend: Arc<dyn FsBackend>, self_id: String) -> Self {
        Self::new(backend, self_id, InboxConvention::new(String::new()))
    }

    /// A mailbox rooted under a host directory: `self_id`'s conversations at
    /// `{root}/conversations/...` over the host filesystem.
    ///
    /// The counterpart to [`Self::daemon_absolute`] for a `scode` running with
    /// no daemon to reach. Same conversations, same addressing, same receiver —
    /// the root is the only difference, and it is the one thing two call sites
    /// must not each decide for themselves.
    #[must_use]
    pub fn workspace_local(root: &std::path::Path, self_id: String) -> Self {
        Self::new(
            Arc::new(crate::fs_backend::StdFsBackend),
            self_id,
            InboxConvention::new(root.to_string_lossy().into_owned()),
        )
    }

    /// Create the recipient's inbox stream if this mailbox has not already.
    ///
    /// A send to an agent that has never run has to work — an inbox is durable
    /// precisely so a message can wait for its reader — and an agent that has
    /// never run has no stream yet. Nobody else can create it either: the
    /// recipient is not here to provision its own, and the sender is the only
    /// party that knows the message exists.
    ///
    /// Skipping this was silent, destructive data loss. `is_append_stream` is a
    /// test of the PATH's shape for the VFS backend — every `…/chat-with-me`
    /// answers yes — so an append to an unprovisioned inbox did not fail. It
    /// created a plain entry at the stream's path, returned success to the
    /// sender, and left a path that can never become a stream again:
    ///
    /// ```text
    /// send    -> Ok                        the sender is told it was delivered
    /// read    -> StreamNotFound            the recipient can never see it
    /// ensure  -> entry_type immutable      and can never repair its own inbox
    /// ```
    fn ensure_provisioned(&self, recipient: &str) -> Result<(), String> {
        {
            let seen = self
                .provisioned
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if seen.contains(recipient) {
                return Ok(());
            }
        }
        self.ensure_conversation(recipient)?;
        self.provisioned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(recipient.to_string());
        Ok(())
    }

    /// Whether the backend frames appends at `path` itself (a DT_STREAM record
    /// per append) rather than leaving framing to us (a JSONL line per append).
    ///
    /// Asked per call, deliberately — do NOT cache it on the struct. For
    /// `KernelFsBackend` this is a real `sys_stat`, so the answer CHANGES the
    /// moment [`Self::ensure_conversation`] creates the stream: resolved once at
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
            InboxConvention::new(String::new()),
        )
    }

    #[must_use]
    #[inline]
    pub fn self_id(&self) -> &str {
        &self.self_id
    }

    /// The transcript this mailbox and `peer` share.
    ///
    /// There is no "my inbox" any more, which is the point: a conversation is
    /// addressed by its PAIR, so the same call serves both directions — what I
    /// read from `peer` and what I write to `peer` are one path. The old
    /// `inbox_path(name)` / `own_inbox_path()` split existed only because a
    /// per-recipient inbox had two different answers for those two questions.
    #[must_use]
    #[inline]
    pub fn transcript_path(&self, peer: &str) -> String {
        self.convention.transcript_path(&self.self_id, peer)
    }

    /// Provision this agent's inbox (idempotent).
    ///
    /// A terminal `scode` is not a managed agent, so nothing registers an
    /// inbox on its behalf — it has to ensure its own exists before a receiver
    /// can read it. The capacity comes from the a2a SSOT
    /// ([`crate::agent_mailbox::DEFAULT_STREAM_CAPACITY`]) so an inbox created
    /// standalone is byte-identical to one a co-host created.
    ///
    /// Unconditional, because every backend's `create_append_log` is already
    /// idempotent — the file one writes only when the path is absent, the
    /// kernel one returns early on an existing entry, and the VFS one is
    /// `ensure_stream`.
    ///
    /// It used to skip the call when [`Self::backend_frames`] said the path was
    /// a stream, which read as "already provisioned" and was not. For the VFS
    /// backend that predicate is a test of the path's SHAPE — every
    /// `…/chat-with-me` answers yes — so the check was always true and the
    /// stream was never created. A standalone agent's inbox therefore did not
    /// exist, and its receiver got `StreamNotFound` on every poll. Unit tests
    /// could not see it: the shape is right, the code runs, and only a real
    /// daemon has an opinion about whether the stream is there.
    /// Make this agent discoverable before it has any conversations.
    ///
    /// Creates the agent's chat-list directory and nothing else. Without it a
    /// receiver leaves NO trace until somebody writes to it: it cannot be
    /// listed, an operator cannot see that it is running, and a sender has no
    /// way to tell "this agent is up but idle" from "this name is a typo". The
    /// per-recipient inbox this replaced gave that for free, because the inbox
    /// itself was the presence; a conversation belongs to a pair, so presence
    /// has to be stated separately.
    ///
    /// Idempotent, and deliberately NOT a precondition for delivery — a send
    /// provisions what it needs, so an agent that has never run still receives.
    ///
    /// # Errors
    /// Returns an error when the directory cannot be created.
    pub fn ensure_presence(&self) -> Result<(), String> {
        let dir = self.convention.chat_list_dir(&self.self_id);
        self.backend
            .create_dir_all(&dir)
            .map_err(|e| format!("announce {} at {dir}: {e}", self.self_id))
    }

    /// Index the conversation under BOTH agents, then create the transcript.
    ///
    /// Both directions, because whichever side sends first provisions, and the
    /// side that has to DISCOVER the conversation is the other one: a receiver
    /// finds what to tail by listing its own chat list, so an entry filed only
    /// under the sender leaves the recipient deaf while every send reports
    /// success.
    ///
    /// The transcript is created LAST so its presence is a sound completion
    /// sentinel — a conversation half-built by an interrupted send is finished
    /// by the next one rather than being mistaken for done.
    pub fn ensure_conversation(&self, peer: &str) -> Result<(), String> {
        // A conversation is addressed by its pair, so a nameless side has no
        // conversation to be in. Refused rather than tolerated: an empty name
        // still produces a cid and still composes a path, just a degenerate one
        // (`…/conversations/`, no leaf), so the failure would otherwise surface
        // far from its cause as an unwritable directory.
        if self.self_id.is_empty() || peer.is_empty() {
            return Err(format!(
                "a conversation needs two names, got self={:?} peer={peer:?}",
                self.self_id
            ));
        }
        let root = self.convention.conversation_root(&self.self_id, peer);
        for (owner, other) in [(self.self_id.as_str(), peer), (peer, self.self_id.as_str())] {
            let alias = self.convention.chat_list_path(owner, other);
            self.backend
                .link(&alias, &root)
                .map_err(|e| format!("index conversation for {owner} at {alias}: {e}"))?;
        }
        let path = self.transcript_path(peer);
        self.backend
            .create_append_log(&path, crate::agent_mailbox::DEFAULT_STREAM_CAPACITY)
            .map_err(|e| format!("ensure conversation with {peer} at {path}: {e}"))
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
        // Stamp here, above the convention split, because a receiver needs it on
        // BOTH paths and only the JSONL branch used to provide it: the framed
        // branch below serialises the envelope as-is, so every message that ever
        // crossed a DT_STREAM arrived with `timestamp: 0`. Delivery is
        // at-least-once (see `spawn_inbox_poller`), and a re-delivered frame is
        // byte-identical to one the receiving model has already answered — the
        // send time is what distinguishes "these same bytes again" from "my peer
        // said that a second time". Guarded on 0 so a caller that supplies its
        // own time (a relay preserving the original) keeps it, which is also why
        // `append_envelope_to_path`'s identical guard stays a no-op after this.
        if envelope.timestamp == 0 {
            envelope.timestamp = crate::agent_mailbox::now_secs();
        }
        let path = self.transcript_path(&envelope.to);

        // Provision on BOTH branches. It used to happen only on the framed one,
        // so over a host FS the conversation was never indexed: the recipient
        // had nothing to discover and heard nothing, while every send reported
        // success.
        self.ensure_provisioned(&envelope.to)?;

        if self.backend_frames(&path) {
            return self
                .backend
                .append(&path, &envelope.to_bytes())
                .map_err(|e| format!("mailbox send to {path}: {e}"));
        }

        // Non-framed backend (StdFs): a message is one JSONL line at the
        // conversation's transcript path, written through the one
        // envelope-line writer so the format has a single definition.
        //
        // No second arm any more. There used to be one for `SharedStream`,
        // which returned "not an append stream — inbox not provisioned" — but
        // that variant was describing a conversation (both parties reading and
        // writing one log) as if it were a special case, and with conventions
        // collapsed there is exactly one shape to write.
        crate::agent_mailbox::append_envelope_to_path(&path, envelope).map(|_| ())
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
    pub fn poll_conversation(
        &self,
        peer: &str,
        cursor: u64,
        block_ms: u64,
    ) -> Result<(Vec<MailboxEnvelope>, u64), String> {
        let path = self.transcript_path(peer);
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

    /// Read the whole conversation with `peer` (batch read). Primarily for the
    /// coordinator's multi-turn loop, which drains between turns.
    pub fn read_conversation(&self, peer: &str) -> Result<Vec<MailboxEnvelope>, String> {
        let path = self.transcript_path(peer);
        let is_stream = self.backend_frames(&path);
        if is_stream {
            let (envs, _cursor) = self.poll_stream(&path, 0, 0)?;
            Ok(envs)
        } else {
            crate::agent_mailbox::read_all_from_path(&path)
        }
    }

    /// Every agent with a presence in this namespace — who a broadcast reaches.
    ///
    /// # Errors
    ///
    /// An `Err` when the namespace cannot be enumerated, rather than an empty
    /// list. The difference matters to the one caller that needs this: a
    /// broadcast over "no recipients" reports success having delivered nothing,
    /// which is the silent kind of failure. Saying so at the source means no
    /// caller has to remember to ask first.
    pub fn list_recipients(&self) -> Result<Vec<String>, String> {
        let dir = self.convention.agents_dir();
        let entries = self
            .backend
            .readdir(&dir)
            .map_err(|e| format!("list recipients at {dir}: {e}"))?;
        let mut names: Vec<String> = entries.into_iter().map(|e| e.name).collect();
        names.sort();
        Ok(names)
    }

    /// The peers this agent has a conversation with — its chat list.
    ///
    /// Deliberately NOT the same question as [`Self::list_recipients`], and the
    /// two are not interchangeable. This one answers "who am I talking to",
    /// which is what the receiver tails; that one answers "who is there", which
    /// is what a broadcast addresses. Broadcasting to this list would silently
    /// skip every agent not yet spoken to, and tailing that one would park a
    /// reader on every agent in the cluster.
    ///
    /// # Errors
    ///
    /// An `Err` when the chat list exists but cannot be read. A chat list that
    /// does not exist yet is not an error — see below.
    pub fn list_conversations(&self) -> Result<Vec<String>, String> {
        let dir = self.convention.chat_list_dir(&self.self_id);
        match self.backend.readdir(&dir) {
            Ok(entries) => {
                let mut peers: Vec<String> = entries.into_iter().map(|e| e.name).collect();
                peers.sort();
                Ok(peers)
            }
            // Nothing has been sent or received yet, so the index does not
            // exist. That is "no conversations", not a failure — distinct from
            // a backend that cannot enumerate at all, which surfaces as Err.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("list conversations at {dir}: {e}")),
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

/// One agent's durable read position in one conversation, and the instance
/// currently advancing it.
///
/// The three fields are ONE register deliberately. A reader whose position
/// lived in one place and whose lease lived in another can be observed
/// half-updated — a new holder reading the previous holder's offset, or an
/// offset advancing under a lease already handed away. Written together, every
/// read sees a self-consistent triple, and that is what lets the claim protocol
/// below be plain read-after-write with no compare-and-swap underneath it.
///
/// Durable facts only: where this agent has read to, and until when the seat is
/// spoken for. Both outlive the process that wrote them, which is the point —
/// this replaced a dotfile beside the workspace, and a position that lives on
/// one machine is lost the moment the agent is restarted on another. That is
/// not hypothetical: it is half of what the Win↔Mac duet hit.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReaderRegister {
    /// The instance tailing this conversation for this agent, empty if the seat
    /// is free. Deliberately NOT the agent name: two instances of the SAME
    /// agent are exactly what this has to tell apart.
    #[serde(default)]
    pub holder: String,
    /// Unix millis after which another instance may take the seat.
    #[serde(default)]
    pub lease_expires_at: u64,
    /// Offset past the last envelope this agent has taken responsibility for.
    #[serde(default)]
    pub read_offset: u64,
}

/// How long a claimed seat stays claimed without being renewed.
///
/// This is a deadlock escape, not a safety boundary: it bounds how long a
/// conversation goes untailed after an instance dies without releasing. It
/// compares wall clocks across machines, so it is sized far above any
/// plausible NTP skew rather than tightly.
const READER_LEASE_MS: u64 = 30_000;

/// Renew once the lease is half spent, so a renewal has a full half-lease to
/// be retried in before anyone else may claim the seat.
///
/// Renewing on every poll return instead would be one replicated write per
/// `block_ms` per conversation, most of them carrying no new information.
const READER_RENEW_MS: u64 = READER_LEASE_MS / 2;

/// How often a receiver re-lists its chat list looking for conversations it is
/// not yet tailing.
///
/// This one IS a poll, and unavoidably so: [`FsBackend`] has no watch
/// primitive, so there is nothing to park on. It bounds how long a peer's
/// FIRST message waits — every later message on that conversation arrives on a
/// parked tail — so it is sized against that latency rather than against its
/// own cost. The listing is a `readdir` of one directory holding one entry per
/// peer, served from the local metastore without consensus; paying it once a
/// second is cheaper than making an opening message wait.
const CONVERSATION_DISCOVERY_MS: u64 = 1_000;

const LOCAL_POLL_BLOCK_MS: u64 = 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Sleep up to `ms`, waking early if the receiver is being shut down.
///
/// A plain sleep would hold shutdown hostage for a full discovery interval.
fn sleep_unless_aborted(abort: &crate::HookAbortSignal, ms: u64) {
    const STEP_MS: u64 = 200;
    let mut left = ms;
    while left > 0 && !abort.is_aborted() {
        let step = left.min(STEP_MS);
        std::thread::sleep(std::time::Duration::from_millis(step));
        left -= step;
    }
}

/// A process-and-machine-unique id for one receiver instance.
///
/// Uniqueness is the whole requirement: two instances of the same agent must
/// never mint the same holder, or each reads the other's claim as its own and
/// both tail the conversation. A pid repeats across machines and a counter
/// repeats across processes, so both are mixed with a nanosecond clock read.
/// The clock is entropy here and nothing else — the lease compares deadlines,
/// never these.
fn instance_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// Whether this instance holds the reader seat for a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seat {
    /// Held by us; carries the position to resume from.
    Taken(u64),
    /// Held by another live instance until this unix-millis deadline.
    HeldBy(u64),
}

/// This mailbox's read position in one conversation, as a claimable seat.
///
/// Exactly one instance advances a given agent's position in a given
/// conversation. Without that, two instances of one agent each tail the same
/// transcript and each advance the same offset, so every message is handled
/// twice or handled by whichever raced ahead — and the other instance never
/// sees it, because the offset it would have read from has already moved.
///
/// The seat is claimed by read-after-write, which is sound here for one
/// specific reason: the register is a single replicated value, so concurrent
/// claims serialise and the last writer wins. Every claimant then reads back
/// and only the one that sees its own holder proceeds. No compare-and-swap
/// primitive is needed, and none exists to use.
pub struct ConversationReader {
    backend: Arc<dyn FsBackend>,
    path: String,
    holder: String,
}

impl ConversationReader {
    /// The register as currently stored, or `None` if this agent has never read
    /// this conversation.
    fn load(&self) -> Result<Option<ReaderRegister>, String> {
        match self.backend.read(&self.path) {
            Ok(raw) if raw.is_empty() => Ok(None),
            Ok(raw) => serde_json::from_slice(&raw)
                .map(Some)
                .map_err(|e| format!("reader register {}: {e}", self.path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read reader register {}: {e}", self.path)),
        }
    }

    fn store(&self, register: &ReaderRegister) -> Result<(), String> {
        let raw = serde_json::to_vec(register)
            .map_err(|e| format!("encode reader register {}: {e}", self.path))?;
        self.backend
            .write_atomic(&self.path, &raw)
            .map_err(|e| format!("write reader register {}: {e}", self.path))
    }

    /// Take the seat if it is free or expired, and report the position to
    /// resume from.
    fn acquire(&self) -> Result<Seat, String> {
        let now = now_ms();
        let current = self.load()?;
        if let Some(held) = current
            .as_ref()
            .filter(|r| !r.holder.is_empty() && r.holder != self.holder && r.lease_expires_at > now)
        {
            return Ok(Seat::HeldBy(held.lease_expires_at));
        }

        // A first read starts at the BEGINNING. There is deliberately no
        // seek-to-tail: a transcript carries one pair's conversation, so its
        // history belongs to this reader, and what seeking skipped was the
        // peer's opening message. The re-reply storm that seeking was added to
        // prevent came from a SHARED inbox, where a restarting agent replayed
        // everything every peer had ever sent it. Against a per-pair transcript
        // with a durable position, a restart resumes and replays nothing.
        //
        // Nor is there a clamp for a position past the tail. That guarded a
        // position kept in a node-local file while the stream lived in the
        // cluster, so the two could outlive each other; the register now sits
        // beside the transcript it describes and they are created and destroyed
        // together.
        let resume = current.as_ref().map_or(0, |r| r.read_offset);

        // `write_atomic` renames into place and will not create the parent, and
        // the side that provisioned this conversation had no reader of its own
        // to make room for. A reader owns its register, including where it
        // lives. Done once per claim rather than on every commit.
        if let Some((dir, _)) = self.path.rsplit_once('/') {
            self.backend
                .create_dir_all(dir)
                .map_err(|e| format!("create reader directory {dir}: {e}"))?;
        }

        self.store(&ReaderRegister {
            holder: self.holder.clone(),
            lease_expires_at: now + READER_LEASE_MS,
            read_offset: resume,
        })?;
        match self.load()? {
            Some(back) if back.holder == self.holder => Ok(Seat::Taken(back.read_offset)),
            Some(back) => Ok(Seat::HeldBy(back.lease_expires_at)),
            // Vanished between write and read-back: something deleted the
            // conversation under us. Treat as contended rather than looping.
            None => Ok(Seat::HeldBy(now + READER_LEASE_MS)),
        }
    }

    /// Record `offset` as read and renew the lease. `Ok(false)` means the seat
    /// was taken over and this instance must stop advancing it.
    ///
    /// Also the renewal path — renewing is committing the position already
    /// held, so there is one write and one place that can get it wrong.
    fn commit(&self, offset: u64) -> Result<bool, String> {
        if let Some(current) = self.load()? {
            if current.holder != self.holder {
                return Ok(false);
            }
        }
        self.store(&ReaderRegister {
            holder: self.holder.clone(),
            lease_expires_at: now_ms() + READER_LEASE_MS,
            read_offset: offset,
        })?;
        Ok(true)
    }

    /// Give the seat up, keeping the position.
    ///
    /// Without this a clean shutdown still holds the seat until the lease runs
    /// out, so an agent restarted inside that window is locked out of its own
    /// conversation and simply appears not to receive. The lease is the
    /// recovery path for an instance that DIED; an instance that is leaving
    /// says so.
    ///
    /// A no-op when the seat has already moved on, so a tail that was taken
    /// over cannot evict its successor on the way out.
    fn release(&self) -> Result<(), String> {
        let Some(current) = self.load()? else {
            return Ok(());
        };
        if current.holder != self.holder {
            return Ok(());
        }
        self.store(&ReaderRegister {
            holder: String::new(),
            lease_expires_at: 0,
            read_offset: current.read_offset,
        })
    }
}

impl Mailbox {
    /// This mailbox's claimable read position in the conversation with `peer`.
    #[must_use]
    pub fn conversation_reader(&self, peer: &str) -> ConversationReader {
        ConversationReader {
            path: self
                .convention
                .reader_path(&self.self_id, peer, &self.self_id),
            backend: Arc::clone(&self.backend),
            holder: instance_id(),
        }
    }
}

/// Spawn the background receiver: one parked tail per conversation, each
/// handing new envelopes to `sink` and advancing its own read position.
///
/// The ONE receive loop. It used to be two — this one over gRPC for A2A and a
/// second for the local JSONL inbox — and because the loop was duplicated the
/// two copies drifted: the local one kept its cursor in a local variable
/// starting at 0, so every process start replayed the whole inbox. The loop
/// does not vary by transport, only the `mailbox` handed to it does, so there
/// is nothing for a second copy to do except diverge.
///
/// A receiver used to tail ONE stream — its own inbox, which every sender
/// appended to. Conversations put each pair on its own transcript, so a
/// receiver tails N of them, discovered from its chat list. That directory is
/// written by whichever side provisions the conversation, so a peer that has
/// never written before shows up there before its first message is readable.
///
/// Event-driven, not polling, wherever the backend can be: each tail parks
/// inside [`Mailbox::poll_conversation`] until the backend reports new data or
/// `block_ms` elapses. A DT_STREAM backend parks on a condvar the kernel
/// signals (including from a peer's replicated append); `StdFsBackend` has no
/// such primitive and falls back to a bounded size poll. Discovery is the one
/// genuine poll, for the reason on [`CONVERSATION_DISCOVERY_MS`].
///
/// `sink` returns whether the consumer has TAKEN RESPONSIBILITY for the
/// envelope — not merely that it was handed over. The read position does not
/// advance past an envelope that was not accepted, so a consumer that blocks
/// until it has the message applies back-pressure to the receiver instead of
/// letting messages pile up past a position already recorded as read.
///
/// That distinction is the whole reason this is a `bool`. Committing after a
/// `sink` that only enqueues means a crash loses everything still in the queue,
/// silently, with the sender already told "delivered" — the loss the register
/// exists to prevent, reintroduced one layer up.
///
/// `sink` is shared by every tail, so it must be `Sync`: it is one consumer
/// receiving from all conversations, not one per peer.
pub fn spawn_inbox_poller(
    mailbox: Arc<Mailbox>,
    block_ms: u64,
    label: &'static str,
    abort: crate::HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + Sync + 'static,
) -> std::thread::JoinHandle<()> {
    let sink: Arc<dyn Fn(&MailboxEnvelope) -> bool + Send + Sync> = Arc::new(sink);
    std::thread::Builder::new()
        .name(format!("{label}-inbox"))
        .spawn(move || {
            // Announce before listening. A receiver that only becomes visible
            // once someone has written to it cannot be found by whoever wants
            // to write, and cannot be observed to be running at all.
            if let Err(e) = mailbox.ensure_presence() {
                eprintln!("[{label}] announcing this agent failed: {e}");
            }
            let mut tailing: std::collections::HashMap<String, std::thread::JoinHandle<()>> =
                std::collections::HashMap::new();
            while !abort.is_aborted() {
                match mailbox.list_conversations() {
                    Ok(peers) => {
                        for peer in peers {
                            if tailing.contains_key(&peer) {
                                continue;
                            }
                            let tail = spawn_conversation_tail(
                                Arc::clone(&mailbox),
                                peer.clone(),
                                block_ms,
                                label,
                                abort.clone(),
                                Arc::clone(&sink),
                            );
                            tailing.insert(peer, tail);
                        }
                    }
                    Err(e) => eprintln!("[{label}] listing conversations failed: {e}"),
                }
                sleep_unless_aborted(&abort, CONVERSATION_DISCOVERY_MS);
            }
            // Joining is what makes the returned handle mean "the receiver has
            // stopped". Without it the caller's join returns while N tails are
            // still delivering into a sink it believes is finished with.
            for (_, tail) in tailing {
                let _ = tail.join();
            }
        })
        .expect("spawn inbox receiver thread")
}

/// One conversation's tail: hold the reader seat, park on the transcript,
/// deliver, commit.
fn spawn_conversation_tail(
    mailbox: Arc<Mailbox>,
    peer: String,
    block_ms: u64,
    label: &'static str,
    abort: crate::HookAbortSignal,
    sink: Arc<dyn Fn(&MailboxEnvelope) -> bool + Send + Sync>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("{label}-tail"))
        .spawn(move || {
            let reader = mailbox.conversation_reader(&peer);
            // Outer loop re-claims: a seat held by an instance that then dies is
            // free once its lease expires, and nobody is coming to tell us.
            while !abort.is_aborted() {
                let mut cursor = match reader.acquire() {
                    Ok(Seat::Taken(at)) => at,
                    Ok(Seat::HeldBy(until)) => {
                        sleep_unless_aborted(&abort, until.saturating_sub(now_ms()).max(1));
                        continue;
                    }
                    Err(e) => {
                        eprintln!("[{label}] claiming the reader seat for {peer} failed: {e}");
                        sleep_unless_aborted(&abort, block_ms.max(1));
                        continue;
                    }
                };
                let mut renew_at = now_ms() + READER_RENEW_MS;
                while !abort.is_aborted() {
                    let (msgs, next) = match mailbox.poll_conversation(&peer, cursor, block_ms) {
                        Ok(batch) => batch,
                        Err(e) => {
                            eprintln!("[{label}] polling the conversation with {peer} failed: {e}");
                            // Avoid a hot error loop when the backend is sick;
                            // the blocking read itself paces the happy path.
                            sleep_unless_aborted(&abort, block_ms.max(1));
                            continue;
                        }
                    };
                    let accepted = msgs.iter().all(|m| sink(m));
                    // Commit only on real forward progress the consumer took in
                    // full, otherwise only when the lease needs renewing. A
                    // batch is all-or-nothing because the position is one
                    // offset — there is no way to say "past the second but not
                    // the third". Re-delivering an accepted envelope is the
                    // tolerable half of that: at-least-once, the right side to
                    // err on for mail.
                    let commit_at = if accepted && next > cursor {
                        next
                    } else if now_ms() >= renew_at {
                        cursor
                    } else {
                        continue;
                    };
                    match reader.commit(commit_at) {
                        Ok(true) => {
                            cursor = commit_at;
                            renew_at = now_ms() + READER_RENEW_MS;
                        }
                        // Another instance of this agent took the seat. It is
                        // now the one advancing the position; two tails
                        // advancing one offset is the split this prevents.
                        Ok(false) => {
                            eprintln!(
                                "[{label}] the reader seat for {peer} was taken over; \
                                 stopping this tail"
                            );
                            break;
                        }
                        Err(e) => {
                            eprintln!("[{label}] committing the read position for {peer}: {e}");
                            sleep_unless_aborted(&abort, block_ms.max(1));
                        }
                    }
                }
            }
            // Hand the seat back rather than leaving the next instance to wait
            // out a lease held by a process that has already stopped.
            if let Err(e) = reader.release() {
                eprintln!("[{label}] releasing the reader seat for {peer} failed: {e}");
            }
        })
        .expect("spawn conversation tail thread")
}

/// [`spawn_inbox_poller`] over a workspace-local root.
///
/// Owns only what is local-specific: the backend and the root the convention
/// hangs off. The read position is NOT local-specific and is not passed in —
/// it lives in the conversation, beside the transcript it describes, under both
/// backends alike.
pub fn spawn_local_poller(
    workspace_root: std::path::PathBuf,
    self_id: String,
    abort: crate::HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + Sync + 'static,
) -> std::thread::JoinHandle<()> {
    let mailbox = Arc::new(Mailbox::workspace_local(&workspace_root, self_id));
    spawn_inbox_poller(mailbox, LOCAL_POLL_BLOCK_MS, "local-inbox", abort, sink)
}

// ---------------------------------------------------------------------------
// Which mailbox a send writes through
// ---------------------------------------------------------------------------

thread_local! {
    static SCOPED_MAILBOX: std::cell::RefCell<Option<Arc<Mailbox>>> =
        const { std::cell::RefCell::new(None) };
}

/// The session's mailbox, established for the duration of one tool call.
///
/// Before this existed, `send`'s destination came from whether an A2A sender had
/// been wired for the PROCESS, and the recipient was never consulted. Two
/// decisions that had to agree, with nothing making them: a session with A2A on
/// addressed a local sub-agent at `/agents/<name>/chat-with-me`, a stream no
/// ephemeral agent has, while that sub-agent read `.sudocode-inbox/<name>.jsonl`
/// and heard nothing.
///
/// Now there is one mailbox, `send` names a recipient, the convention turns that
/// into a path, and the backend decides what crossing it means.
///
/// The mailbox is OWNED by the tool dispatcher — one per session, handed over at
/// startup — and this scope only carries it the last hop, onto whichever thread
/// the dispatcher runs the synchronous tool body on. Deliberately not a process
/// global: a daemon hosts several co-hosted agents at once, each with its own
/// identity and inbox, and one global would have them writing as each other. The
/// same reasoning (and the same shape) as
/// [`crate::workspace_root::WorkspaceRootScope`], which crosses that last hop
/// beside this one.
///
/// Also what makes this testable: a handle set once per process cannot be
/// changed, so two tests in one binary needing different conventions would be in
/// each other's way. A test gives its executor a mailbox like the host does.
pub struct MailboxScope {
    previous: Option<Arc<Mailbox>>,
}

impl MailboxScope {
    /// Enter `mailbox` as this thread's mailbox until the guard drops.
    #[must_use]
    pub fn enter(mailbox: Arc<Mailbox>) -> Self {
        let previous = SCOPED_MAILBOX.with(|cell| cell.borrow_mut().replace(mailbox));
        Self { previous }
    }
}

impl Drop for MailboxScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        SCOPED_MAILBOX.with(|cell| {
            *cell.borrow_mut() = previous;
        });
    }
}

/// The mailbox to send through: the one this thread is scoped onto, else
/// workspace-local JSONL.
///
/// Two levels, and the second is the contract rather than a fallback: a session
/// that has a mailbox hands it to its dispatcher, which scopes the thread running
/// the tool; a plain `scode` with no nexus configured has always delivered to
/// `.sudocode-inbox/` in the workspace, and still does — through this same call,
/// resolved one level down. Being one code path is what stops the two drifting.
///
/// `self_id` is empty on the ambient fallback deliberately. It serves two
/// purposes on a `Mailbox` — filling an envelope's `from`, and filtering your own
/// writes out of a poll — and neither applies: `send` always supplies `from`, and
/// this handle is never polled. A receiver builds its own with its real identity.
#[must_use]
pub fn sending_mailbox() -> Arc<Mailbox> {
    if let Some(mailbox) = SCOPED_MAILBOX.with(|cell| cell.borrow().clone()) {
        return mailbox;
    }
    // Ambient fallback with no scoped mailbox: workspace-rooted, so a
    // coordinator/sub-agent send stays per-workspace (a sub-agent belongs to its
    // parent scode; different scodes must not cross-talk). The standalone
    // same-machine pair uses a scoped mailbox rooted at `local_pair_root()`,
    // installed by the host — this fallback is not that path.
    //
    // The identity is DERIVED rather than left empty. It used to be empty, on
    // the reasoning that a real identity rides the scoped mailbox and this one
    // only needed to address a recipient. A conversation is addressed by its
    // PAIR, so a nameless side has no conversation to be in: the id degenerates
    // and the chat-list entry comes out as `…/conversations/` with no leaf.
    // `local_agent_name` is the same derivation the host uses when it builds
    // the scoped mailbox, so the two agree instead of nearly agreeing.
    let root = crate::current_workspace_root_or_default();
    let self_id = local_agent_name(None, &root);
    Arc::new(Mailbox::workspace_local(&root, self_id))
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
            InboxConvention::new(root.to_string()),
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

    /// A restarted receiver must neither replay what it already handled nor
    /// lose what arrived while it was gone.
    ///
    /// Both halves in one test because they are the two ways to get this wrong
    /// and a fix for either alone reintroduces the other. Replaying is the #81
    /// re-reply storm; losing the offline message is the Win-Mac duet handoff,
    /// where the sender was told "delivered", the envelope sat durably in the
    /// transcript, and no later reader ever looked back.
    ///
    /// What makes the second run able to resume at all is WHERE the position
    /// lives: in the conversation, beside the transcript it describes. The
    /// node-local cursor file this replaced did not survive the agent being
    /// restarted anywhere else, and a position that can be lost is a position
    /// that replays.
    ///
    /// Note what is NOT asserted: that a first run skips history. It does not,
    /// and must not. A transcript is one pair's conversation, so what a
    /// first-run seek-to-tail would skip is the peer's opening message.
    #[test]
    fn a_restarted_receiver_resumes_instead_of_replaying() {
        let ws = temp_workspace("poller-resume");
        let ws_path = std::path::PathBuf::from(&ws);
        let peer = local_mailbox(&ws, "peer");

        peer.send(note("peer", "me", "first-1")).expect("send");
        peer.send(note("peer", "me", "first-2")).expect("send");

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        let abort = crate::HookAbortSignal::new();
        let sink_seen = Arc::clone(&seen);
        let first =
            spawn_local_poller(ws_path.clone(), "me".to_string(), abort.clone(), move |m| {
                sink_seen.lock().unwrap().push(m.body.clone());
                true
            });
        wait_until("the conversation this receiver has never read", || {
            seen.lock().unwrap().len() == 2
        });
        abort.abort();
        first.join().expect("first poller joins");

        // Arrives with nobody listening - must survive the gap.
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
            seen.lock().unwrap().len() == 3
        });
        abort2.abort();
        second.join().expect("second poller joins");

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                "first-1".to_string(),
                "first-2".to_string(),
                "offline-1".to_string()
            ],
            "each message exactly once and in order: the second run must resume \
             where the first stopped, not replay what it already took"
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
        let (msgs, _cursor) = worker_mb.poll_conversation("team-lead", 0, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "team-lead");
        assert_eq!(msgs[0].body, "hello worker");

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn poll_filters_self_writes() {
        let ws = temp_workspace("self-filter");
        let mb = local_mailbox(&ws, "agent-a");

        // Both land in the SAME transcript — that is the point of a shared
        // conversation. Each side tells its own appends apart by `from`.
        mb.send(MailboxEnvelope {
            from: "agent-a".to_string(),
            to: "agent-b".to_string(),
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
            to: "agent-b".to_string(),
            body: "from peer".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();

        let (msgs, _) = mb.poll_conversation("agent-b", 0, 0).unwrap();
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

        let envs = mb.read_conversation("worker").unwrap();
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

        // The sender is in its own namespace — provisioning a conversation
        // gives BOTH sides a presence. Excluding self is the broadcast
        // caller's job, not this call's: "who is there" and "who should this
        // message go to" are different questions.
        let names = mb.list_recipients().unwrap();
        assert_eq!(names, vec!["alpha", "beta", "coordinator"]);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn transcript_path_conventions() {
        let cid = a2a::conversation_id("me", "worker");

        // One shape, root-prefixed. Standalone: a host dir root.
        let local = InboxConvention::new("/workspace".to_string());
        assert_eq!(
            local.transcript_path("me", "worker"),
            format!("/workspace/conversations/{cid}/transcript")
        );

        // Nexus: empty root → daemon-absolute, exactly one leading slash.
        let nexus = InboxConvention::new(String::new());
        assert_eq!(
            nexus.transcript_path("me", "worker"),
            format!("/conversations/{cid}/transcript")
        );

        // A trailing slash on the root must not double up.
        let trailing = InboxConvention::new("/workspace/".to_string());
        assert_eq!(
            trailing.transcript_path("me", "worker"),
            format!("/workspace/conversations/{cid}/transcript")
        );

        // Order-free, which is what lets each side derive the shared transcript
        // from its own point of view without agreeing on who is "first".
        assert_eq!(
            nexus.transcript_path("worker", "me"),
            nexus.transcript_path("me", "worker")
        );

        // The chat list is keyed by the PEER, not the cid: a digest is one-way,
        // so a cid-named entry would say a conversation exists without saying
        // with whom, and the receiver could derive no transcript from it.
        assert_eq!(
            nexus.chat_list_path("me", "worker"),
            "/agents/me/conversations/worker"
        );
    }

    #[test]
    fn local_agent_name_prefers_configured_over_derived() {
        let ws = std::path::Path::new("/home/me/projects/app");
        assert_eq!(local_agent_name(Some("alice"), ws), "alice");
        assert_eq!(local_agent_name(Some("  bob  "), ws), "bob");
        // Empty / whitespace config falls through to derivation.
        assert_ne!(local_agent_name(Some("   "), ws), "");
    }

    #[test]
    fn local_agent_name_disambiguates_same_basename_folders() {
        let a = local_agent_name(None, std::path::Path::new("/home/me/x/app"));
        let b = local_agent_name(None, std::path::Path::new("/home/me/y/app"));
        assert!(a.starts_with("app-"), "keeps basename: {a}");
        assert!(b.starts_with("app-"), "keeps basename: {b}");
        assert_ne!(a, b, "same basename, different paths must not collide");
        // Deterministic: same path → same name.
        assert_eq!(
            a,
            local_agent_name(None, std::path::Path::new("/home/me/x/app"))
        );
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

        // Deliberately NOT waiting for the receiver to be listening first. A
        // first read starts at the beginning of the conversation, so a message
        // that lands before the tail exists is still delivered once discovery
        // finds it — and a test that had to establish "listening" first could
        // not tell that apart from a seek-to-tail that silently drops it.
        //
        // Write a message from a sub-agent to team-lead.
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
