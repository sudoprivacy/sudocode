//! Own one shell's captured descriptors and their asynchronous readers.

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use std::io;
#[cfg(not(target_os = "macos"))]
use std::process::Stdio;
use tokio::process::Command;

use super::shell_process_group;

#[cfg(target_os = "macos")]
pub(super) type StdoutPipe = FifoReader;
#[cfg(target_os = "macos")]
pub(super) type StderrPipe = FifoReader;
#[cfg(not(target_os = "macos"))]
pub(super) type StdoutPipe = tokio::process::ChildStdout;
#[cfg(not(target_os = "macos"))]
pub(super) type StderrPipe = tokio::process::ChildStderr;

/// Spawn captured output once for both streaming and nonstreaming execution.
pub(super) fn spawn_captured_shell(
    mut command: Command,
) -> io::Result<(AsyncGroupChild, StdoutPipe, StderrPipe)> {
    // Darwin's anonymous pipe() is followed by separate CLOEXEC fcntl calls.
    // Concurrent spawns can inherit writers during that window. A private,
    // unlinked FIFO lets open(O_CLOEXEC) establish ownership atomically.
    #[cfg(target_os = "macos")]
    let (stdout_read, stderr_read) = {
        let (stdout_read, stdout_write) = close_on_exec_pipe()?;
        let (stderr_read, stderr_write) = close_on_exec_pipe()?;
        command.stdout(stdout_write).stderr(stderr_write);
        (stdout_read, stderr_read)
    };
    #[cfg(not(target_os = "macos"))]
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    command.kill_on_drop(true);
    let child = shell_process_group(command.group()).spawn()?;
    // Custom Stdio files remain owned by Command after spawn. Close the
    // parent's writers before registering readers or awaiting output.
    drop(command);
    #[cfg(target_os = "macos")]
    let (stdout, stderr) = (FifoReader::new(stdout_read)?, FifoReader::new(stderr_read)?);
    #[cfg(not(target_os = "macos"))]
    let (child, stdout, stderr) = {
        let mut child = child;
        let stdout = child.inner().stdout.take().expect("stdout piped");
        let stderr = child.inner().stderr.take().expect("stderr piped");
        (child, stdout, stderr)
    };
    Ok((child, stdout, stderr))
}

/// Create a pipe whose endpoints are atomically close-on-exec on Darwin.
#[cfg(target_os = "macos")]
fn close_on_exec_pipe() -> io::Result<(std::os::fd::OwnedFd, std::fs::File)> {
    use nix::{fcntl::OFlag, sys::stat::Mode, unistd::mkfifo};
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    let directory = tempfile::Builder::new()
        .prefix("scode-shell-pipe-")
        .tempdir()?;
    let path = directory.path().join("pipe");
    mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR)?;
    let read = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).bits())
        .open(&path)?;
    let write = OpenOptions::new()
        .write(true)
        .custom_flags(OFlag::O_CLOEXEC.bits())
        .open(&path)?;
    std::fs::remove_file(path)?;
    Ok((read.into(), write))
}

/// A FIFO reader that drains readiness until the syscall reports `WouldBlock`.
///
/// Darwin announces FIFO data through readiness but never the FIFO's end:
/// once the last writer closes, kqueue (edge- or level-triggered, or freshly
/// registered) and `poll()` all stay silent while `read()` returns 0. A
/// reader that drained the data before the shell exited would otherwise wait
/// out the command's whole timeout. So while it waits it also reads again
/// every [`EOF_RECHECK`].
#[cfg(target_os = "macos")]
pub(super) struct FifoReader {
    io: tokio::io::unix::AsyncFd<std::fs::File>,
    recheck: std::pin::Pin<Box<tokio::time::Sleep>>,
}

/// How long a drained FIFO waits before reading again to look for its end.
#[cfg(target_os = "macos")]
const EOF_RECHECK: std::time::Duration = std::time::Duration::from_millis(10);

#[cfg(target_os = "macos")]
impl FifoReader {
    fn new(descriptor: std::os::fd::OwnedFd) -> io::Result<Self> {
        Ok(Self {
            io: tokio::io::unix::AsyncFd::with_interest(
                descriptor.into(),
                tokio::io::Interest::READABLE,
            )?,
            recheck: Box::pin(tokio::time::sleep(EOF_RECHECK)),
        })
    }

    fn read_into(file: &std::fs::File, buffer: &mut tokio::io::ReadBuf<'_>) -> io::Result<()> {
        use std::io::Read;

        let mut file = file;
        let count = file.read(buffer.initialize_unfilled())?;
        buffer.advance(count);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl tokio::io::AsyncRead for FifoReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::future::Future;
        use std::task::Poll;

        let this = self.get_mut();
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Read buffered bytes and EOF directly, including after a short read.
        match Self::read_into(this.io.get_ref(), buffer) {
            Ok(()) => return Poll::Ready(Ok(())),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Poll::Ready(Err(error)),
        }
        loop {
            if let Poll::Ready(guard) = this.io.poll_read_ready(context) {
                if let Ok(result) = guard?.try_io(|io| Self::read_into(io.get_ref(), buffer)) {
                    return Poll::Ready(result);
                }
                continue;
            }
            // Readiness will never report the end; look for it ourselves.
            if this.recheck.as_mut().poll(context).is_pending() {
                return Poll::Pending;
            }
            this.recheck
                .as_mut()
                .reset(tokio::time::Instant::now() + EOF_RECHECK);
            match Self::read_into(this.io.get_ref(), buffer) {
                Ok(()) => return Poll::Ready(Ok(())),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}
