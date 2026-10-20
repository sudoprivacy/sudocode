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

use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::NexusVfsFsBackend;
use runtime::mailbox::Mailbox;
use runtime::nexus_mailbox::Config;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration on the node.
const INBOX_WAIT_MS: u64 = 500;

/// The connected node session, dialled at most once per process.
pub struct Node {
    config: Config,
    mailbox: Arc<Mailbox>,
}

/// Where this session's peers are.
pub enum Peer {
    Node(&'static Node),
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

static NODE: OnceLock<Result<Option<Node>, String>> = OnceLock::new();

/// The node this process is attached to, dialled exactly once. `None` when no
/// node is configured; an incomplete configuration or a failed dial is an
/// error, never a silent fall back to disk.
pub fn node() -> Result<Option<&'static Node>, String> {
    NODE.get_or_init(|| match Config::from_env()? {
        None => Ok(None),
        Some(config) => {
            let client = config.connect()?;
            // Cert-only auth: the client cert IS the authorization (the node
            // derives identity from its SAN), so no per-request token is sent.
            let backend = NexusVfsFsBackend::from_arc(client, String::new());
            let mailbox = Arc::new(Mailbox::daemon_absolute(
                Arc::new(backend),
                config.agent.clone(),
            ));
            Ok(Some(Node { config, mailbox }))
        }
    })
    .as_ref()
    .map(Option::as_ref)
    .map_err(Clone::clone)
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
/// A node is configured but cannot be reached.
pub fn attach(agent_name: &str) -> Result<Peer, String> {
    Ok(match node()? {
        Some(node) => Peer::Node(node),
        None => Peer::Disk(agent_name.to_string()),
    })
}
