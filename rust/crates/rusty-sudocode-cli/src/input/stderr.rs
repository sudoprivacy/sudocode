//! Deliver captured stderr through the line editor which owns the prompt.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
#[cfg(not(unix))]
use std::time::Duration;

use rustyline::ExternalPrinter;

use crate::render::output::Utf8Stream;
use crate::stderr_capture::StderrRedirect;

/// Wait for either captured bytes or a notice/stop notification. Retaining both
/// socket ends with every sender also keeps late notifications safe after exit.
#[cfg(unix)]
struct Readiness {
    read: std::os::unix::net::UnixStream,
    write: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl Readiness {
    #[inline]
    fn new() -> std::io::Result<Self> {
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok(Self { read, write })
    }

    #[inline]
    fn notify(&self) {
        use std::io::Write;
        // WouldBlock means an earlier notification already makes poll ready.
        while matches!((&self.write).write(&[1]), Err(error) if error.kind() == std::io::ErrorKind::Interrupted)
        {
        }
    }

    #[inline]
    fn clear(&self) {
        use std::io::Read;
        let mut bytes = [0; 256];
        for _ in 0..16 {
            if !matches!((&self.read).read(&mut bytes), Ok(1..)) {
                break;
            }
        }
    }

    #[inline]
    fn wait(&self, capture: &std::os::fd::OwnedFd) -> std::io::Result<()> {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        use std::os::fd::AsFd;

        let mut descriptors = [
            PollFd::new(capture.as_fd(), PollFlags::POLLIN),
            PollFd::new(self.read.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut descriptors, PollTimeout::NONE) {
            Err(nix::errno::Errno::EINTR) => return Ok(()),
            result => result?,
        };
        if descriptors.iter().any(|descriptor| {
            descriptor.revents().is_some_and(|flags| {
                flags.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
            })
        }) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        Ok(())
    }
}

/// Peer notices must not wait for the editor's bounded print queue: the
/// receiver is joined before that editor is dropped. Only display text is
/// queued here; the mailbox still owns delivery and acknowledgment.
#[derive(Clone)]
pub(crate) struct BackgroundPrinter {
    notices: mpsc::Sender<String>,
    #[cfg(not(unix))]
    wake: std::thread::Thread,
    #[cfg(unix)]
    readiness: Arc<Readiness>,
}

impl BackgroundPrinter {
    #[inline]
    pub(crate) fn print(&self, text: String) -> Result<(), mpsc::SendError<String>> {
        self.notices.send(text)?;
        #[cfg(unix)]
        self.readiness.notify();
        #[cfg(not(unix))]
        self.wake.unpark();
        Ok(())
    }
}

pub(super) struct StderrOutput {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    printer: BackgroundPrinter,
    redirect: Arc<Mutex<StderrRedirect>>,
}

impl StderrOutput {
    pub(super) fn activate(mut printer: impl ExternalPrinter + Send + 'static) -> Option<Self> {
        let mut redirect = StderrRedirect::activate()?;
        // Outside readline the caller or its child owns the terminal.
        redirect.suspend().ok()?;
        #[cfg(unix)]
        let descriptor = redirect.poll_handle().ok()?;
        #[cfg(unix)]
        let readiness = Arc::new(Readiness::new().ok()?);
        #[cfg(unix)]
        let ready = Arc::clone(&readiness);
        let redirect = Arc::new(Mutex::new(redirect));
        let captured = Arc::clone(&redirect);
        let (notices, pending) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("readline-stderr".into())
            .spawn(move || {
                let mut utf8 = Utf8Stream::default();
                loop {
                    // Clear before checking the channel so a racing sender
                    // cannot lose its wake between the drain and poll.
                    #[cfg(unix)]
                    ready.clear();
                    let mut progressed = false;
                    // One pending notice per blocked peer delivery, never the
                    // peer message body. Keep each drain finite.
                    for notice in pending.try_iter().take(16) {
                        progressed = true;
                        if printer.print(notice).is_err() {
                            return;
                        }
                    }
                    let bytes = captured
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .drain();
                    if let Some(bytes) = bytes {
                        progressed = true;
                        let text = utf8.push(&bytes);
                        if !text.is_empty() && printer.print(text).is_err() {
                            break;
                        }
                    }
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    if !progressed {
                        #[cfg(unix)]
                        if ready.wait(&descriptor).is_err() {
                            break;
                        }
                        #[cfg(not(unix))]
                        std::thread::park_timeout(Duration::from_millis(80));
                    }
                }
            })
            .ok()?;
        #[cfg(windows)]
        redirect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wake_on_data(worker.thread().clone());
        let printer = BackgroundPrinter {
            notices,
            #[cfg(not(unix))]
            wake: worker.thread().clone(),
            #[cfg(unix)]
            readiness,
        };
        Some(Self {
            stop,
            worker: Some(worker),
            printer,
            redirect,
        })
    }

    #[inline]
    pub(super) fn printer(&self) -> BackgroundPrinter {
        self.printer.clone()
    }

    #[inline]
    pub(super) fn set_active(&self, active: bool) -> std::io::Result<()> {
        // A writer may still hold stderr while blocked on the old capture
        // pipe after suspension. Keep the redirect mutex available to the
        // drain worker until that writer releases stderr.
        let _stderr = active.then(|| std::io::stderr().lock());
        let mut redirect = self
            .redirect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active {
            // Finish any write to the uncaptured terminal before readline
            // paints its prompt. Suspension never waits for stderr.
            redirect.resume()
        } else {
            redirect.suspend()
        }
    }
}

impl Drop for StderrOutput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.set_active(false);
        #[cfg(unix)]
        self.printer.readiness.notify();
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}
