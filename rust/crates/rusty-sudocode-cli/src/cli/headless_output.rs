//! A bounded stdout writer. A stalled consumer cannot pin the engine thread.
use std::io::{self, Write};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

type Packet = (Vec<u8>, mpsc::Sender<io::Result<()>>);

pub(super) struct Output {
    tx: mpsc::Sender<Packet>,
    signal: Arc<AtomicI32>,
    failed: bool,
}
impl Output {
    pub(super) fn new(signal: Arc<AtomicI32>) -> Self {
        let (tx, rx) = mpsc::channel::<Packet>();
        std::thread::spawn(move || {
            while let Ok((bytes, ack)) = rx.recv() {
                let mut out = io::stdout().lock();
                let result = out.write_all(&bytes).and_then(|()| out.flush());
                let failed = result.is_err();
                let _ = ack.send(result);
                if failed {
                    break;
                }
            }
        });
        Self {
            tx,
            signal,
            failed: false,
        }
    }
}
impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout is unavailable",
            ));
        }
        let (tx, rx) = mpsc::channel();
        self.tx
            .send((buf.to_vec(), tx))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stdout writer disconnected"))?;
        let started = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(())) => return Ok(buf.len()),
                Ok(Err(error)) => {
                    self.failed = true;
                    return Err(error);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.failed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stdout writer disconnected",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if started.elapsed() > Duration::from_secs(30)
                        || self.signal.load(Ordering::SeqCst) != 0
                    {
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "stdout consumer stalled or task cancelled",
                        ));
                    }
                }
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
