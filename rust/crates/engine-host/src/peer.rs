//! The session's one peer namespace, for `scode`.
//!
//! A machine has exactly one place its agents meet: the nexus node when this
//! process is attached to one, otherwise the same-machine pair root on local
//! disk. [`attach`] is the only place that decides which, so a recipient name
//! means the same destination to the `send` tool, to `agent_list`, and to the
//! receiver tailing this session's inbox.
//!
//! Both sides are the unified [`runtime::mailbox::Mailbox`]: identical
//! conversations, addressing and receiver over different backends. A node is
//! reached through [`NexusVfsFsBackend`]; the pair root through the host
//! filesystem. A co-hosted agent never calls this: its daemon injects the
//! mailbox it lives in.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::NexusVfsFsBackend;
use runtime::mailbox::Mailbox;
use runtime::nexus_mailbox::Config;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration on the node.
const INBOX_WAIT_MS: u64 = 500;

/// A configured node session for one agent. Connectivity is established by IO.
pub struct Node {
    config: Config,
    mailbox: Arc<Mailbox>,
}

/// Where this session's peers are.
pub enum Peer {
    Node(Arc<Node>),
    /// The same-machine pair root, as the session's own agent name.
    Disk(String),
}

impl Peer {
    /// The mailbox `send` and `agent_list` use.
    #[must_use]
    pub fn mailbox(&self) -> Arc<Mailbox> {
        match self {
            Self::Node(node) => Arc::clone(&node.mailbox),
            Self::Disk(name) => Arc::new(Mailbox::workspace_local(
                &runtime::mailbox::local_pair_root(),
                name.clone(),
            )),
        }
    }

    /// The system-prompt section that teaches the model its identity and how
    /// to reply on this namespace.
    #[must_use]
    pub fn prompt_section(&self) -> String {
        match self {
            Self::Node(node) => node.config.peer_system_prompt(),
            Self::Disk(name) => runtime::agent_mailbox::repl_a2a_prompt_section(name),
        }
    }

    #[must_use]
    pub fn is_node(&self) -> bool {
        matches!(self, Self::Node(_))
    }

    /// Start the receiver for this session's inbox.
    ///
    /// The loop is [`runtime::mailbox::spawn_inbox_poller`], shared with the
    /// co-hosted agent. Nothing is provisioned first: a conversation is
    /// provisioned by whichever side sends first, and its read position lives
    /// in the conversation, so the same agent reached again resumes where it
    /// left off instead of replaying.
    pub fn spawn_receiver(
        &self,
        abort: HookAbortSignal,
        sink: impl Fn(&MailboxEnvelope) -> bool + Send + Sync + 'static,
    ) -> JoinHandle<()> {
        match self {
            Self::Node(node) => runtime::mailbox::spawn_inbox_poller(
                Arc::clone(&node.mailbox),
                INBOX_WAIT_MS,
                "nexus-a2a",
                abort,
                sink,
            ),
            Self::Disk(name) => runtime::mailbox::spawn_local_poller(
                runtime::mailbox::local_pair_root(),
                name.clone(),
                abort,
                sink,
            ),
        }
    }
}

/// Each agent's attach decision, made once per process: the node it attached
/// to, or `None` for the disk namespace. Keyed by agent because a host can run
/// sessions under several identities, each with its own credential. A failed
/// attach is not cached, so the next session reports the failure again.
static NODES: OnceLock<Mutex<HashMap<String, Option<Arc<Node>>>>> = OnceLock::new();

/// The node `agent` is attached to, from this machine's node record.
///
/// `None` when the machine has no node. Otherwise the credential is loaded and
/// a lazy client is created. Network availability is not a session-startup
/// dependency: the receiver and tools report their own node IO failures and
/// keep using this same namespace. They never silently fall back to disk.
///
/// # Errors
/// The node record or credential is unusable, or the client cannot be built.
pub fn node(agent: &str) -> Result<Option<Arc<Node>>, String> {
    let mut nodes = NODES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(decided) = nodes.get(agent) {
        return Ok(decided.clone());
    }
    let decided = match Config::from_node_record(agent)? {
        None => None,
        Some(config) => {
            let client = config.connect()?;
            // Cert-only auth: the client cert IS the authorization (the node
            // derives identity from its SAN), so no per-request token is sent.
            // That transport is a server-verified credential, so the backend
            // takes the mTLS (trusted-local) shape — the fail-closed
            // `from_arc` is for references the server never sees.
            let backend = NexusVfsFsBackend::from_arc_mtls(client, String::new(), "root");
            let mailbox = Arc::new(Mailbox::daemon_absolute(
                Arc::new(backend),
                config.agent.clone(),
            ));
            Some(Arc::new(Node { config, mailbox }))
        }
    };
    nodes.insert(agent.to_string(), decided.clone());
    Ok(decided)
}
impl Node {
    #[must_use]
    pub fn mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.mailbox)
    }
}

/// This session's peer namespace: the node when attached, else the pair root
/// under `agent_name`.
///
/// # Errors
/// The machine has a node but it is unusable for `agent_name`.
pub fn attach(agent_name: &str) -> Result<Peer, String> {
    Ok(match node(agent_name)? {
        Some(node) => Peer::Node(node),
        None => Peer::Disk(agent_name.to_string()),
    })
}

/// What `/status` and an ACP client report about `agent`'s peers.
#[must_use]
pub fn status(agent: &str) -> runtime::mailbox::PeerStatus {
    let receiver = runtime::mailbox::receiver_state(agent)
        .map_or("none", runtime::mailbox::ReceiverState::as_str);
    let (namespace, endpoint, error) = match attach(agent) {
        Ok(Peer::Node(node)) => (Some("node"), Some(node.config.endpoint.clone()), None),
        Ok(Peer::Disk(_)) => (Some("disk"), None, None),
        Err(error) => (None, None, Some(error)),
    };
    runtime::mailbox::PeerStatus {
        agent: agent.to_string(),
        namespace,
        endpoint,
        receiver,
        delivery: "automatic",
        cursor: runtime::mailbox::cursor_guarantee(agent)
            .map_or("none", runtime::cursor_fence::CursorGuarantee::as_str),
        error,
        last_receiver_error: runtime::mailbox::last_receiver_error(agent),
    }
}
