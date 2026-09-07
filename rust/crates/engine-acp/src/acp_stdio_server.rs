//! Stdio-based ACP server.
//!
//! Thin wrapper that runs the shared ACP handler chain over stdin/stdout.

use std::pin::Pin;
use std::task::{Context, Poll};

use agent_client_protocol::ByteStreams;
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::acp_sdk_server::{new_session_registry, run_acp_on_transport, SdkAcpConfig};

/// Run the ACP server on stdin/stdout.
///
/// # Errors
///
/// Returns an error if the transport or handler chain fails.
pub async fn run_acp_stdio_server(config: SdkAcpConfig) -> Result<(), Box<dyn std::error::Error>> {
    // When launched over stdio by a host (e.g. an editor), the agent must not
    // outlive that host. Two signals drive shutdown: stdin reaching EOF (a
    // graceful disconnect — see `ExitOnStdinEof`), and, where the platform can
    // report it, the original parent dying while stdin stays inherited by some
    // other process (`spawn_parent_exit_watchdog`).
    spawn_parent_exit_watchdog();

    // Hand the transport its own stdin wrapper rather than using
    // `agent_client_protocol_tokio::Stdio`, which reads stdin directly. The
    // transport keeps its future alive at stdin EOF because stdout is still
    // open, so EOF has to be acted on by someone; noticing it *inside* the
    // reader the transport already owns is the only place that neither races
    // that reader nor needs a platform-specific probe.
    run_acp_on_transport(
        &config,
        new_session_registry(),
        ByteStreams::new(
            tokio::io::stdout().compat_write(),
            ExitOnStdinEof(tokio::io::stdin()).compat(),
        ),
    )
    .await
}

/// Process stdin that terminates the process when its writer end closes.
///
/// Previously this was a `poll(2)`-on-fd-0 watchdog beside the transport,
/// which could only be built on Unix — so on Windows an ACP host that
/// disconnected left `scode acp` running forever, one orphan per session.
/// Reading is the one thing every platform agrees on: a pipe whose writer is
/// gone reports EOF, and a `poll_read` that fills nothing when it had room is
/// exactly that.
struct ExitOnStdinEof(tokio::io::Stdin);

impl AsyncRead for ExitOnStdinEof {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let had_room = buf.remaining() > 0;
        let filled_before = buf.filled().len();
        let poll = Pin::new(&mut self.0).poll_read(cx, buf);
        if matches!(poll, Poll::Ready(Ok(()))) && had_room && buf.filled().len() == filled_before {
            std::process::exit(0);
        }
        poll
    }
}

/// Watch for the parent process going away and exit when it does.
///
/// We record the parent PID at startup and poll it: when the original parent
/// exits, the kernel reparents us (to init or a subreaper), changing the
/// reported parent PID. That is an unambiguous signal that the host is gone and
/// we should exit rather than linger. `getppid` reflects only true *process*
/// death, so this never fires while the host is still alive.
#[cfg(unix)]
fn spawn_parent_exit_watchdog() {
    use runtime::sandbox::detect_container_environment;

    let initial_ppid = nix::unistd::getppid();

    // Already orphaned before we even started (parent reaped, reparented to
    // init): nothing to serve, so exit immediately.
    // Skip this check in containers where ppid=1 is normal.
    if initial_ppid.as_raw() <= 1 && !detect_container_environment().in_container {
        eprintln!("[acp-stdio] Exiting: parent process is PID 1 (orphaned) and not running in a container");
        std::process::exit(0);
    }

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if nix::unistd::getppid() != initial_ppid {
                eprintln!("[acp-stdio] Exiting: parent process changed (parent exited)");
                std::process::exit(0);
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_parent_exit_watchdog() {
    // No portable parent-death notification is available here. `ExitOnStdinEof`
    // still covers the disconnect that matters — a host that exits closes its
    // end of the pipe — so this only leaves the case where the host dies but
    // something else keeps the write handle open.
}
