//! Process-singleton standalone nexus-A2A session (send + receive) for `scode`.
//!
//! Holds the one daemon connection an interactive `scode` process makes,
//! lazily dialed from [`runtime::nexus_mailbox::Config::from_env`]. The send
//! half feeds [`crate::tool_executor::CliToolExecutor`] via the shared
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

use nexus_vfs_client::NexusVfsClient;
use runtime::nexus_mailbox::{self, Config, Inbound};
use runtime::spawn_task::MailboxSender;
use runtime::HookAbortSignal;

/// Blocking-tail wait per receive iteration. Each drain parks in a blocking
/// `stream_read_at` (the DT_STREAM `read_at_blocking` primitive) up to this
/// long, waking sub-millisecond on the next inbox write and returning empty at
/// the deadline so the loop can re-check `abort`. This is event-driven, not a
/// poll interval — an idle receiver costs one parked RPC, not a `sleep` spin.
const INBOX_WAIT_MS: u64 = 500;

/// The resolved, connected standalone A2A session.
pub struct Session {
    config: Config,
    /// The one daemon connection: `ensure_inbox`, the send half, the CLI tool
    /// executor, and the blocking receive tail all share it. Safe to share
    /// because [`NexusVfsClient`] dispatches each op on its own task — a
    /// receiver parked in a blocking `stream_read_at` no longer stalls the
    /// worker, so it can't starve concurrent sends. (An earlier revision split
    /// off a second connection to dodge a single-worker stall; the client is
    /// now concurrent, so one connection is correct and simpler.)
    client: Arc<NexusVfsClient>,
    sender: MailboxSender,
}

impl Session {
    /// Clone the shared send capability for the CLI tool executor.
    pub fn sender(&self) -> MailboxSender {
        Arc::clone(&self.sender)
    }

    /// The peer-awareness system-prompt section (self identity + how to
    /// reach peers). Single source: [`Config::peer_system_prompt`].
    pub fn peer_system_prompt(&self) -> String {
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
pub fn session() -> Result<Option<&'static Session>, String> {
    SESSION
        .get_or_init(|| match Config::from_env()? {
            None => Ok(None),
            Some(config) => {
                let client = config.connect()?;
                // Provision our own inbox before anyone polls it — a terminal
                // scode is not a managed agent, so nothing else creates it.
                nexus_mailbox::ensure_inbox(&client, &config.agent, &config.api_key)?;
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

/// Spawn the background inbox receiver (the receive half).
///
/// Surfaces each new peer message to `sink` as it arrives. Starts at the
/// stream tail — a fresh interactive session never replays history, the same
/// seek-to-tail the co-host cold-start uses — and stops when `abort` fires.
///
/// Event-driven, not polling: each iteration parks in a blocking
/// `stream_read_at` (via `poll_new`'s `block_ms`) until the daemon signals the
/// next inbox write, then drains the burst. The block returns empty at the
/// `INBOX_WAIT_MS` deadline so the loop can re-check `abort`. No `sleep` spin —
/// an idle receiver costs one parked RPC, replacing the former poll interval.
pub fn spawn_poller(
    session: &'static Session,
    abort: HookAbortSignal,
    sink: impl Fn(&Inbound) + Send + 'static,
) -> JoinHandle<()> {
    // Shares `session.client`: the client dispatches ops concurrently, so
    // parking here on the blocking tail cannot starve the send half.
    let client = Arc::clone(&session.client);
    let agent = session.config.agent.clone();
    let api_key = session.config.api_key.clone();
    std::thread::Builder::new()
        .name("nexus-a2a-receiver".into())
        .spawn(move || {
            // Seek to tail: a non-blocking (block_ms=0) drain-and-discard once
            // to fix the cursor at the current end, so only messages that
            // arrive after startup surface.
            let mut cursor = match nexus_mailbox::poll_new(&client, &agent, 0, &api_key, 0) {
                Ok((_history, tail)) => tail,
                Err(e) => {
                    eprintln!("[nexus-a2a] initial inbox seek failed: {e}");
                    0
                }
            };
            while !abort.is_aborted() {
                // Block on the tail up to INBOX_WAIT_MS, then drain the burst.
                match nexus_mailbox::poll_new(&client, &agent, cursor, &api_key, INBOX_WAIT_MS) {
                    Ok((msgs, next)) => {
                        for m in &msgs {
                            sink(m);
                        }
                        cursor = next;
                    }
                    Err(e) => {
                        eprintln!("[nexus-a2a] inbox poll failed: {e}");
                        // Avoid a hot error loop if the daemon connection is
                        // sick; the blocking read itself paces the happy path.
                        std::thread::sleep(std::time::Duration::from_millis(INBOX_WAIT_MS));
                    }
                }
            }
        })
        .expect("spawn nexus-a2a receiver thread")
}
