//! Process-singleton standalone nexus-A2A session (send + receive) for `scode`.
//!
//! Holds the one daemon connection an interactive `scode` process makes,
//! lazily dialed from [`runtime::nexus_mailbox::Config::from_env`]. The send
//! half feeds [`crate::cli::tool_executor::CliToolExecutor`] via the shared
//! [`MailboxSender`] (the same handler the co-host uses); the receive half is
//! a background poller that surfaces peer messages into the REPL as they
//! arrive.
//!
//! This process-global lives in the CLI, not in `runtime`: `runtime` is the
//! reentrant engine the multi-agent co-host also drives, so it must stay free
//! of any "one A2A session per process" assumption. A standalone `scode` *is*
//! exactly one session per process, so the singleton is a CLI concern. All the
//! transport (config, dial, send, poll) still lives once in
//! `runtime::nexus_mailbox`; this module only owns the process-lifetime handle.

use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use nexus_vfs_client::NexusVfsClient;
use runtime::nexus_mailbox::{self, Config, Inbound};
use runtime::spawn_task::MailboxSender;
use runtime::HookAbortSignal;

/// How often the receive poller checks the inbox. A2A is turn-scale, not
/// latency-critical, so a coarse interval keeps idle cost negligible.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// The resolved, connected standalone A2A session.
pub(crate) struct Session {
    config: Config,
    client: Arc<NexusVfsClient>,
    sender: MailboxSender,
}

impl Session {
    /// Clone the shared send capability for the CLI tool executor.
    pub(crate) fn sender(&self) -> MailboxSender {
        Arc::clone(&self.sender)
    }

    /// The peer-awareness system-prompt section (self identity + how to
    /// reach peers). Single source: [`Config::peer_system_prompt`].
    pub(crate) fn peer_system_prompt(&self) -> String {
        self.config.peer_system_prompt()
    }
}

/// Lazily resolved once; `Err` on partial config or dial failure (fail loud).
static SESSION: OnceLock<Result<Option<Session>, String>> = OnceLock::new();

/// Resolve + dial the standalone A2A session, exactly once.
///
/// Returns `Ok(None)` when A2A is off (no `NEXUS_A2A_ENDPOINT`) — the fast
/// path that leaves `scode` unchanged — `Ok(Some)` when connected, and `Err`
/// when the environment is a *partial* configuration or the daemon cannot be
/// dialed. The result is cached, so repeated calls are cheap and never
/// re-dial. Callers propagate the `Err` up their existing `Result` chain so a
/// misconfiguration fails startup loudly rather than silently disabling A2A.
pub(crate) fn session() -> Result<Option<&'static Session>, String> {
    SESSION
        .get_or_init(|| match Config::from_env()? {
            None => Ok(None),
            Some(config) => {
                let client = config.connect()?;
                let sender = nexus_mailbox::grpc_sender(
                    Arc::clone(&client),
                    config.agent.clone(),
                    config.api_key.clone(),
                );
                Ok(Some(Session {
                    config,
                    client,
                    sender,
                }))
            }
        })
        .as_ref()
        .map(Option::as_ref)
        .map_err(Clone::clone)
}

/// Spawn the background inbox poller (the receive half).
///
/// Surfaces each new peer message to `sink` as it arrives. Starts at the
/// stream tail — a fresh interactive session never replays history, the same
/// seek-to-tail the co-host cold-start uses — and stops when `abort` fires.
pub(crate) fn spawn_poller(
    session: &'static Session,
    abort: HookAbortSignal,
    sink: impl Fn(&Inbound) + Send + 'static,
) -> JoinHandle<()> {
    let client = Arc::clone(&session.client);
    let agent = session.config.agent.clone();
    let api_key = session.config.api_key.clone();
    std::thread::Builder::new()
        .name("nexus-a2a-poller".into())
        .spawn(move || {
            // Seek to tail: drain-and-discard once to fix the cursor at the
            // current end, so only messages that arrive after startup surface.
            let mut cursor = match nexus_mailbox::poll_new(&client, &agent, 0, &api_key) {
                Ok((_history, tail)) => tail,
                Err(e) => {
                    eprintln!("[nexus-a2a] initial inbox seek failed: {e}");
                    0
                }
            };
            while !abort.is_aborted() {
                match nexus_mailbox::poll_new(&client, &agent, cursor, &api_key) {
                    Ok((msgs, next)) => {
                        for m in &msgs {
                            sink(m);
                        }
                        cursor = next;
                    }
                    Err(e) => eprintln!("[nexus-a2a] inbox poll failed: {e}"),
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        })
        .expect("spawn nexus-a2a poller thread")
}
