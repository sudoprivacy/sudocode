use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type Gate = Arc<(Mutex<bool>, Condvar)>;

/// A transparent TCP tunnel whose forwarding can pause while sockets stay open.
pub struct PausedTransport {
    address: SocketAddr,
    gate: Gate,
    stopped: Arc<AtomicBool>,
    sockets: Arc<Mutex<Vec<TcpStream>>>,
    worker: Option<JoinHandle<()>>,
    blocked: Arc<(Mutex<u64>, Condvar)>,
}

impl PausedTransport {
    pub fn new(endpoint: &str) -> io::Result<Self> {
        let endpoint = endpoint
            .strip_prefix("https://")
            .or_else(|| endpoint.strip_prefix("http://"))
            .unwrap_or(endpoint);
        let target = endpoint
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::other("missing daemon address"))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let blocked = Arc::new((Mutex::new(0), Condvar::new()));
        let worker = {
            let gate = Arc::clone(&gate);
            let stopped = Arc::clone(&stopped);
            let sockets = Arc::clone(&sockets);
            let blocked = Arc::clone(&blocked);
            thread::spawn(move || {
                let mut pipes = Vec::new();
                while !stopped.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((incoming, _)) => {
                            let upstream =
                                TcpStream::connect_timeout(&target, Duration::from_secs(1))
                                    .expect("connect owned test daemon");
                            for stream in [&incoming, &upstream] {
                                stream
                                    .set_read_timeout(Some(Duration::from_millis(100)))
                                    .unwrap();
                                sockets.lock().unwrap().push(stream.try_clone().unwrap());
                            }
                            for (source, destination) in [
                                (incoming.try_clone().unwrap(), upstream.try_clone().unwrap()),
                                (upstream, incoming),
                            ] {
                                let gate = Arc::clone(&gate);
                                let stopped = Arc::clone(&stopped);
                                let blocked = Arc::clone(&blocked);
                                pipes.push(thread::spawn(move || {
                                    forward(source, destination, &gate, &stopped, &blocked);
                                }));
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept test transport: {error}"),
                    }
                }
                for pipe in pipes {
                    pipe.join().expect("test tunnel worker");
                }
            })
        };
        Ok(Self {
            address,
            gate,
            stopped,
            sockets,
            worker: Some(worker),
            blocked,
        })
    }

    pub fn endpoint(&self) -> String {
        self.address.to_string()
    }

    pub fn set_paused(&self, paused: bool) {
        *self.gate.0.lock().unwrap() = paused;
        self.gate.1.notify_all();
    }

    /// Wait for bytes actually read from a socket and held at the closed gate.
    /// This is an IO barrier, not a statement about which encrypted RPC it is.
    #[allow(dead_code)]
    pub fn wait_for_blocked(&self, budget: Duration) -> bool {
        let (count, _) = self
            .blocked
            .1
            .wait_timeout_while(self.blocked.0.lock().unwrap(), budget, |count| *count == 0)
            .unwrap();
        *count > 0
    }
}

impl Drop for PausedTransport {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.set_paused(false);
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().expect("test transport cleanup");
        }
    }
}

fn forward(
    mut source: TcpStream,
    mut destination: TcpStream,
    gate: &Gate,
    stopped: &AtomicBool,
    blocked: &(Mutex<u64>, Condvar),
) {
    let mut buffer = [0; 16 * 1024];
    while !stopped.load(Ordering::Acquire) {
        let size = match source.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => size,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(_) => break,
        };
        let mut paused = gate.0.lock().unwrap();
        if *paused {
            *blocked.0.lock().unwrap() += 1;
            blocked.1.notify_all();
        }
        while *paused && !stopped.load(Ordering::Acquire) {
            paused = gate.1.wait(paused).unwrap();
        }
        drop(paused);
        if stopped.load(Ordering::Acquire) || destination.write_all(&buffer[..size]).is_err() {
            break;
        }
    }
    let _ = destination.shutdown(Shutdown::Write);
}
