//! Stdio-based ACP server.
//!
//! Thin wrapper that runs the shared ACP handler chain over stdin/stdout.

use std::sync::{Arc, Mutex};

use agent_client_protocol_tokio::Stdio;

use crate::acp_sdk_server::{
    new_abort_registry, run_acp_on_transport, SdkAcpConfig, SdkAcpDelegate, SharedDelegate,
};

/// Run the ACP server on stdin/stdout.
///
/// # Errors
///
/// Returns an error if the transport or handler chain fails.
pub async fn run_acp_stdio_server(
    config: SdkAcpConfig,
    delegate: Box<dyn SdkAcpDelegate>,
) -> Result<(), Box<dyn std::error::Error>> {
    // When launched over stdio by a host (e.g. an editor), the agent must not
    // outlive that host. Two independent signals drive shutdown:
    //
    // * `spawn_stdin_eof_watchdog` exits when stdin's writer end is closed
    //   (graceful disconnect). The SDK transport keeps its future alive on
    //   stdin EOF because stdout is still open, so we need our own probe.
    // * `spawn_parent_exit_watchdog` exits when the original parent process
    //   dies and we are reparented (host killed abruptly, stdin inherited).
    spawn_stdin_eof_watchdog();
    spawn_parent_exit_watchdog();

    let delegate: SharedDelegate = Arc::new(Mutex::new(delegate));
    run_acp_on_transport(&config, delegate, new_abort_registry(), Stdio::new()).await
}

/// Watch for stdin's writer end closing and exit when it does.
///
/// Uses `poll(2)` on fd 0 without consuming bytes: `POLLHUP` is always
/// reported in `revents` regardless of the requested events, so a closed
/// writer wakes the poll even though we never registered for `POLLIN`. This
/// avoids racing the SDK transport's own stdin reader.
#[cfg(unix)]
fn spawn_stdin_eof_watchdog() {
    use std::os::fd::AsFd;

    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    tokio::task::spawn_blocking(|| {
        let stdin = std::io::stdin();
        let exit_flags = PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL;
        loop {
            let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLPRI)];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {
                    let revents = fds[0].revents().unwrap_or(PollFlags::empty());
                    if revents.intersects(exit_flags) {
                        std::process::exit(0);
                    }
                }
                Err(nix::errno::Errno::EINTR) => {}
                Err(_) => return,
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_stdin_eof_watchdog() {
    // Non-unix: rely on the transport returning on stdin EOF.
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
    let initial_ppid = nix::unistd::getppid();

    // Already orphaned before we even started (parent reaped, reparented to
    // init): nothing to serve, so exit immediately.
    if initial_ppid.as_raw() <= 1 {
        std::process::exit(0);
    }

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if nix::unistd::getppid() != initial_ppid {
                std::process::exit(0);
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_parent_exit_watchdog() {
    // No portable parent-death notification is available; on these platforms we
    // rely on the transport returning when stdin reaches EOF.
}
