//! Deliver captured stderr through the line editor which owns the prompt.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustyline::ExternalPrinter;

use crate::render::output::Utf8Stream;
use crate::stderr_capture::StderrRedirect;

/// Peer notices must not wait for the editor's bounded print queue: the
/// receiver is joined before that editor is dropped. Only display text is
/// queued here; the mailbox still owns delivery and acknowledgment.
#[derive(Clone)]
pub(crate) struct BackgroundPrinter {
    notices: mpsc::Sender<String>,
    wake: std::thread::Thread,
}

impl BackgroundPrinter {
    #[inline]
    pub(crate) fn print(&self, text: String) -> Result<(), mpsc::SendError<String>> {
        self.notices.send(text)?;
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
            wake: worker.thread().clone(),
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
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}
