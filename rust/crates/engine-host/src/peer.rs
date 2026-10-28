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
use runtime::FsBackend;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration on the node.
const INBOX_WAIT_MS: u64 = 500;

enum NodeState {
    Pending {
        endpoint: Option<String>,
        error: Option<String>,
    },
    Ready {
        config: Config,
        backend: Arc<dyn FsBackend>,
    },
}

struct NodeBinding {
    agent: String,
    state: Mutex<NodeState>,
}

impl NodeBinding {
    fn backend(&self) -> Result<Arc<dyn FsBackend>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut endpoint = match &*state {
            NodeState::Pending { endpoint, .. } => endpoint.clone(),
            NodeState::Ready { backend, .. } => return Ok(Arc::clone(backend)),
        };
        let resolved: Result<(Config, Arc<dyn FsBackend>), String> = (|| {
            let config = Config::from_node_record(&self.agent)?.ok_or_else(|| {
                "the selected node record is no longer available; A2A remains on the node namespace".to_string()
            })?;
            endpoint = Some(config.endpoint.clone());
            let client = config.connect()?;
            // The node authenticates the presented agent certificate. Keep
            // the existing cert-only backend; no per-request token or local
            // identity reference is substituted for that server verification.
            let backend: Arc<dyn FsBackend> = Arc::new(NexusVfsFsBackend::from_arc_mtls(
                client,
                String::new(),
                "root",
            ));
            Ok((config, backend))
        })();
        match resolved {
            Ok((config, backend)) => {
                *state = NodeState::Ready {
                    config,
                    backend: Arc::clone(&backend),
                };
                Ok(backend)
            }
            Err(error) => {
                *state = NodeState::Pending {
                    endpoint,
                    error: Some(error.clone()),
                };
                Err(error)
            }
        }
    }

    #[inline]
    fn snapshot(&self) -> (Option<String>, Option<String>) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            NodeState::Pending { endpoint, error } => (endpoint.clone(), error.clone()),
            NodeState::Ready { config, .. } => (Some(config.endpoint.clone()), None),
        }
    }
}

/// A selected node namespace, including an agent awaiting its credential.
pub struct Node {
    binding: Arc<NodeBinding>,
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
    /// to reach peers: the same one on every host.
    #[must_use]
    pub fn prompt_section(&self) -> String {
        match self {
            Self::Node(node) => runtime::agent_mailbox::a2a_prompt_section(&node.binding.agent),
            Self::Disk(name) => runtime::agent_mailbox::a2a_prompt_section(name),
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

/// Each agent's namespace selection, made once per process. A selected node's
/// unavailable binding is retained and retried by that same mailbox; it never
/// becomes `None` (disk) after a credential/configuration failure. A process
/// that selected disk stays there until an explicit handoff/restart.
static NODES: OnceLock<Mutex<HashMap<String, Option<Arc<Node>>>>> = OnceLock::new();

/// The node `agent` is attached to, from this machine's node record.
///
/// `None` only when the machine has no node record. A present but unusable
/// record retains a node mailbox whose IO reports the cause and retries local
/// configuration resolution. No network or credential error prevents an
/// ordinary standalone session from starting.
#[must_use]
pub fn node(agent: &str) -> Option<Arc<Node>> {
    let mut nodes = NODES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(decided) = nodes.get(agent) {
        return decided.clone();
    }
    let resolved = Config::from_node_record(agent);
    // A dangling record symlink is still a configured namespace. So is an
    // unreadable path: neither is permission to deliver to a different disk.
    let absent = matches!(&resolved, Ok(None)) && !runtime::nexus_mailbox::node_record_present();
    let decided = if absent {
        None
    } else {
        let (endpoint, error) = match resolved {
            Ok(Some(config)) => (Some(config.endpoint), None),
            Ok(None) => (
                None,
                Some("the selected node record is unavailable".to_string()),
            ),
            Err(error) => (None, Some(error)),
        };
        let binding = Arc::new(NodeBinding {
            agent: agent.to_string(),
            state: Mutex::new(NodeState::Pending { endpoint, error }),
        });
        let resolving = Arc::clone(&binding);
        let mailbox = Arc::new(Mailbox::daemon_resolving(agent.to_string(), move || {
            resolving.backend()
        }));
        Some(Arc::new(Node { binding, mailbox }))
    };
    nodes.insert(agent.to_string(), decided.clone());
    decided
}

impl Node {
    /// The actual authenticated backend shared with this actor's mailbox.
    /// Resolve only when a configured Nexus model mount needs it.
    #[inline]
    pub fn fs(&self) -> Result<Arc<dyn FsBackend>, String> {
        self.binding.backend()
    }

    #[inline]
    #[must_use]
    pub fn mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.mailbox)
    }
}

/// This session's selected peer namespace, independent of node availability.
#[must_use]
pub fn attach(agent_name: &str) -> Peer {
    match node(agent_name) {
        Some(node) => Peer::Node(node),
        None => Peer::Disk(agent_name.to_string()),
    }
}

/// What `/status` and an ACP client report about `agent`'s peers.
#[must_use]
pub fn status(agent: &str) -> runtime::mailbox::PeerStatus {
    let receiver = runtime::mailbox::receiver_state(agent)
        .map_or("none", runtime::mailbox::ReceiverState::as_str);
    let (namespace, endpoint, error) = match attach(agent) {
        Peer::Node(node) => {
            let (endpoint, error) = node.binding.snapshot();
            (Some("node"), endpoint, error)
        }
        Peer::Disk(_) => (Some("disk"), None, None),
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
