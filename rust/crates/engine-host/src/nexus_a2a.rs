//! Process-singleton standalone nexus-A2A session (send + receive) for `scode`.
//!
//! Holds the one daemon connection an interactive `scode` process makes,
//! lazily dialed from [`runtime::nexus_mailbox::Config::from_env`]. The send
//! half feeds [`crate::tool_executor::CliToolExecutor`] via the shared
//! [`MailboxSender`]; the receive half is a background poller that surfaces
//! peer messages into the REPL as they arrive.
//!
//! Transport is the unified [`runtime::mailbox::Mailbox`] backed by
//! [`NexusVfsFsBackend`] — the same abstraction the local JSONL path uses
//! (with [`StdFsBackend`]), so standalone A2A and coordinator sub-agents
//! share every line except the backend construction.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::NexusVfsFsBackend;
use runtime::mailbox::{InboxConvention, InboxCursor, Mailbox};
use runtime::nexus_mailbox::Config;
use runtime::spawn_task::MailboxSender;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration.
const INBOX_WAIT_MS: u64 = 500;

/// Where this agent's A2A read position lives.
///
/// The config home rather than the workspace: an A2A identity outlives any one
/// checkout, and the same agent reached from two directories is still one
/// receiver of one inbox.
fn cursor_store_for(agent: &str) -> InboxCursor {
    InboxCursor::at(
        runtime::config::default_config_home().join(InboxCursor::file_name("a2a-cursor-", agent)),
    )
}

/// The resolved, connected standalone A2A session.
pub struct Session {
    config: Config,
    mailbox: Arc<Mailbox>,
}

impl Session {
    /// Build a [`MailboxSender`] for the CLI tool executor.
    pub fn sender(&self) -> MailboxSender {
        self.mailbox.sender()
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
                let mailbox = Arc::new(Mailbox::new(
                    Arc::new(backend),
                    config.agent.clone(),
                    InboxConvention::NexusA2a,
                ));
                mailbox
                    .ensure_inbox()
                    .map_err(|e| format!("ensure A2A inbox: {e}"))?;
                Ok(Some(Session { config, mailbox }))
            }
        })
        .as_ref()
        .map(Option::as_ref)
        .map_err(Clone::clone)
}

/// Spawn the background A2A inbox receiver.
///
/// The loop itself is [`runtime::mailbox::spawn_inbox_poller`], shared with the
/// local JSONL inbox. All that is A2A-specific is the mailbox (already built on
/// the session's nexus-vfs backend) and where the cursor lives.
pub fn spawn_poller(
    session: &'static Session,
    abort: HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) + Send + 'static,
) -> JoinHandle<()> {
    runtime::mailbox::spawn_inbox_poller(
        Arc::clone(&session.mailbox),
        cursor_store_for(&session.config.agent),
        INBOX_WAIT_MS,
        "nexus-a2a",
        abort,
        sink,
    )
}
