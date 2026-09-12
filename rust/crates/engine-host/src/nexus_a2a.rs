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
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::nexus_mailbox::Config;
use runtime::spawn_task::MailboxSender;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration.
const INBOX_WAIT_MS: u64 = 500;

fn cursor_path_for(agent: &str) -> PathBuf {
    let safe: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    runtime::config::default_config_home().join(format!("a2a-cursor-{safe}"))
}

fn load_cursor(agent: &str) -> Option<u64> {
    std::fs::read_to_string(cursor_path_for(agent))
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
}

fn save_cursor(agent: &str, offset: u64) {
    let path = cursor_path_for(agent);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, offset.to_string());
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

/// Spawn the background inbox receiver.
pub fn spawn_poller(
    session: &'static Session,
    abort: HookAbortSignal,
    sink: impl Fn(&MailboxEnvelope) + Send + 'static,
) -> JoinHandle<()> {
    let mailbox = Arc::clone(&session.mailbox);
    let agent = session.config.agent.clone();
    std::thread::Builder::new()
        .name("nexus-a2a-receiver".into())
        .spawn(move || {
            let mut cursor = match load_cursor(&agent) {
                Some(saved) => saved,
                None => match mailbox.poll(0, 0) {
                    Ok((_history, tail)) => {
                        save_cursor(&agent, tail);
                        tail
                    }
                    Err(e) => {
                        eprintln!("[nexus-a2a] initial inbox seek failed: {e}");
                        0
                    }
                },
            };
            while !abort.is_aborted() {
                match mailbox.poll(cursor, INBOX_WAIT_MS) {
                    Ok((msgs, next)) => {
                        for m in &msgs {
                            sink(m);
                        }
                        if next > cursor {
                            cursor = next;
                            save_cursor(&agent, cursor);
                        }
                    }
                    Err(e) => {
                        eprintln!("[nexus-a2a] inbox poll failed: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(INBOX_WAIT_MS));
                    }
                }
            }
        })
        .expect("spawn nexus-a2a receiver thread")
}
