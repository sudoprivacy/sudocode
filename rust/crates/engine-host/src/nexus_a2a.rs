//! Process-singleton standalone nexus-A2A session (send + receive) for `scode`.
//!
//! Holds the one daemon connection an interactive `scode` process makes,
//! lazily dialed from [`runtime::nexus_mailbox::Config::from_env`]. The send
//! half is the session's [`Mailbox`], handed to
//! [`crate::tool_executor::CliToolExecutor`] so `send` resolves recipients
//! through it; the receive half is a background receiver that surfaces peer
//! messages into the REPL as they arrive.
//!
//! Transport is the unified [`runtime::mailbox::Mailbox`] backed by
//! [`NexusVfsFsBackend`]. This is the SAME mailbox a co-hosted agent runs on
//! inside the daemon and the same one the workspace-local path runs on over
//! `StdFsBackend`: identical conversations, addressing and receiver, differing
//! only in the backend that reaches them. Running `scode` standalone and
//! running it under the daemon's managed-agent service are two deployments of
//! one contract, not two contracts.

use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::NexusVfsFsBackend;
use runtime::mailbox::Mailbox;
use runtime::nexus_mailbox::Config;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration.
const INBOX_WAIT_MS: u64 = 500;

/// The resolved, connected standalone A2A session.
pub struct Session {
    config: Config,
    mailbox: Arc<Mailbox>,
}

impl Session {
    /// The session's mailbox — handed to the tool dispatcher so everything that
    /// sends resolves the same namespace.
    #[must_use]
    pub fn mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.mailbox)
    }

    /// The peer-awareness system-prompt section.
    pub fn peer_system_prompt(&self) -> String {
        self.config.peer_system_prompt()
    }
}

static SESSION: OnceLock<Result<Option<Session>, String>> = OnceLock::new();

/// Resolve + dial the standalone A2A session, exactly once.
pub fn session() -> Result<Option<&'static Session>, String> {
    SESSION
        .get_or_init(|| match Config::from_env()? {
            None => Ok(None),
            Some(config) => {
                let client = config.connect()?;
                let backend = NexusVfsFsBackend::from_arc(client, config.api_key.clone());
                let mailbox = Arc::new(Mailbox::daemon_absolute(
                    Arc::new(backend),
                    config.agent.clone(),
                ));
                Ok(Some(Session { config, mailbox }))
            }
        })
        .as_ref()
        .map(Option::as_ref)
        .map_err(Clone::clone)
}

/// Spawn the background A2A receiver.
///
/// The loop itself is [`runtime::mailbox::spawn_inbox_poller`], shared with the
/// co-hosted agent and the local JSONL inbox. All that is A2A-specific here is
/// the mailbox, already built on the session's nexus-vfs backend.
///
/// Nothing is provisioned first, and the read position is not passed in. A
/// receiver has no inbox of its own to create: a conversation is provisioned by
/// whichever side sends first, indexed under BOTH names, and until that happens
/// an empty chat list is the honest answer. The position then lives in the
/// conversation rather than in this process's config home, so the same agent
/// reached from another machine resumes where it left off instead of replaying.
pub fn spawn_poller(
    session: &'static Session,
    abort: HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) -> bool + Send + Sync + 'static,
) -> JoinHandle<()> {
    runtime::mailbox::spawn_inbox_poller(
        Arc::clone(&session.mailbox),
        INBOX_WAIT_MS,
        "nexus-a2a",
        abort,
        sink,
    )
}
