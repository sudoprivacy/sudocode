//! ACP over the A2A session mailbox (`acp-mailbox/1`).
//!
//! The third transport for the SAME handler chain stdio and WebSocket run. ACP
//! supplies the turn vocabulary; `a2a::session` supplies addressing, a
//! connection generation and ordered replay suppression over the conversation
//! the two participants already share. Nothing about the protocol changes with
//! the pipe it arrives on, so this module only converts shapes: the session
//! mailbox speaks whole `SessionPayload` frames, the ACP SDK speaks a byte
//! stream of newline-delimited JSON-RPC.
//!
//! Writing a second ACP server for this transport would have been the obvious
//! mistake: `initialize`, `session/new`, `session/prompt`, the `session/update`
//! stream and the reverse `session/request_permission` all already exist, and a
//! copy would drift from them silently.

use std::sync::Arc;

use a2a::session::{SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use agent_client_protocol::ByteStreams;
use kernel::kernel::syscall::KernelSyscall;
use kernel::kernel::OperationContext;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::acp_sdk_server::{new_session_registry, run_acp_on_transport, SdkAcpConfig};

/// How long a receive blocks before the pump re-checks for closure. Short
/// enough that a closed channel is noticed promptly, long enough that an idle
/// session is not a spin loop.
const RECEIVE_TIMEOUT_MS: u64 = 500;

/// In-memory pipe capacity between the frame pumps and the ACP transport. One
/// ACP message fits comfortably; the frame codec enforces the real ceiling.
const PIPE_CAPACITY: usize = 256 * 1024;

/// Serve ACP for `endpoint`'s agent side over the session mailbox.
///
/// Runs until the channel closes or the handler chain returns. The caller owns
/// the task: upstream requires this loop to stay independent of the engine's
/// turn worker, because a reverse permission request has to be answerable while
/// a turn is still running.
///
/// # Errors
///
/// When the mailbox cannot be opened (a malformed endpoint, an actor that does
/// not match the authenticated context, a conversation without a framed stream)
/// or when the handler chain fails.
pub async fn run_acp_session_mailbox<K>(
    config: SdkAcpConfig,
    kernel: Arc<K>,
    ctx: OperationContext,
    endpoint: a2a::session::SessionEndpoint,
) -> Result<(), Box<dyn std::error::Error>>
where
    K: KernelSyscall + Send + Sync + 'static,
{
    let mailbox = Arc::new(
        SessionMailbox::open(kernel, ctx, endpoint, SessionSide::Agent)
            .map_err(|e| format!("attach acp-mailbox/1: {e}"))?,
    );

    // Two pipes, because the SDK wants a reader and a writer it owns: inbound
    // frames become bytes the transport reads, bytes the transport writes become
    // outbound frames.
    let (inbound_tx, inbound_rx) = tokio::io::duplex(PIPE_CAPACITY);
    let (outbound_tx, outbound_rx) = tokio::io::duplex(PIPE_CAPACITY);

    let reader_pump = {
        let mailbox = Arc::clone(&mailbox);
        tokio::task::spawn_blocking(move || pump_inbound(&mailbox, inbound_tx))
    };
    let writer_pump = {
        let mailbox = Arc::clone(&mailbox);
        tokio::spawn(async move { pump_outbound(&mailbox, outbound_rx).await })
    };

    let served = run_acp_on_transport(
        &config,
        new_session_registry(),
        ByteStreams::new(outbound_tx.compat_write(), inbound_rx.compat()),
    )
    .await;

    // Say why the channel ended before dropping it, so a controller sees a
    // `Closed` frame instead of silence it has to time out on.
    let reason = match &served {
        Ok(()) => "agent handler chain ended".to_string(),
        Err(error) => format!("agent handler chain failed: {error}"),
    };
    let _ = mailbox.send(SessionPayload::Closed { reason });
    reader_pump.abort();
    writer_pump.abort();
    served
}

/// Inbound frames -> bytes the ACP transport reads. One JSON object per line.
fn pump_inbound<K>(mailbox: &SessionMailbox<K>, pipe: tokio::io::DuplexStream)
where
    K: KernelSyscall,
{
    let mut pipe = pipe;
    let handle = tokio::runtime::Handle::current();
    loop {
        match mailbox.receive(RECEIVE_TIMEOUT_MS) {
            // A timeout, our own write, or another attachment's traffic.
            Ok(None) => {
                if mailbox.is_closed() {
                    return;
                }
            }
            Ok(Some(SessionPayload::Rpc { message })) => {
                let Ok(mut line) = serde_json::to_vec(&message) else {
                    return;
                };
                line.push(b'\n');
                if handle.block_on(pipe.write_all(&line)).is_err() {
                    return;
                }
            }
            // The controller detached. Closing the pipe is what tells the
            // handler chain to wind down.
            Ok(Some(SessionPayload::Closed { .. })) | Err(_) => return,
        }
    }
}

/// Bytes the ACP transport writes -> outbound frames, one per line.
async fn pump_outbound<K>(mailbox: &SessionMailbox<K>, pipe: tokio::io::DuplexStream)
where
    K: KernelSyscall,
{
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            return;
        };
        if mailbox.send(SessionPayload::Rpc { message }).is_err() {
            return;
        }
    }
}

/// The session driver a co-host binary installs so `acp-mailbox/1` is served by
/// this ACP server.
///
/// Lives here because the serving half is ACP and `engine-acp` already depends
/// on `engine-host`; the composition root sees both crates and is the one place
/// that can hand this across without either reaching over the boundary.
pub struct AcpSessionDriver {
    config: SdkAcpConfig,
}

impl AcpSessionDriver {
    /// Serve sessions with the model and auth configuration a co-hosted agent
    /// resolves exactly as the CLI does.
    #[must_use]
    pub fn new(config: SdkAcpConfig) -> Self {
        Self { config }
    }
}

impl engine_host::managed_agent::SessionDriver for AcpSessionDriver {
    fn serve(
        &self,
        kernel: Arc<kernel::kernel::Kernel>,
        agent: &str,
        owner_id: &str,
        zone_id: &str,
        endpoint: a2a::session::SessionEndpoint,
    ) -> Result<(), String> {
        // The identity the mailbox authenticates as is not ours to choose:
        // `open` refuses an actor that does not match the endpoint's agent.
        let ctx = OperationContext::new(owner_id, zone_id, false, Some(agent), true);
        // Its own runtime, on the thread the spawn gave this driver: borrowing
        // whichever executor happened to be current would tie a session's life
        // to it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("acp-mailbox/1 runtime: {e}"))?;
        rt.block_on(run_acp_session_mailbox(
            self.config.clone(),
            kernel,
            ctx,
            endpoint,
        ))
        .map_err(|e| e.to_string())
    }
}
