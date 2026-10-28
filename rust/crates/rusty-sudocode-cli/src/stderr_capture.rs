//! Capture process stderr for the current terminal owner.
//!
//! Renderers drain these bytes through their own output handle and the shared
//! UTF-8 decoder. External terminal owners suspend capture; drop restores stderr.

#[cfg(any(unix, windows))]
pub(crate) use platform::StderrRedirect;

/// Unix pipe-based stderr redirect: replaces fd 2 with a pipe so that
/// any library writing to stderr (e.g. tracing, nix, openssl) does not
/// corrupt the iocraft canvas.  Captured bytes are drained each tick and
/// routed through the iocraft stdout handle.
///
/// All syscalls go through `nix`'s safe wrappers — no `unsafe` blocks.
#[cfg(unix)]
mod platform {
    use std::io::Read;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};

    pub struct StderrRedirect {
        read_file: std::fs::File,
        write_fd: OwnedFd,
        original_stderr: OwnedFd,
    }

    impl StderrRedirect {
        /// Replace fd 2 with the write end of a pipe.  Returns `Some` on
        /// success.  The read end is kept for `drain()`.
        pub fn activate() -> Option<Self> {
            let (read_fd, write_fd): (OwnedFd, OwnedFd) = nix::unistd::pipe().ok()?;
            let original_stderr = std::io::stderr().as_fd().try_clone_to_owned().ok()?;

            // Make the read end non-blocking so drain() never stalls.
            let flags =
                nix::fcntl::fcntl(read_fd.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL).ok()?;
            let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
            oflags.insert(nix::fcntl::OFlag::O_NONBLOCK);
            nix::fcntl::fcntl(read_fd.as_raw_fd(), nix::fcntl::FcntlArg::F_SETFL(oflags)).ok()?;

            let read_file = std::fs::File::from(read_fd);
            nix::unistd::dup2(write_fd.as_raw_fd(), 2).ok()?;
            Some(Self {
                read_file,
                write_fd,
                original_stderr,
            })
        }

        /// External programs inherit the real terminal, not the capture pipe.
        #[inline]
        pub fn suspend(&mut self) -> std::io::Result<()> {
            nix::unistd::dup2(self.original_stderr.as_raw_fd(), 2)?;
            Ok(())
        }

        #[inline]
        pub fn resume(&mut self) -> std::io::Result<()> {
            nix::unistd::dup2(self.write_fd.as_raw_fd(), 2)?;
            Ok(())
        }

        /// Non-blocking drain: returns captured text or `None`.
        pub fn drain(&self) -> Option<Vec<u8>> {
            let mut buf = [0u8; 4096];
            let mut collected = Vec::new();
            let mut file = &self.read_file;
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => collected.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if collected.is_empty() {
                None
            } else {
                Some(collected)
            }
        }
    }

    impl Drop for StderrRedirect {
        fn drop(&mut self) {
            let _ = self.suspend();
        }
    }
}

/// Win32 stderr cannot be made nonblocking like a Unix pipe. One reader drains
/// the real pipe into a bounded queue; the UI tick never waits for pipe IO.
/// Pipe creation reuses the PTY dependency. Standard-handle ownership uses std
/// OwnedHandle and the existing Win32 wrapper crate, without pipe-type probes.
#[cfg(windows)]
mod platform {
    use std::io::{self, Read};
    use std::os::windows::io::{AsHandle, OwnedHandle};
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, OnceLock};
    use std::thread::JoinHandle;

    use filedescriptor::Pipe;

    const CAPTURE_CHUNKS: usize = 16;
    const CHUNK_BYTES: usize = 4096;

    pub struct StderrRedirect {
        original: OwnedHandle,
        writer: Option<OwnedHandle>,
        active: bool,
        captured: Option<Receiver<Vec<u8>>>,
        reader: Option<JoinHandle<()>>,
        wake: Arc<OnceLock<std::thread::Thread>>,
    }

    impl StderrRedirect {
        pub fn activate() -> Option<Self> {
            // Standard streams can share a Win32 handle (for example 2>&1).
            // Own a duplicate, never the handle still installed on stdout.
            let original = io::stderr().as_handle().try_clone_to_owned().ok()?;
            let Pipe { mut read, write } = Pipe::new().ok()?;
            let writer: OwnedHandle = write.as_file().ok()?.into();
            drop(write);
            let (tx, rx) = mpsc::sync_channel(CAPTURE_CHUNKS);
            let wake = Arc::new(OnceLock::<std::thread::Thread>::new());
            let notify = Arc::clone(&wake);
            let reader = std::thread::Builder::new()
                .name("repl-stderr".into())
                .spawn(move || {
                    let mut buf = [0; CHUNK_BYTES];
                    loop {
                        match read.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if tx.send(buf[..n].to_vec()).is_err() {
                                    break;
                                }
                                if let Some(worker) = notify.get() {
                                    worker.unpark();
                                }
                            }
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Err(_) => break,
                        }
                    }
                })
                .ok()?;
            let Ok(previous) = redirect(&writer) else {
                drop(writer);
                drop(rx);
                let _ = reader.join();
                return None;
            };
            Some(Self {
                original: previous.unwrap_or(original),
                writer: Some(writer),
                active: true,
                captured: Some(rx),
                reader: Some(reader),
                wake,
            })
        }

        /// Classic output waits on its forwarding thread; iocraft retains its
        /// existing UI tick. Both drain the same bounded capture queue.
        #[inline]
        pub fn wake_on_data(&self, worker: std::thread::Thread) {
            let _ = self.wake.set(worker);
        }

        #[inline]
        pub fn suspend(&mut self) -> io::Result<()> {
            if self.active {
                let previous = redirect(&self.original)?;
                // std OwnedHandle drops call CloseHandle without probing the
                // pipe. The displaced handle stays alive; dropping the other
                // duplicate neither closes the last writer nor queries it.
                if let Some(previous) = previous {
                    self.writer = Some(previous);
                }
                self.active = false;
            }
            Ok(())
        }

        #[inline]
        pub fn resume(&mut self) -> io::Result<()> {
            if !self.active {
                if let Some(writer) = self.writer.as_ref() {
                    if let Some(previous) = redirect(writer)? {
                        self.original = previous;
                    }
                    self.active = true;
                }
            }
            Ok(())
        }

        pub fn drain(&self) -> Option<Vec<u8>> {
            let rx = self.captured.as_ref()?;
            let mut bytes = Vec::new();
            // Bound each tick even if another library continuously writes.
            for chunk in rx.try_iter().take(CAPTURE_CHUNKS) {
                bytes.extend_from_slice(&chunk);
            }
            (!bytes.is_empty()).then_some(bytes)
        }
    }

    #[inline]
    fn redirect(source: &OwnedHandle) -> io::Result<Option<OwnedHandle>> {
        stdin_peek::replace_stderr_handle(source.as_handle())
    }

    impl Drop for StderrRedirect {
        fn drop(&mut self) {
            let _ = self.suspend();
            // Release the queue before the writer, including a reader parked
            // on a full queue. EOF normally ends the reader immediately. Do not
            // block UI shutdown waiting for a library's retained stderr clone.
            drop(self.captured.take());
            drop(self.writer.take());
            drop(self.reader.take());
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct StderrRedirect;

#[cfg(not(any(unix, windows)))]
impl StderrRedirect {
    #[inline]
    pub fn activate() -> Option<Self> {
        None
    }
    #[inline]
    pub fn drain(&self) -> Option<Vec<u8>> {
        None
    }
    #[inline]
    pub fn suspend(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    #[inline]
    pub fn resume(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
