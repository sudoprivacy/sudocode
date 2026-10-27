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

// Receivers run behind terminal editors. Diagnostics belong in the shared
// receiver observation and tracing path, never directly on terminal streams.
#![deny(clippy::print_stderr, clippy::print_stdout)]

use std::sync::Arc;

use crate::agent_mailbox::MailboxEnvelope;
use crate::fs_backend::FsBackend;

/// Whether `path` is a log a send APPENDS to — a conversation's transcript.
///
/// Re-exported from the a2a substrate so that a backend deciding "is this
/// framed?" and a test asserting the same thing cannot each answer it
/// separately. They did: both spelled the leaf themselves, and when the log
/// moved to `…/transcript` both silently said "not a stream" — sending a JSONL
/// line to the local filesystem instead of a record to the daemon, and
/// reporting success.
///
/// The daemon decides this, and it is the same question it answers when it
/// chooses what to stamp and what a cross-org guest may reach, so there is one
/// predicate and scode reads it rather than holding an opinion.
pub use a2a::is_conversation_transcript_path;
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
    let full_raw = workspace_root.to_string_lossy();
    let full = strip_windows_verbatim_prefix(&full_raw);
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

/// Strip the Windows extended-length verbatim prefix (`\\?\`, and its `\\?\UNC\`
/// UNC form) from a path string. These prefixes spell the same location as the
/// plain path, so an identity derived from the string must not depend on which
/// spelling a call site happened to pass. A no-op on paths without the prefix
/// (i.e. always, off Windows).
#[inline]
fn strip_windows_verbatim_prefix(path: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(path)
    }
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

type BackendResolver = dyn Fn() -> Result<Arc<dyn FsBackend>, String> + Send + Sync;

/// A fixed transport, or a host's still-unavailable binding to that transport.
/// Resolving must keep the selected namespace and return the actual backend;
/// it must never substitute a local filesystem after an error.
#[derive(Clone)]
pub(crate) enum MailboxBackend {
    Fixed(Arc<dyn FsBackend>),
    Resolving(Arc<BackendResolver>),
}

impl MailboxBackend {
    #[inline]
    pub(crate) fn resolve(&self) -> Result<Arc<dyn FsBackend>, String> {
        match self {
            Self::Fixed(backend) => Ok(Arc::clone(backend)),
            Self::Resolving(resolve) => resolve(),
        }
    }
}

/// Transport-agnostic agent mailbox.
///
/// Wraps an [`FsBackend`] + path convention, providing send/receive that
/// works identically whether the underlying transport is a local JSONL
/// file or a nexus DT_STREAM.
pub struct Mailbox {
    backend: MailboxBackend,
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
        Self::with_backend(MailboxBackend::Fixed(backend), self_id, convention)
    }

    #[inline]
    fn with_backend(backend: MailboxBackend, self_id: String, convention: InboxConvention) -> Self {
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

    /// Keep a daemon namespace while its host prepares this agent's connection.
    /// Every IO path, including the existing receiver, uses the same resolver.
    /// A failed resolution is an explicit IO failure, never an empty scope.
    #[inline]
    #[must_use]
    pub fn daemon_resolving(
        self_id: String,
        resolve: impl Fn() -> Result<Arc<dyn FsBackend>, String> + Send + Sync + 'static,
    ) -> Self {
        Self::with_backend(
            MailboxBackend::Resolving(Arc::new(resolve)),
            self_id,
            InboxConvention::new(String::new()),
        )
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
        Self::workspace_at(Arc::new(crate::fs_backend::StdFsBackend), root, self_id)
    }

    /// A mailbox rooted at its backend's own workspace, independently of the shell.
    ///
    /// # Errors
    /// The backend cannot resolve its working root.
    pub fn workspace(fs: Arc<dyn FsBackend>, self_id: String) -> std::io::Result<Self> {
        let root = fs.working_root()?;
        Ok(Self::workspace_at(fs, std::path::Path::new(&root), self_id))
    }

    /// A mailbox at an explicit root in the supplied backend's namespace.
    #[must_use]
    pub fn workspace_at(fs: Arc<dyn FsBackend>, root: &std::path::Path, self_id: String) -> Self {
        Self::new(
            fs,
            self_id,
            InboxConvention::new(
                strip_windows_verbatim_prefix(&root.to_string_lossy()).into_owned(),
            ),
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
    /// Classification comes from the backend's current entry type. Errors
    /// propagate to the caller. Polling obtains data and its cursor directly
    /// through `tail_read` within that operation's wait budget.
    #[inline]
    fn backend_frames(&self, path: &str) -> Result<bool, String> {
        self.backend
            .resolve()?
            .is_append_stream(path)
            .map_err(|error| format!("classify mailbox {path}: {error}"))
    }

    /// A nexus A2A mailbox for `agent` over an already-dialled client.
    ///
    /// The one place that says what a nexus A2A mailbox is made of. Pairing a
    /// backend with a convention by hand at each call site is how they end up
    /// mismatched — a `LocalJsonl` convention over a VFS backend writes lines
    /// nothing tails, and the mistake is silent.
    ///
    /// The underlying [`NexusVfsFsBackend::from_arc`] context is unverified
    /// env metadata, not a server-verifiable credential: every operation
    /// through this mailbox fails closed before any RPC. A mailbox that
    /// performs IO needs the mTLS pairing — [`Self::over_nexus_mtls`].
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

    /// The same mailbox over a connection that IS the credential: an mTLS
    /// client certificate the daemon verifies (identity from its SAN). The
    /// fail-closed env/delegation context behind [`Self::over_nexus`] does not
    /// apply — the transport itself is the server-verified authorization — but
    /// a host-injected `NEXUS_RESOURCE_SCOPE` still narrows it.
    #[must_use]
    pub fn over_nexus_mtls(
        client: Arc<nexus_vfs_client::NexusVfsClient>,
        agent: impl Into<String>,
        auth_token: impl Into<String>,
        zone_id: impl Into<String>,
    ) -> Self {
        Self::new(
            Arc::new(crate::fs_backend::NexusVfsFsBackend::from_arc_mtls(
                client,
                auth_token.into(),
                zone_id,
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

    /// The persistent description in this mailbox's actual agent namespace.
    pub fn description_store(
        &self,
        agent: &str,
    ) -> std::io::Result<crate::agent_description::DescriptionStore> {
        if agent.is_empty() || agent == "." || agent == ".." || agent.contains(['/', '\\', '\0']) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid agent name",
            ));
        }
        Ok(crate::agent_description::DescriptionStore::new(
            self.backend.clone(),
            format!(
                "{}/{agent}/{}",
                self.convention.agents_dir(),
                crate::agent_description::DESCRIPTION_FILE
            ),
        ))
    }

    /// Read-only binding of this exact mailbox, before any A2A IO.
    #[must_use]
    pub fn binding(&self) -> crate::external_a2a::MailboxBinding {
        crate::external_a2a::MailboxBinding {
            actor: self.self_id.clone(),
            namespace_root: self.convention.root.clone(),
        }
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
    /// This agent's recorded position in its conversation with `peer`, or
    /// `None` if it has never read that conversation.
    ///
    /// Readable rather than private because the position is a durable fact
    /// ABOUT the conversation, not bookkeeping private to the receiver: an
    /// operator asking "has this agent seen it yet?" and a test asserting that
    /// a receiver advanced past what it surfaced are the same question, and
    /// neither should have to reconstruct the register's path to ask it.
    ///
    /// # Errors
    /// Returns an error when the register exists but cannot be read or parsed.
    pub fn read_position(&self, peer: &str) -> Result<Option<u64>, String> {
        self.conversation_reader(peer)
            .load()
            .map(|register| register.map(|r| r.read_offset))
    }

    /// Read without advancing the durable cursor. The returned receipt holds
    /// the exclusive reader until acknowledgement or drop. A dropped batch
    /// stays unread, including after a process crash.
    pub fn read_unread(&self, peer: &str) -> Result<Option<UnreadBatch>, String> {
        let Some(reader) = self.conversation_reader(peer).acquire()? else {
            return Ok(None);
        };
        let (messages, next) = self.poll_conversation(peer, reader.offset, 0)?;
        Ok(Some(UnreadBatch {
            messages,
            next,
            reader,
        }))
    }

    /// Gather pending deliveries across conversations, retaining their receipts.
    pub fn read_pending(&self) -> Result<PendingMessages, String> {
        let mut batches = Vec::new();
        for peer in self.list_conversations()? {
            if let Some(batch) = self.read_unread(&peer)? {
                batches.push(batch);
            }
        }
        Ok(PendingMessages { batches })
    }

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
            .resolve()?
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
        let backend = self.backend.resolve()?;
        let root = self.convention.conversation_root(&self.self_id, peer);
        for (owner, other) in [(self.self_id.as_str(), peer), (peer, self.self_id.as_str())] {
            let dir = self.convention.chat_list_dir(owner);
            backend
                .create_dir_all(&dir)
                .map_err(|e| format!("create chat list {dir}: {e}"))?;
            // A link, which is what this is: an index pointing into the flat
            // conversation store rather than a copy of where it lives.
            //
            // It was a plain entry holding the root as bytes, because only one of the
            // three backends could make a link — the gRPC `Setattr` carried no link
            // target, so over that transport `link` fell to the trait's no-op and the
            // entry never appeared at all. A receiver finds its conversations by
            // listing this directory, so an entry that never appears is a receiver
            // that never hears anything.
            //
            // The wire carries a link target now, so all three backends realise this
            // one call: a DT_LINK on the VFS (the destination in metadata, which a
            // mount with no content store keeps) and a pointer file on the host FS,
            // where a privilege-free symlink does not exist. `read_link` is the follow
            // half on both, so a caller resolving the index is backend-agnostic.
            let alias = self.convention.chat_list_path(owner, other);
            backend
                .link(&alias, &root)
                .map_err(|e| format!("index conversation for {owner} at {alias}: {e}"))?;
        }
        let path = self.transcript_path(peer);
        backend
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

        if self.backend_frames(&path)? {
            return self
                .backend
                .resolve()?
                .append(&path, &envelope.to_bytes())
                .map_err(|e| format!("mailbox send to {path}: {e}"));
        }

        // Non-framed transcript: a message is one JSONL line at the
        // conversation's transcript path, written through the one envelope-line
        // writer so the format has a single definition — and through THIS
        // mailbox's backend, so it lands in the same filesystem the reader is
        // reading. Which shape a transcript has is not a property of the
        // backend: a kernel-backed conversation whose stream could not be
        // created is byte-addressed too, and writing that line to the host disk
        // put the reply somewhere its peer could never look.
        //
        // No second arm any more. There used to be one for `SharedStream`,
        // which returned "not an append stream — inbox not provisioned" — but
        // that variant was describing a conversation (both parties reading and
        // writing one log) as if it were a special case, and with conventions
        // collapsed there is exactly one shape to write.
        crate::agent_mailbox::append_envelope_via(self.backend.resolve()?.as_ref(), &path, envelope)
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
        let (mut messages, next) = self.poll_tail(&path, cursor, block_ms)?;
        // Session control has its own generation-aware codec and consumer.
        // All peer consumers skip those frames while advancing past them.
        messages.retain(|message| message.kind != a2a::session::SESSION_KIND);
        Ok((messages, next))
    }

    /// Drain backend-owned chunks without a separate type lookup. A chunk is
    /// either a complete envelope frame or a batch of JSONL envelope records.
    fn poll_tail(
        &self,
        path: &str,
        mut cursor: u64,
        block_ms: u64,
    ) -> Result<(Vec<MailboxEnvelope>, u64), String> {
        let mut out = Vec::new();
        let backend = self.backend.resolve()?;
        let mut first = true;
        loop {
            let blocking = first && block_ms > 0;
            first = false;
            let (data, next, eof) = backend
                .tail_read(path, cursor, if blocking { block_ms } else { 0 })
                .map_err(|e| format!("mailbox poll {path}@{cursor}: {e}"))?;
            if eof {
                break;
            }
            let envelopes = MailboxEnvelope::from_bytes(&data).map_or_else(
                || crate::agent_mailbox::parse_envelope_lines(&data),
                |envelope| vec![envelope],
            );
            for env in envelopes {
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

    /// Read the whole conversation with `peer` (batch read). Primarily for the
    /// coordinator's multi-turn loop, which drains between turns.
    pub fn read_conversation(&self, peer: &str) -> Result<Vec<MailboxEnvelope>, String> {
        let path = self.transcript_path(peer);
        let is_stream = self.backend_frames(&path)?;
        if is_stream {
            let (envs, _cursor) = self.poll_tail(&path, 0, 0)?;
            Ok(envs)
        } else {
            // Through the backend, for the same reason the write is: the
            // transcript of a kernel-backed conversation is not on host disk.
            match self.backend.resolve()?.read(&path) {
                Ok(bytes) => Ok(crate::agent_mailbox::parse_envelope_lines(&bytes)),
                // Nothing written yet is an empty conversation, not a failure.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(format!("read conversation {path}: {e}")),
            }
        }
    }

    /// Every agent with a presence in this namespace — who a broadcast reaches.
    ///
    /// # Errors
    ///
    /// A namespace whose directory has not been created yet has no recipients.
    /// An `Err` when an existing namespace cannot be enumerated, or when an entry's
    /// presence cannot be determined — never an empty list, and never a silently
    /// shortened one. The difference matters to every caller: a broadcast over "no
    /// recipients" reports success having delivered nothing, and an agent told it
    /// has no peers tells its operator so. Saying it at the source means no caller
    /// has to remember to ask first.
    pub fn list_recipients(&self) -> Result<Vec<String>, String> {
        let dir = self.convention.agents_dir();
        let backend = self.backend.resolve()?;
        let entries = match backend.readdir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("list recipients at {dir}: {error}")),
        };
        // A recipient is an agent that ANNOUNCED presence, and presence is a
        // `conversations` subdir under `agents_dir/<name>/` (the
        // `ensure_presence` contract). Filter to that shape so infrastructure
        // siblings a mount can expose under `/agents` (e.g. `raft`, `sm` on a
        // co-host topology) are not mistaken for peers. Checking the contract
        // rather than a denylist means a future infra entry needs no upkeep.
        // An entry whose presence cannot be DETERMINED is an error, not a
        // non-agent. Swallowing that check (`unwrap_or(false)`) would drop a real
        // agent whenever the probe itself failed — the same "I could not look"
        // reported as "nobody is there" that this call returns `Err` to prevent,
        // one level down and harder to see, because the list still looks plausible.
        // `is_dir` first, so a stray file costs no round trip.
        let mut names: Vec<String> = Vec::with_capacity(entries.len());
        for entry in entries {
            if !entry.is_dir {
                continue;
            }
            let chat_list = self.convention.chat_list_dir(&entry.name);
            if backend.exists(&chat_list).map_err(|e| {
                format!(
                    "list recipients at {dir}: cannot tell whether {} announced presence \
                     ({chat_list}): {e}",
                    entry.name
                )
            })? {
                names.push(entry.name);
            }
        }
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
        match self.backend.resolve()?.readdir(&dir) {
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
                delivery: crate::turn_input::InputDelivery::Steer,
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

/// An agent's durable position in one conversation. Exclusive ownership comes
/// from the backend lock. Where that lock is an expiring lease, writes to this
/// register are additionally fenced by a conditional write when the backend has
/// one ([`crate::cursor_fence`]), so a holder that outlived its lease cannot
/// overwrite a newer holder's position.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReaderRegister {
    /// The instance tailing this conversation for this agent, empty if the seat
    /// is free. Deliberately NOT the agent name: two instances of the SAME
    /// agent are exactly what this has to tell apart.
    #[serde(default)]
    /// The claim that failed to advance, empty when no reader was held (see
    /// [`DeliveryStall::ClaimFailed`]).
    pub holder: String,
    /// Legacy diagnostic deadline; the backend lock controls ownership.
    #[serde(default)]
    pub lease_expires_at: u64,
    /// Offset past the last envelope this agent has taken responsibility for.
    #[serde(default)]
    pub read_offset: u64,
    /// Which claim wrote this register, on a backend whose writes are
    /// conditional (see [`crate::cursor_fence`]). Zero, and left out of the
    /// stored form, everywhere else.
    #[serde(default, skip_serializing_if = "is_unfenced")]
    pub epoch: u64,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature
fn is_unfenced(epoch: &u64) -> bool {
    *epoch == 0
}

/// How long a claimed seat stays claimed without being renewed.
///
/// This is a deadlock escape, not a safety boundary: it bounds how long a
/// conversation goes untailed after an instance dies without releasing. It
/// is enforced by the backend lock manager. This client never steals a seat
/// based on a timestamp stored in the cursor file.
const READER_LEASE_MS: u64 = 30_000;

/// Renew once the lease is half spent, so a renewal has a full half-lease to
/// be retried in before anyone else may claim the seat.
///
/// Renewing on every poll return instead would be one replicated write per
/// `block_ms` per conversation, most of them carrying no new information.
const READER_RENEW_MS: u64 = READER_LEASE_MS / 2;

/// How far ahead of the lease TTL a holder stops acting on a lease it last
/// proved a while ago.
///
/// The backend expires the lock `READER_LEASE_MS` after it *received* the last
/// renewal, so every age here is measured from when a renewal was *requested*.
/// The margin bounds how old a successful renewal may be and still count, and
/// how long belief in the last one lasts while the next is still out. It is a
/// scheduling heuristic, not a guarantee: a process that is suspended can still
/// deliver or commit after the backend handed the seat to a standby. Only a
/// fenced register write closes the commit side, and delivery cannot be fenced
/// at all.
const LEASE_SAFETY_MARGIN_MS: u64 = 10_000;

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

/// Refused inputs stay unread and retry with backoff, capped at the renewal
/// interval so a fast consumer failure cannot spin or defer lease renewal.
const DELIVERY_RETRY_INITIAL_MS: u64 = 1_000;

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

/// A durable cursor guarded by the backend's atomic exclusive lock.
pub struct ConversationReader {
    backend: MailboxBackend,
    path: String,
    holder: String,
    agent: String,
}

impl ConversationReader {
    /// The register as currently stored, or `None` if this agent has never read
    /// this conversation.
    fn load(&self) -> Result<Option<ReaderRegister>, String> {
        crate::cursor_fence::load_plain(self.backend.resolve()?.as_ref(), &self.path)
    }

    /// Hold the conversation's lock, then claim its register through
    /// [`crate::cursor_fence`]. `None` while another instance holds either.
    fn acquire(self) -> Result<Option<OwnedReader>, String> {
        let backend = self.backend.resolve()?;
        let Some(lease) = HeldLease::try_acquire(&backend, &format!("{}.lock", self.path))
            .map_err(|e| format!("claim reader {}: {e}", self.path))?
        else {
            return Ok(None);
        };
        let Some((cursor, offset)) = crate::cursor_fence::RegisterCursor::claim(
            backend,
            self.path,
            self.holder,
            now_ms() + READER_LEASE_MS,
        )?
        else {
            return Ok(None);
        };
        record_cursor_guarantee(&self.agent, cursor.guarantee());
        Ok(Some(OwnedReader {
            lease,
            cursor,
            offset,
        }))
    }
}

/// Whether a lease last proven `elapsed_ms` ago may still be acted on: it has to
/// leave [`LEASE_SAFETY_MARGIN_MS`] before the TTL the lock was taken with.
fn within_grace(elapsed_ms: u64) -> bool {
    elapsed_ms.saturating_add(LEASE_SAFETY_MARGIN_MS) < READER_LEASE_MS
}

/// Milliseconds elapsed since `at`.
fn elapsed_ms(at: std::time::Instant) -> u64 {
    u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// How old a renewal requested at `requested` is, as far as the lease goes: a
/// lock that cannot expire under its holder never ages.
fn request_age_ms(expiring: bool, requested: std::time::Instant) -> u64 {
    if expiring {
        elapsed_ms(requested)
    } else {
        0
    }
}

/// Record a renewal that succeeded, as of when it was requested. Never moves the
/// record back: the worker and a commit can renew concurrently and return out of
/// order.
fn mark_renewed(
    last_renewed: &std::sync::Mutex<std::time::Instant>,
    requested: std::time::Instant,
) {
    let mut at = last_renewed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if requested > *at {
        *at = requested;
    }
}

/// Where a lease was found not to be fresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseSite {
    /// The background renewal worker.
    Worker,
    /// The renewal ahead of a write that relies on still holding the lock.
    Confirm,
}

/// Why a lease stopped being trusted: the first renewal that did not prove it
/// fresh, or belief lapsing while none answered.
///
/// Recorded so that a delivery that did not advance can say which, and so that
/// a transient renewal error, if one ever happens in production, is counted
/// rather than inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseLoss {
    /// The renewal returned an error `elapsed_ms` after it was requested.
    RenewFailed {
        site: LeaseSite,
        kind: std::io::ErrorKind,
        elapsed_ms: u64,
    },
    /// The backend answered that the lock is no longer held.
    NotHeld { site: LeaseSite, elapsed_ms: u64 },
    /// The backend answered that the lock is held, but too long after the
    /// request for that to count.
    LateSuccess { site: LeaseSite, elapsed_ms: u64 },
    /// No renewal answered in time: the last successful one was requested
    /// `since_ms` ago.
    Lapsed { since_ms: u64 },
}

impl std::fmt::Display for LeaseLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RenewFailed {
                site,
                kind,
                elapsed_ms,
            } => write!(
                f,
                "{site:?} renewal failed ({kind:?}) {elapsed_ms}ms after it was requested"
            ),
            Self::NotHeld { site, elapsed_ms } => write!(
                f,
                "{site:?} renewal found the lock no longer held {elapsed_ms}ms after it was requested"
            ),
            Self::LateSuccess { site, elapsed_ms } => write!(
                f,
                "{site:?} renewal succeeded {elapsed_ms}ms after it was requested, too late to count"
            ),
            Self::Lapsed { since_ms } => write!(
                f,
                "no renewal answered in time; the last successful one was requested {since_ms}ms ago"
            ),
        }
    }
}

/// What one renewal requested at `requested` says about the lease: `None` if
/// it proved the lease fresh.
fn renewal_loss(
    site: LeaseSite,
    result: &std::io::Result<bool>,
    expiring: bool,
    requested: std::time::Instant,
) -> Option<LeaseLoss> {
    let elapsed_ms = elapsed_ms(requested);
    match result {
        Err(e) => Some(LeaseLoss::RenewFailed {
            site,
            kind: e.kind(),
            elapsed_ms,
        }),
        Ok(false) => Some(LeaseLoss::NotHeld { site, elapsed_ms }),
        Ok(true) if !within_grace(request_age_ms(expiring, requested)) => {
            Some(LeaseLoss::LateSuccess { site, elapsed_ms })
        }
        Ok(true) => None,
    }
}

/// An exclusive backend lock, renewed on its own thread for as long as this
/// value lives.
///
/// Renewal is independent of what the holder is doing, because a model turn can
/// block for longer than the lease. A renewal proves ownership only as of when
/// it was requested: a success that comes back too late counts as lost, the
/// next renewal is timed from the last successful request, and belief in the
/// lease lapses on its own once that request is older than the grace allows.
/// Once a renewal fails the lease stays invalid and the holder must stop acting
/// on it. Dropping stops the renewal thread first and only then releases the
/// lock.
struct HeldLease {
    lock: Arc<dyn crate::fs_backend::FsLock>,
    stop: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    valid: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the backend can expire this lock under its holder (a TTL lease)
    /// rather than holding it until the holder exits. Only then does the age of
    /// a renewal matter.
    expiring: bool,
    /// When the last renewal that actually succeeded was *requested*, on the
    /// monotonic clock, shared by the worker and [`HeldLease::confirm`].
    last_renewed: Arc<std::sync::Mutex<std::time::Instant>>,
    /// The first reason this lease stopped being trusted, once it has.
    loss: Arc<std::sync::OnceLock<LeaseLoss>>,
    renewer: Option<std::thread::JoinHandle<()>>,
}

impl HeldLease {
    fn try_acquire(backend: &Arc<dyn FsBackend>, lock_path: &str) -> Result<Option<Self>, String> {
        if let Some((dir, _)) = lock_path.rsplit_once('/') {
            backend
                .create_dir_all(dir)
                .map_err(|e| format!("create directory {dir}: {e}"))?;
        }
        let requested = std::time::Instant::now();
        let Some(lock) = backend
            .try_lock(lock_path, READER_LEASE_MS)
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        // `fenced_locks` means the lock holds until its holder exits; anything
        // else is a lease the backend can expire under us.
        let expiring = !backend.fenced_locks();
        if !within_grace(request_age_ms(expiring, requested)) {
            // The grant came back too late to act on. Release it (dropping the
            // lock does) rather than start delivering on a lease that may
            // already be near expiry; the caller retries as for a held seat.
            return Ok(None);
        }
        let stop = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let valid = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let last_renewed = Arc::new(std::sync::Mutex::new(requested));
        let loss = Arc::new(std::sync::OnceLock::new());
        let worker_stop = Arc::clone(&stop);
        let worker_valid = Arc::clone(&valid);
        let worker_lock = Arc::clone(&lock);
        let worker_last_renewed = Arc::clone(&last_renewed);
        let worker_loss = Arc::clone(&loss);
        let renewer = std::thread::Builder::new()
            .name("mailbox-lease".into())
            .spawn(move || {
                let (mutex, wake) = &*worker_stop;
                // Every renewal is timed from when the last successful one was
                // requested, never from when its reply came back: a slow reply
                // must not push the next renewal past the TTL.
                let mut wait_ms = READER_RENEW_MS.saturating_sub(elapsed_ms(requested));
                loop {
                    let stopped = mutex
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let (stopped, _) = wake
                        .wait_timeout_while(
                            stopped,
                            std::time::Duration::from_millis(wait_ms),
                            |stopped| !*stopped,
                        )
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if *stopped {
                        break;
                    }
                    drop(stopped);
                    let requested = std::time::Instant::now();
                    let result = worker_lock.renew();
                    if let Some(lost) =
                        renewal_loss(LeaseSite::Worker, &result, expiring, requested)
                    {
                        let _ = worker_loss.set(lost);
                        worker_valid.store(false, std::sync::atomic::Ordering::Release);
                        break;
                    }
                    mark_renewed(&worker_last_renewed, requested);
                    wait_ms = READER_RENEW_MS.saturating_sub(elapsed_ms(requested));
                }
            })
            .map_err(|e| format!("start lease renewal: {e}"))?;
        Ok(Some(Self {
            lock,
            stop,
            valid,
            expiring,
            last_renewed,
            loss,
            renewer: Some(renewer),
        }))
    }

    /// Whether the holder may still act on this lease.
    ///
    /// The flag records a renewal that failed; the age check covers one that has
    /// not answered at all. While a renewal is in flight the worker cannot clear
    /// the flag, so belief must also lapse on its own once the last successful
    /// renewal was requested longer ago than the grace allows, and then it
    /// stays lapsed.
    fn is_valid(&self) -> bool {
        if !self.valid.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let since_ms = request_age_ms(
            self.expiring,
            *self
                .last_renewed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if within_grace(since_ms) {
            return true;
        }
        // Lapsing is final, like a failed renewal: a success that lands later
        // must not restore belief the holder has already acted on losing.
        let _ = self.loss.set(LeaseLoss::Lapsed { since_ms });
        self.valid
            .store(false, std::sync::atomic::Ordering::Release);
        false
    }

    /// The first reason this lease stopped being trusted, if it has. Set
    /// exactly when [`HeldLease::is_valid`] turns false, and before it does.
    fn loss(&self) -> Option<LeaseLoss> {
        self.loss.get().copied()
    }

    /// Renew now, ahead of a write that relies on still holding the lock.
    /// `Ok(false)` means it was lost and marks the lease invalid for good; a
    /// success that took too long to come back counts as lost too. An `Err`
    /// refuses the write and ends the lease the same way, as a failed renewal
    /// in the worker does: every caller lets go of a lease whose write was
    /// refused, and a lease left valid after a failed renewal would carry a
    /// reason for a loss that had not happened.
    fn confirm(&self) -> std::io::Result<bool> {
        if !self.is_valid() {
            return Ok(false);
        }
        let requested = std::time::Instant::now();
        let result = self.lock.renew();
        let lost = renewal_loss(LeaseSite::Confirm, &result, self.expiring, requested);
        if let Some(lost) = lost {
            let _ = self.loss.set(lost);
            self.valid
                .store(false, std::sync::atomic::Ordering::Release);
        }
        result?;
        let held = lost.is_none();
        if held {
            mark_renewed(&self.last_renewed, requested);
        }
        Ok(held)
    }
}

impl Drop for HeldLease {
    fn drop(&mut self) {
        let (mutex, wake) = &*self.stop;
        *mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_all();
        if let Some(renewer) = self.renewer.take() {
            let _ = renewer.join();
        }
        // The lock is released only after the renewal worker has stopped.
    }
}

struct OwnedReader {
    lease: HeldLease,
    cursor: crate::cursor_fence::RegisterCursor,
    offset: u64,
}

impl OwnedReader {
    fn is_valid(&self) -> bool {
        self.lease.is_valid()
    }

    fn commit(&mut self, offset: u64) -> Result<(), CommitRefusal> {
        let confirmed = self.lease.confirm();
        if !matches!(confirmed, Ok(true)) {
            // Read the loss now: it is what refused this commit. The lease
            // records it before it refuses, and only the first one is kept.
            let message = match confirmed {
                Err(e) => format!("renew reader: {e}"),
                Ok(_) => "reader ownership lost; delivery remains unacknowledged".into(),
            };
            return Err(CommitRefusal {
                loss: self.lease.loss(),
                message,
            });
        }
        // The lease held. A write that fails now is the register's failure,
        // even if the lease is lost while it is in flight.
        self.cursor
            .commit(offset, now_ms() + READER_LEASE_MS)
            .map_err(|message| CommitRefusal {
                loss: None,
                message,
            })?;
        self.offset = offset;
        Ok(())
    }
}

/// Why a reader commit did not happen. `loss` is the lease's loss when the
/// renewal ahead of the write refused it, and `None` when the lease held and
/// the register write failed.
struct CommitRefusal {
    loss: Option<LeaseLoss>,
    message: String,
}

impl std::fmt::Display for CommitRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
/// A pending delivery. Only acknowledge after the consumer durably records its
/// successful turn; filtering or enqueueing alone does not consume the batch.
pub struct UnreadBatch {
    messages: Vec<MailboxEnvelope>,
    next: u64,
    reader: OwnedReader,
}

impl UnreadBatch {
    #[must_use]
    pub fn messages(&self) -> &[MailboxEnvelope] {
        &self.messages
    }
    pub fn acknowledge(mut self) -> Result<(), String> {
        self.reader
            .commit(self.next)
            .map_err(|refusal| refusal.message)
    }
}

/// Pending deliveries sharing one acknowledgement boundary.
#[derive(Default)]
pub struct PendingMessages {
    batches: Vec<UnreadBatch>,
}

impl PendingMessages {
    pub fn messages(&self) -> impl Iterator<Item = &MailboxEnvelope> {
        self.batches.iter().flat_map(UnreadBatch::messages)
    }
    pub fn acknowledge(self) -> Result<(), String> {
        for batch in self.batches {
            batch.acknowledge()?;
        }
        Ok(())
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
            backend: self.backend.clone(),
            holder: instance_id(),
            agent: self.self_id.clone(),
        }
    }

    /// Claim this agent's one receiver seat, `agents/<self>/receiver.lock`.
    ///
    /// `None` while another instance of the same agent holds it. Without this
    /// seat, two instances of one agent each claim whichever conversations they
    /// reach first, and one agent's messages end up answered by two sessions.
    fn claim_receiver(&self) -> Result<Option<HeldLease>, String> {
        let path = format!(
            "{}/{}/receiver.lock",
            self.convention.agents_dir(),
            self.self_id
        );
        HeldLease::try_acquire(&self.backend.resolve()?, &path).map_err(|e| {
            format!(
                "claim the receiver seat for {} at {path}: {e}",
                self.self_id
            )
        })
    }
}

/// What a receiver in this process is doing for its agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverState {
    /// Configuration or transport IO has not made the receiver usable yet.
    Unavailable,
    /// Another instance of the agent holds its receiver seat; this one waits to
    /// take over.
    Standby,
    /// This instance holds the agent's receiver seat and tails its
    /// conversations.
    Receiving,
}

impl ReceiverState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Standby => "standby",
            Self::Receiving => "receiving",
        }
    }
}

/// Where a session's peers are and whether this process receives for it: what
/// `/status` and an ACP client are told.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PeerStatus {
    /// The agent name the session answers to.
    pub agent: String,
    /// The selected `"node"` or `"disk"` namespace, even when unavailable.
    pub namespace: Option<&'static str>,
    /// The node's `host:port`, when attached to one.
    pub endpoint: Option<String>,
    /// `"receiving"`, `"standby"`, `"unavailable"`, or `"none"` when this
    /// process runs no receiver for the agent.
    pub receiver: &'static str,
    /// When a received message runs: `"automatic"` as soon as the session is
    /// idle, or `"next_prompt"` in the synchronous REPL, which runs it once the
    /// user hands the prompt back.
    pub delivery: &'static str,
    /// What keeps a resumed holder from moving a read position back:
    /// `"fenced"`, `"fenced_cas"` or `"at_least_once"` (see
    /// [`crate::cursor_fence::CursorGuarantee`]), or `"none"` until this
    /// process has claimed a conversation for the agent.
    pub cursor: &'static str,
    /// The last local node-binding error, when it could not yet be resolved.
    pub error: Option<String>,
    /// Most recent background receiver failure in this process. Historical:
    /// receiving can recover while this diagnostic remains inspectable.
    pub last_receiver_error: Option<String>,
}

/// The cursor guarantee each agent's readers got in this process, as decided
/// when a conversation was last claimed.
static CURSOR_GUARANTEES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, crate::cursor_fence::CursorGuarantee>>,
> = std::sync::OnceLock::new();

fn record_cursor_guarantee(agent: &str, guarantee: crate::cursor_fence::CursorGuarantee) {
    CURSOR_GUARANTEES
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(agent.to_string(), guarantee);
}

/// What protects `agent`'s read positions in this process: `None` until one of
/// its conversations has been claimed here.
#[must_use]
pub fn cursor_guarantee(agent: &str) -> Option<crate::cursor_fence::CursorGuarantee> {
    CURSOR_GUARANTEES
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(agent)
        .copied()
}

#[derive(Default)]
struct ReceiverObservation {
    last_error: Option<String>,
    unavailable: usize,
    standby: usize,
    receiving: usize,
}

impl ReceiverObservation {
    #[inline]
    fn count_mut(&mut self, state: ReceiverState) -> &mut usize {
        match state {
            ReceiverState::Unavailable => &mut self.unavailable,
            ReceiverState::Standby => &mut self.standby,
            ReceiverState::Receiving => &mut self.receiving,
        }
    }
}

/// Receiver state in this process, per agent. These are runtime observations,
/// not persisted delivery positions or proof that a remote peer is online.
static RECEIVER_STATES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, ReceiverObservation>>,
> = std::sync::OnceLock::new();

fn receiver_states(
) -> std::sync::MutexGuard<'static, std::collections::HashMap<String, ReceiverObservation>> {
    RECEIVER_STATES
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What this process does to receive as `agent`: `None` when it runs no
/// receiver for that agent. Receiving outranks standby when it runs several.
#[must_use]
pub fn receiver_state(agent: &str) -> Option<ReceiverState> {
    match receiver_states().get(agent) {
        Some(counts) if counts.receiving > 0 => Some(ReceiverState::Receiving),
        Some(counts) if counts.standby > 0 => Some(ReceiverState::Standby),
        Some(counts) if counts.unavailable > 0 => Some(ReceiverState::Unavailable),
        _ => None,
    }
}

/// Last background failure, retained with the receiver's transient state.
#[must_use]
pub fn last_receiver_error(agent: &str) -> Option<String> {
    receiver_states()
        .get(agent)
        .and_then(|state| state.last_error.clone())
}

/// Background services never own the terminal. Hosts can inspect the same
/// latest diagnostic through peer status or subscribe to structured tracing.
fn report_receiver_error(mailbox: &Mailbox, label: &str, error: String) {
    tracing::warn!(receiver = label, agent = mailbox.self_id(), %error, "mailbox receiver failed");
    receiver_states()
        .entry(mailbox.self_id().to_string())
        .or_default()
        .last_error = Some(error);
}

/// One operational observation feeds the existing status/tracing owner and
/// the shared retry policy. Neither consumer changes the operation's result.
#[inline]
fn observe_receiver_failure(
    mailbox: &Mailbox,
    diagnostics: &telemetry::RetryDiagnostics,
    scope: &str,
    error: &str,
) {
    report_receiver_error(
        mailbox,
        diagnostics.component(),
        format!("{scope}: {error}"),
    );
    diagnostics.failed(scope, error);
}

/// One receiver's entry in [`receiver_state`], withdrawn when it stops.
struct ReceiverStateEntry {
    agent: String,
    state: Option<ReceiverState>,
}

impl ReceiverStateEntry {
    fn set(&mut self, state: ReceiverState) {
        if self.state == Some(state) {
            return;
        }
        let mut states = receiver_states();
        let counts = states.entry(self.agent.clone()).or_default();
        if let Some(previous) = self.state.replace(state) {
            *counts.count_mut(previous) -= 1;
        }
        *counts.count_mut(state) += 1;
    }
}

impl Drop for ReceiverStateEntry {
    fn drop(&mut self) {
        let mut states = receiver_states();
        if let Some(counts) = states.get_mut(&self.agent) {
            if let Some(previous) = self.state {
                *counts.count_mut(previous) -= 1;
            }
            if counts.unavailable == 0 && counts.standby == 0 && counts.receiving == 0 {
                states.remove(&self.agent);
            }
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
///
/// An agent has one receiver at a time, wherever it runs. The receiver tails
/// nothing until it holds the agent's receiver seat ([`Mailbox::claim_receiver`]);
/// until then it stands by and retries, so a second instance of the agent takes
/// over when the first stops. Each conversation's reader seat still guards that
/// conversation's read position, so a tail finishing a delivery while the agent
/// seat changes hands cannot be read twice. [`receiver_state`] reports which of
/// the two this process is doing.
#[inline]
pub fn spawn_inbox_poller(
    mailbox: Arc<Mailbox>,
    block_ms: u64,
    label: &'static str,
    abort: crate::HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + Sync + 'static,
) -> std::thread::JoinHandle<()> {
    spawn_inbox_poller_with_notices(
        mailbox,
        block_ms,
        label,
        abort,
        telemetry::RetryNotices::LogOnly,
        sink,
    )
}

/// The same receiver for a host which explicitly owns retry presentation.
/// Terminal notices require a host-owned capture/printer; ordinary receivers
/// expose errors through their existing status, tracing and local log instead.
pub fn spawn_inbox_poller_with_notices(
    mailbox: Arc<Mailbox>,
    block_ms: u64,
    label: &'static str,
    abort: crate::HookAbortSignal,
    notices: telemetry::RetryNotices,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + Sync + 'static,
) -> std::thread::JoinHandle<()> {
    let sink: Arc<dyn Fn(&MailboxEnvelope) -> bool + Send + Sync> = Arc::new(sink);
    std::thread::Builder::new()
        .name(format!("{label}-inbox"))
        .spawn(move || {
            // Credential or node availability may arrive after startup. Keep
            // retrying the same mailbox, including its presence announcement,
            // before claiming the receiver seat. No second receiver is needed.
            let mut announced = false;
            let mut state = ReceiverStateEntry {
                agent: mailbox.self_id().to_string(),
                state: None,
            };
            let diagnostics = Arc::new(
                telemetry::RetryDiagnostics::new(
                    label,
                    mailbox.self_id(),
                    "Message receiving is interrupted; retrying in the background.",
                    "Message receive checks recovered.",
                )
                .with_notices(notices),
            );
            while !abort.is_aborted() {
                state.set(ReceiverState::Unavailable);
                if !announced {
                    if let Err(e) = mailbox.ensure_presence() {
                        observe_receiver_failure(
                            &mailbox,
                            &diagnostics,
                            "presence announcement",
                            &e,
                        );
                        sleep_unless_aborted(&abort, CONVERSATION_DISCOVERY_MS);
                        continue;
                    }
                    announced = true;
                    diagnostics.succeeded("presence announcement");
                }
                let seat = match mailbox.claim_receiver() {
                    Ok(Some(seat)) => seat,
                    Ok(None) => {
                        state.set(ReceiverState::Standby);
                        diagnostics.succeeded("receiver seat");
                        sleep_unless_aborted(&abort, CONVERSATION_DISCOVERY_MS);
                        continue;
                    }
                    Err(e) => {
                        observe_receiver_failure(&mailbox, &diagnostics, "receiver seat", &e);
                        sleep_unless_aborted(&abort, CONVERSATION_DISCOVERY_MS);
                        continue;
                    }
                };
                diagnostics.succeeded("receiver seat");
                state.set(ReceiverState::Receiving);
                receive_while_seated(&mailbox, &seat, block_ms, &diagnostics, &abort, &sink);
            }
        })
        .expect("spawn inbox receiver thread")
}

/// Discover and tail this agent's conversations for as long as `seat` holds.
///
/// Returns once the receiver is aborted or the seat is lost, after every tail
/// it started has stopped: a tail outliving its seat would keep delivering for
/// an agent this instance no longer receives for.
fn receive_while_seated(
    mailbox: &Arc<Mailbox>,
    seat: &HeldLease,
    block_ms: u64,
    diagnostics: &Arc<telemetry::RetryDiagnostics>,
    abort: &crate::HookAbortSignal,
    sink: &Arc<dyn Fn(&MailboxEnvelope) -> bool + Send + Sync>,
) {
    let tails_stop = crate::HookAbortSignal::new();
    let mut tailing: std::collections::HashMap<String, std::thread::JoinHandle<()>> =
        std::collections::HashMap::new();
    while !abort.is_aborted() && seat.is_valid() {
        match mailbox.list_conversations() {
            Ok(peers) => {
                diagnostics.succeeded("conversation discovery");
                for peer in peers {
                    if tailing.contains_key(&peer) {
                        continue;
                    }
                    let tail = spawn_conversation_tail(
                        Arc::clone(mailbox),
                        peer.clone(),
                        block_ms,
                        Arc::clone(diagnostics),
                        tails_stop.clone(),
                        Arc::clone(sink),
                    );
                    tailing.insert(peer, tail);
                }
            }
            Err(e) => observe_receiver_failure(mailbox, diagnostics, "conversation discovery", &e),
        }
        sleep_unless_aborted(abort, CONVERSATION_DISCOVERY_MS);
    }
    if let Some(loss) = seat.loss() {
        report_receiver_error(
            mailbox,
            diagnostics.component(),
            format!("receiver seat lost: {loss}"),
        );
    }
    // Joining is what makes the receiver's handle mean "the receiver has
    // stopped". Without it the caller's join returns while N tails are still
    // delivering into a sink it believes is finished with.
    tails_stop.abort();
    for (_, tail) in tailing {
        let _ = tail.join();
    }
}
/// One conversation's tail: hold the reader seat, park on the transcript,
/// deliver, commit.
/// Why a delivery attempt left the reader where it was.
///
/// Recorded because the interesting case was previously invisible. Three of
/// these only reached an `eprintln!` on a stderr nobody captures, and
/// [`DeliveryStall::ConsumerRefused`] — the one that actually re-hands a batch
/// to a consumer and so produces duplicate deliveries — was logged nowhere at
/// all. Six agents then had to argue from inference about whether a repeat was
/// transport or a peer saying something twice, which is the distinction
/// [`MailboxEnvelope::timestamp`] is documented to settle but which nothing
/// persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStall {
    /// The consumer did not accept every message in the batch, so the whole
    /// batch is re-read from the same offset. This is the DESIGNED
    /// at-least-once boundary, not a fault — it is recorded so that a repeat
    /// can be attributed instead of guessed at.
    ///
    /// Recorded only when the lease was still valid, so this really is the
    /// consumer's refusal. The batch test is
    /// `msgs.iter().all(|m| reader.is_valid() && sink(m))`, which also goes
    /// false when the lease lapses mid-batch; attributing that to the consumer
    /// would blame the wrong cause for exactly the repeats this record exists
    /// to explain — and the "not a fault" note above would then be asserted
    /// about a fault. That case is [`DeliveryStall::LeaseLost`].
    ConsumerRefused,
    /// The lease went invalid part-way through the batch, so the handover was
    /// abandoned and the batch is re-read. Distinct from
    /// [`DeliveryStall::ConsumerRefused`] because this one IS a fault, and
    /// because a repeat caused by it has a different remedy.
    LeaseLost,
    /// The batch was accepted but `commit` refused to store the new offset, so
    /// the accepted work will be handed over again.
    CommitRefused,
    /// The reader could not be claimed at all.
    ClaimFailed,
    /// Reading the conversation failed.
    PollFailed,
}

/// One delivery attempt that did not advance a reader.
///
/// Carries only what attributes the stall — peer, holder, offset — and never
/// message bodies: the transcript already holds those, and a diagnostic has no
/// business copying them.
#[derive(Debug, Clone)]
pub struct DeliveryEvent {
    pub peer: String,
    pub holder: String,
    /// The offset that did NOT advance, or `None` when no reader was held and
    /// there is no offset to report.
    ///
    /// `Option` for the same reason as `tail`: offset **0** is a real and common
    /// value — the start of a conversation, which the integration tests assert
    /// on — so a zero standing for "unknown" would be indistinguishable from a
    /// genuine stall at the first frame. [`DeliveryStall::ClaimFailed`] happens
    /// before a reader exists, so it has no offset rather than an invented one.
    pub offset: Option<u64>,
    /// End of the transcript as of the poll that observed this stall, or `None`
    /// when no poll succeeded and nothing beyond the cursor is known.
    ///
    /// This is the field that makes the record self-sufficient. `offset` alone
    /// cannot tell a re-hand from a peer speaking again: while a consumer keeps
    /// refusing, the offset stays put in BOTH cases, so both look identical on
    /// `offset` and `repeats`. `tail` separates them — it is unchanged when the
    /// same frames are handed over again, and larger when the peer appended
    /// something new.
    ///
    /// `Option` rather than a sentinel because `Some(tail) == Some(offset)`
    /// already means something definite — the reader is caught up, nothing
    /// pending. Encoding "unknown" the same way would make "no backlog" and
    /// "could not see the backlog" indistinguishable, which is the exact
    /// ambiguity this field exists to remove. A type keeps them apart for every
    /// reader; a doc comment only helps the ones who read it.
    pub tail: Option<u64>,
    pub stall: DeliveryStall,
    /// Why the lease stopped being trusted, when that is what stopped this
    /// delivery.
    ///
    /// - [`DeliveryStall::LeaseLost`]: always `Some`. The lease records its
    ///   loss before it reads invalid, so a `LeaseLost` with `None` here is a
    ///   bug, not a missing measurement.
    /// - [`DeliveryStall::CommitRefused`]: `Some` when the commit was refused
    ///   because the lease was lost — the renewal ahead of the write returned
    ///   an error, answered "not held" or answered too late, or the lease was
    ///   already lost — captured the moment the renewal refused it. `None` when
    ///   the lease held and the register write itself failed, even if the
    ///   lease is lost while that write is in flight: the write is what
    ///   refused the commit.
    /// - Every other stall: `None` **by construction**, not by observation. A
    ///   consumer refusal is classified at the moment it happens, and the lease
    ///   may lapse a moment later; attaching that later loss would give the
    ///   refusal a cause it did not have — the misattribution `stall` exists to
    ///   prevent.
    pub loss: Option<LeaseLoss>,
    /// Consecutive stalls at the current `offset` within one reader claim.
    ///
    /// Reset when a batch is accepted, and started afresh when the reader is
    /// re-acquired — so it never carries a stale count across a claim or back
    /// to an earlier offset.
    pub repeats: u32,
    pub at_ms: u64,
}

/// Bounded so a long-lived agent cannot accumulate these without limit.
const DELIVERY_EVENT_CAP: usize = 256;

/// A bounded record of delivery stalls.
///
/// A type rather than loose statics so it can be exercised in isolation: the
/// process-wide instance is just one of these, and a test gets its own instead
/// of racing its peers through shared state.
#[derive(Default)]
struct DeliveryLog {
    events: std::sync::Mutex<std::collections::VecDeque<DeliveryEvent>>,
}

impl DeliveryLog {
    /// Infallible and side-effect-free with respect to delivery: recording must
    /// never influence whether a batch is acknowledged.
    fn record(&self, event: DeliveryEvent) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if events.len() == DELIVERY_EVENT_CAP {
            events.pop_front();
        }
        events.push_back(event);
    }

    fn events(&self) -> Vec<DeliveryEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }
}

fn global_delivery_log() -> &'static DeliveryLog {
    static LOG: std::sync::OnceLock<DeliveryLog> = std::sync::OnceLock::new();
    LOG.get_or_init(DeliveryLog::default)
}

impl DeliveryEvent {
    #[inline]
    fn new(
        peer: &str,
        holder: &str,
        offset: Option<u64>,
        tail: Option<u64>,
        stall: DeliveryStall,
        loss: Option<LeaseLoss>,
        repeats: u32,
    ) -> Self {
        Self {
            peer: peer.to_string(),
            holder: holder.to_string(),
            offset,
            tail,
            stall,
            loss,
            repeats,
            at_ms: now_ms(),
        }
    }

    /// All consumers receive the facts captured at the original decision.
    /// A refused consumer or lost lease is not a transport reconnect warning.
    #[inline]
    fn observe(
        self,
        mailbox: &Mailbox,
        diagnostics: &telemetry::RetryDiagnostics,
        scope: &str,
        error: Option<&str>,
    ) {
        let observation = error.map(|error| {
            let operation = match self.stall {
                DeliveryStall::ClaimFailed => "claim",
                DeliveryStall::PollFailed => "poll",
                DeliveryStall::CommitRefused => "commit",
                DeliveryStall::ConsumerRefused => "consumer",
                DeliveryStall::LeaseLost => "lease",
            };
            // Keep the captured cause in status even when expiry/transfer is
            // not an I/O outage. Never re-query the lease after the refusal.
            let context = if matches!(
                self.loss,
                Some(
                    LeaseLoss::NotHeld { .. }
                        | LeaseLoss::LateSuccess { .. }
                        | LeaseLoss::Lapsed { .. }
                )
            ) {
                None
            } else {
                Some(serde_json::Map::from_iter([
                    ("peer".into(), self.peer.clone().into()),
                    ("holder".into(), self.holder.clone().into()),
                    ("operation".into(), operation.into()),
                    ("offset".into(), self.offset.into()),
                    ("tail".into(), self.tail.into()),
                    ("stall".into(), format!("{:?}", self.stall).into()),
                    ("loss".into(), self.loss.map(|loss| loss.to_string()).into()),
                    ("repeats".into(), self.repeats.into()),
                ]))
            };
            (error, format!("{operation} {scope}: {error}"), context)
        });
        global_delivery_log().record(self);
        if let Some((error, status, context)) = observation {
            report_receiver_error(mailbox, diagnostics.component(), status);
            if let Some(context) = context {
                diagnostics.failed_with(scope, error, context);
            }
        }
    }
}

/// Delivery stalls observed by this process, oldest first.
///
/// Process-scoped by design: the question being answered is "what is happening
/// on this receiver", and draining to disk from the tail thread would add a new
/// failure mode to the delivery path for a diagnostic's sake.
#[must_use]
pub fn delivery_events() -> Vec<DeliveryEvent> {
    global_delivery_log().events()
}

fn spawn_conversation_tail(
    mailbox: Arc<Mailbox>,
    peer: String,
    block_ms: u64,
    diagnostics: Arc<telemetry::RetryDiagnostics>,
    abort: crate::HookAbortSignal,
    sink: Arc<dyn Fn(&MailboxEnvelope) -> bool + Send + Sync>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("{}-tail", diagnostics.component()))
        .spawn(move || {
            let scope = format!("conversation with {peer}");
            while !abort.is_aborted() {
                let mut reader = match mailbox.conversation_reader(&peer).acquire() {
                    Ok(Some(reader)) => reader,
                    Ok(None) => {
                        sleep_unless_aborted(&abort, CONVERSATION_DISCOVERY_MS);
                        continue;
                    }
                    Err(e) => {
                        DeliveryEvent::new(
                            &peer,
                            "",
                            None,
                            None,
                            DeliveryStall::ClaimFailed,
                            None,
                            0,
                        )
                        .observe(&mailbox, &diagnostics, &scope, Some(&e));
                        sleep_unless_aborted(&abort, block_ms.max(1));
                        continue;
                    }
                };
                let mut retry_delay_ms = DELIVERY_RETRY_INITIAL_MS;
                let mut stall_repeats = 0_u32;
                while !abort.is_aborted() && reader.is_valid() {
                    let (msgs, next) =
                        match mailbox.poll_conversation(&peer, reader.offset, block_ms) {
                            Ok(batch) => batch,
                            Err(e) => {
                                DeliveryEvent::new(
                                    &peer,
                                    reader.cursor.holder(),
                                    Some(reader.offset),
                                    None,
                                    DeliveryStall::PollFailed,
                                    None,
                                    0,
                                )
                                .observe(
                                    &mailbox,
                                    &diagnostics,
                                    &scope,
                                    Some(&e),
                                );
                                sleep_unless_aborted(&abort, block_ms.max(1));
                                continue;
                            }
                        };
                    let mut lease_lost = false;
                    if msgs.iter().all(|m| {
                        // Record which step failed as it happens. Asking
                        // `is_valid()` again afterwards would be a second
                        // question with a window between: a sink that refused
                        // under a held lease, followed by the lease lapsing
                        // before the check, would be filed as `LeaseLost` when
                        // the consumer was the actual cause.
                        if !reader.is_valid() {
                            lease_lost = true;
                            return false;
                        }
                        sink(m)
                    }) {
                        retry_delay_ms = DELIVERY_RETRY_INITIAL_MS;
                        stall_repeats = 0;
                        if next > reader.offset {
                            if let Err(e) = reader.commit(next) {
                                DeliveryEvent::new(
                                    &peer,
                                    reader.cursor.holder(),
                                    Some(reader.offset),
                                    Some(next),
                                    DeliveryStall::CommitRefused,
                                    e.loss,
                                    0,
                                )
                                .observe(
                                    &mailbox,
                                    &diagnostics,
                                    &scope,
                                    Some(&e.message),
                                );
                                break;
                            }
                        }
                    } else {
                        // The batch is re-read from this same offset, so every
                        // message in it is handed over again. Recorded: this is
                        // the path that produces a byte-identical redelivery,
                        // and it used to leave no trace whatsoever.
                        if !msgs.is_empty() {
                            stall_repeats = stall_repeats.saturating_add(1);
                            // Which step actually failed, captured above rather
                            // than re-derived here. Only a lost lease carries
                            // the lease's loss; see `DeliveryEvent::loss`.
                            let (stall, loss) = if lease_lost {
                                (DeliveryStall::LeaseLost, reader.lease.loss())
                            } else {
                                (DeliveryStall::ConsumerRefused, None)
                            };
                            DeliveryEvent::new(
                                &peer,
                                reader.cursor.holder(),
                                Some(reader.offset),
                                Some(next),
                                stall,
                                loss,
                                stall_repeats,
                            )
                            .observe(
                                &mailbox,
                                &diagnostics,
                                &scope,
                                None,
                            );
                        }
                        sleep_unless_aborted(&abort, retry_delay_ms);
                        retry_delay_ms = retry_delay_ms.saturating_mul(2).min(READER_RENEW_MS);
                    }
                    diagnostics.succeeded(&scope);
                }
                if let Some(loss) = reader.lease.loss() {
                    report_receiver_error(
                        &mailbox,
                        diagnostics.component(),
                        format!("reader for {peer} lost: {loss}"),
                    );
                }
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
    /// Enter `mailbox` as this thread's peer namespace until the guard drops.
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

/// The mailbox to send through and enumerate: the session's peer namespace
/// this thread is scoped onto, else workspace-local JSONL.
///
/// Two levels, and the second is the contract rather than a fallback: a session
/// hands its peer mailbox to its dispatcher, which scopes the thread running the
/// tool; a caller with no session scope (a sub-agent, a test) delivers to
/// `.sudocode-inbox/` in its workspace through this same call. Being one code
/// path is what stops the two drifting.
///
/// The fallback's identity is derived rather than left empty: a conversation is
/// addressed by its PAIR, so a nameless side has no conversation to be in and
/// the chat-list entry would come out as `…/conversations/` with no leaf.
/// `local_agent_name` is the derivation the host uses for its own scoped
/// mailbox, so the two agree instead of nearly agreeing.
#[must_use]
pub fn sending_mailbox() -> Arc<Mailbox> {
    if let Some(mailbox) = SCOPED_MAILBOX.with(|cell| cell.borrow().clone()) {
        return mailbox;
    }
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
                delivery: crate::turn_input::InputDelivery::Steer,
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
    fn list_recipients_skips_non_agent_entries() {
        // A mount can expose infrastructure siblings under `/agents` (e.g. `raft`,
        // `sm` on a co-host topology). They are directories but never announced
        // presence, so they have no `conversations` child and must not be listed
        // as peers.
        let ws = temp_workspace("list-recip-shape");
        let mb = local_mailbox(&ws, "coordinator");
        mb.send(MailboxEnvelope {
            delivery: crate::turn_input::InputDelivery::Steer,
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

        // Plant a non-agent dir directly in the agents namespace, plus a stray
        // file — neither has a `conversations` child.
        let agents_dir = mb.convention.agents_dir();
        std::fs::create_dir_all(format!("{agents_dir}/raft")).unwrap();
        std::fs::create_dir_all(format!("{agents_dir}/sm")).unwrap();
        std::fs::write(format!("{agents_dir}/stray-file"), b"x").unwrap();

        let names = mb.list_recipients().unwrap();
        assert_eq!(
            names,
            vec!["alpha", "coordinator"],
            "only presence-announced agents are recipients; infra dirs are skipped"
        );

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[cfg(windows)]
    #[test]
    fn workspace_mailbox_accepts_verbatim_windows_root() {
        let root = std::env::temp_dir().join(format!("mailbox-verbatim-{}", std::process::id()));
        let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", root.display()));
        let sender = Mailbox::workspace_local(&verbatim, "parent".to_string());
        let receiver = Mailbox::workspace_local(&verbatim, "worker".to_string());
        sender
            .send(note("parent", "worker", "follow-up"))
            .expect("a verbatim root must support creating chat-list paths");
        let unread = receiver
            .read_unread("parent")
            .expect("worker receives mail")
            .expect("reader available");
        assert_eq!(unread.messages().len(), 1);
        assert_eq!(unread.messages()[0].body, "follow-up");
        unread.acknowledge().unwrap();
        let _ = std::fs::remove_dir_all(root);
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
    fn strip_windows_verbatim_prefix_collapses_spellings() {
        // The fix itself: the normalized string is spelling-independent. Tested
        // directly (not through `local_agent_name`) so it is deterministic on
        // every platform — `Path::file_name` treats `\` as a separator only on
        // Windows, so the end-to-end name equality is asserted under cfg(windows).
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\C:\Users\me\proj\app"),
            r"C:\Users\me\proj\app",
            "\\\\?\\ prefix must be stripped to the plain spelling"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share\app"),
            r"\\server\share\app",
            "\\\\?\\UNC\\ must fold back to the plain UNC spelling"
        );
        // A plain path is returned unchanged.
        assert_eq!(
            strip_windows_verbatim_prefix(r"C:\Users\me\proj\app"),
            r"C:\Users\me\proj\app"
        );
        assert_eq!(
            strip_windows_verbatim_prefix("/home/me/app"),
            "/home/me/app"
        );
    }

    #[cfg(windows)]
    #[test]
    fn agent_name_ignores_windows_verbatim_prefix() {
        let plain = local_agent_name(None, std::path::Path::new(r"C:\Users\me\proj\app"));
        let verbatim = local_agent_name(None, std::path::Path::new(r"\\?\C:\Users\me\proj\app"));
        assert_eq!(
            plain, verbatim,
            "\\\\?\\ and plain spellings must yield the same name"
        );
        let unc_plain = local_agent_name(None, std::path::Path::new(r"\\server\share\app"));
        let unc_verbatim =
            local_agent_name(None, std::path::Path::new(r"\\?\UNC\server\share\app"));
        assert_eq!(
            unc_plain, unc_verbatim,
            "UNC verbatim and plain must match too"
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
            delivery: crate::turn_input::InputDelivery::Steer,
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
