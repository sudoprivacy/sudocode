//! A TEST-ONLY fault boundary around a real node backend.
//!
//! Every fault here is INJECTED by this file and nowhere else: the daemon, the
//! gRPC client, the mailbox and the lease code under test are all real. The
//! wrapper sits between the mailbox and the real [`FsBackend`], so what it can
//! fake is exactly what a flaky link or a partition would look like from this
//! process, while the node keeps its own clock and its own lease table.
//!
//! It also records what the receiver seat (`…/receiver.lock`) and the
//! conversation readers' locks did, with monotonic instants, so a test can
//! assert on timing it would otherwise have to guess.

use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use runtime::fs_backend::{
    FsBackend, FsDirEntry, FsLock, FsMetadata, ManagedRoot, VersionToken, WriteOutcome,
};

/// What this boundary does to traffic right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Pass everything through.
    None,
    /// INJECTED: renewing the receiver seat fails at once with an I/O error and
    /// never reaches the node. Nothing else is touched.
    SeatRenewFailsFast,
    /// INJECTED, one shot: the next seat renewal really reaches the node and
    /// really succeeds, but its reply is held back this long before it returns.
    SeatRenewReturnsLate(Duration),
    /// INJECTED: the next this-many renewals of a conversation reader's lock
    /// (not the seat) fail at once with an I/O error and never reach the node.
    ReaderRenewFails(u32),
    /// INJECTED: this process cannot reach the node at all. Every call fails at
    /// once, and a lock dropped meanwhile is never released on the node, so the
    /// node can only expire it by its own TTL.
    Partitioned,
}

/// How one seat renewal ended, as the lease code saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The node renewed it.
    Held,
    /// The node said the lock is no longer ours.
    Lost,
    /// The node, or the link to it, failed for real.
    Error,
    /// This boundary failed it; the node never saw the attempt.
    Injected,
}

#[derive(Clone, Copy, Debug)]
pub struct Renewal {
    pub requested: Instant,
    pub returned: Instant,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug)]
pub struct Release {
    pub requested: Instant,
    pub returned: Instant,
    pub reached_node: bool,
}

/// Hold completion observation after the real lock has been released.
/// Dropping the gate also unblocks the releasing thread on assertion failure.
pub struct ReleaseGate {
    released: mpsc::Receiver<()>,
    _finish: mpsc::Sender<()>,
}

impl ReleaseGate {
    pub fn wait_until_released(&self, within: Duration) {
        self.released
            .recv_timeout(within)
            .expect("the real seat release to finish before its observation is unblocked");
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LockEvent {
    Acquired(Instant),
    Released(Release),
    Renewed(Renewal),
}

/// The switch a test flips, and what it observed.
pub struct Probe {
    fault: Mutex<Fault>,
    seat: Mutex<Vec<LockEvent>>,
    reader: Mutex<Vec<LockEvent>>,
    /// INJECTED, one shot: hold the next claim of a conversation reader's lock
    /// this long before it reaches the node.
    reader_claim_delay: Mutex<Option<Duration>>,
    seat_release_gate: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
    /// Locks dropped while partitioned. Held, not released, so the node keeps
    /// them until their TTL runs out.
    leaked: Mutex<Vec<Arc<dyn FsLock>>>,
}

impl Probe {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            fault: Mutex::new(Fault::None),
            seat: Mutex::new(Vec::new()),
            reader: Mutex::new(Vec::new()),
            reader_claim_delay: Mutex::new(None),
            seat_release_gate: Mutex::new(None),
            leaked: Mutex::new(Vec::new()),
        })
    }

    pub fn set(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    fn fault(&self) -> Fault {
        *self.fault.lock().unwrap()
    }

    fn partitioned(&self) -> bool {
        self.fault() == Fault::Partitioned
    }

    /// Consume the one-shot late reply, if armed.
    fn take_late(&self) -> Option<Duration> {
        let mut fault = self.fault.lock().unwrap();
        match *fault {
            Fault::SeatRenewReturnsLate(delay) => {
                *fault = Fault::None;
                Some(delay)
            }
            _ => None,
        }
    }

    /// Arm a one-shot INJECTED delay on the next reader lock claim.
    pub fn delay_next_reader_claim(&self, delay: Duration) {
        *self.reader_claim_delay.lock().unwrap() = Some(delay);
    }

    /// Arm a one-shot completion gate without delaying the real node release.
    pub fn hold_next_seat_release(&self) -> ReleaseGate {
        let (released_tx, released) = mpsc::channel();
        let (finish, finish_rx) = mpsc::channel();
        let mut gate = self.seat_release_gate.lock().unwrap();
        assert!(gate.is_none(), "a seat release gate is already armed");
        *gate = Some((released_tx, finish_rx));
        ReleaseGate {
            released,
            _finish: finish,
        }
    }

    /// Consume one armed reader renewal failure, if any.
    fn take_reader_failure(&self) -> bool {
        let mut fault = self.fault.lock().unwrap();
        match *fault {
            Fault::ReaderRenewFails(left) => {
                *fault = if left > 1 {
                    Fault::ReaderRenewFails(left - 1)
                } else {
                    Fault::None
                };
                true
            }
            _ => false,
        }
    }

    fn record(&self, seat: bool, event: LockEvent) {
        if seat { &self.seat } else { &self.reader }
            .lock()
            .unwrap()
            .push(event);
    }

    pub fn seat_events(&self) -> Vec<LockEvent> {
        self.seat.lock().unwrap().clone()
    }

    pub fn reader_events(&self) -> Vec<LockEvent> {
        self.reader.lock().unwrap().clone()
    }

    /// Whether this instance currently holds the seat, as far as it knows.
    pub fn holds_seat(&self) -> bool {
        let events = self.seat_events();
        let acquired = events
            .iter()
            .filter(|e| matches!(e, LockEvent::Acquired(_)))
            .count();
        let released = events
            .iter()
            .filter(|e| matches!(e, LockEvent::Released(_)))
            .count();
        acquired > released
    }

    pub fn acquisitions(&self) -> Vec<Instant> {
        self.seat_events()
            .into_iter()
            .filter_map(|e| match e {
                LockEvent::Acquired(at) => Some(at),
                _ => None,
            })
            .collect()
    }

    pub fn releases(&self) -> Vec<Release> {
        self.seat_events()
            .into_iter()
            .filter_map(|e| match e {
                LockEvent::Released(release) => Some(release),
                _ => None,
            })
            .collect()
    }

    pub fn reader_acquisitions(&self) -> Vec<Instant> {
        self.reader_events()
            .into_iter()
            .filter_map(|e| match e {
                LockEvent::Acquired(at) => Some(at),
                _ => None,
            })
            .collect()
    }

    pub fn reader_releases(&self) -> Vec<Release> {
        self.reader_events()
            .into_iter()
            .filter_map(|e| match e {
                LockEvent::Released(release) => Some(release),
                _ => None,
            })
            .collect()
    }

    pub fn reader_renewals(&self) -> Vec<Renewal> {
        self.reader_events()
            .into_iter()
            .filter_map(|e| match e {
                LockEvent::Renewed(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    pub fn renewals(&self) -> Vec<Renewal> {
        self.seat_events()
            .into_iter()
            .filter_map(|e| match e {
                LockEvent::Renewed(r) => Some(r),
                _ => None,
            })
            .collect()
    }
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("INJECTED by the test fault boundary: {what}"))
}

fn is_seat(path: &str) -> bool {
    path.ends_with("/receiver.lock")
}

struct FaultyLock {
    inner: Option<Arc<dyn FsLock>>,
    probe: Arc<Probe>,
    seat: bool,
}

impl FsLock for FaultyLock {
    fn renew(&self) -> io::Result<bool> {
        let requested = Instant::now();
        let fail_fast = self.probe.partitioned()
            || (self.seat && self.probe.fault() == Fault::SeatRenewFailsFast)
            || (!self.seat && self.probe.take_reader_failure());
        if fail_fast {
            self.probe.record(
                self.seat,
                LockEvent::Renewed(Renewal {
                    requested,
                    returned: Instant::now(),
                    outcome: Outcome::Injected,
                }),
            );
            return Err(injected("renewal"));
        }
        let result = self.inner.as_ref().expect("lock held").renew();
        if self.seat {
            if let Some(delay) = self.probe.take_late() {
                std::thread::sleep(delay);
            }
        }
        self.probe.record(
            self.seat,
            LockEvent::Renewed(Renewal {
                requested,
                returned: Instant::now(),
                outcome: match result {
                    Ok(true) => Outcome::Held,
                    Ok(false) => Outcome::Lost,
                    Err(_) => Outcome::Error,
                },
            }),
        );
        result
    }
}

impl Drop for FaultyLock {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let requested = Instant::now();
        let reached_node = !self.probe.partitioned();
        if reached_node {
            drop(inner);
            let gate = if self.seat {
                self.probe.seat_release_gate.lock().unwrap().take()
            } else {
                None
            };
            if let Some((released, finish)) = gate {
                let _ = released.send(());
                let _ = finish.recv();
            }
        } else {
            self.probe.leaked.lock().unwrap().push(inner);
        }
        self.probe.record(
            self.seat,
            LockEvent::Released(Release {
                requested,
                returned: Instant::now(),
                reached_node,
            }),
        );
    }
}

/// A real backend, behind the switch in its [`Probe`].
pub struct FaultyBackend {
    inner: Arc<dyn FsBackend>,
    probe: Arc<Probe>,
}

impl FaultyBackend {
    pub fn new(inner: Arc<dyn FsBackend>, probe: Arc<Probe>) -> Self {
        Self { inner, probe }
    }

    fn reachable(&self) -> io::Result<()> {
        if self.probe.partitioned() {
            Err(injected("partitioned from the node"))
        } else {
            Ok(())
        }
    }
}

impl FsBackend for FaultyBackend {
    fn fenced_locks(&self) -> bool {
        self.inner.fenced_locks()
    }
    fn try_lock(&self, path: &str, ttl_ms: u64) -> io::Result<Option<Arc<dyn FsLock>>> {
        self.reachable()?;
        if !is_seat(path) {
            if let Some(delay) = self.probe.reader_claim_delay.lock().unwrap().take() {
                std::thread::sleep(delay);
            }
        }
        let Some(lock) = self.inner.try_lock(path, ttl_ms)? else {
            return Ok(None);
        };
        let seat = is_seat(path);
        self.probe.record(seat, LockEvent::Acquired(Instant::now()));
        Ok(Some(Arc::new(FaultyLock {
            inner: Some(lock),
            probe: Arc::clone(&self.probe),
            seat,
        })))
    }
    // Conditional writes are forwarded, not left to the trait's Unsupported
    // default: this boundary injects only the faults named above, so it must
    // not change what the backend underneath can do.
    fn read_versioned(&self, path: &str) -> io::Result<Option<(Vec<u8>, VersionToken)>> {
        self.reachable()?;
        self.inner.read_versioned(path)
    }
    fn write_if_version(
        &self,
        path: &str,
        expected: Option<&VersionToken>,
        data: &[u8],
    ) -> io::Result<WriteOutcome> {
        self.reachable()?;
        self.inner.write_if_version(path, expected, data)
    }
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.reachable()?;
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.reachable()?;
        self.inner.write(path, data)
    }
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.reachable()?;
        self.inner.append(path, data)
    }
    fn delete(&self, path: &str) -> io::Result<()> {
        self.reachable()?;
        self.inner.delete(path)
    }
    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        self.reachable()?;
        self.inner.stat(path)
    }
    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        self.reachable()?;
        self.inner.readdir(path)
    }
    fn exists(&self, path: &str) -> io::Result<bool> {
        self.reachable()?;
        self.inner.exists(path)
    }
    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.reachable()?;
        self.inner.create_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.reachable()?;
        self.inner.rename(from, to)
    }
    fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.reachable()?;
        self.inner.canonicalize(path)
    }
    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        self.reachable()?;
        self.inner.symlink_metadata(path)
    }
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.reachable()?;
        self.inner.read_to_string(path)
    }
    fn write_atomic(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.reachable()?;
        self.inner.write_atomic(path, data)
    }
    fn create_append_log(&self, path: &str, retention: u64) -> io::Result<()> {
        self.reachable()?;
        self.inner.create_append_log(path, retention)
    }
    fn is_append_stream(&self, path: &str) -> io::Result<bool> {
        self.reachable()?;
        self.inner.is_append_stream(path)
    }
    fn managed_root(&self, concern: ManagedRoot) -> Option<String> {
        self.inner.managed_root(concern)
    }
    fn link(&self, alias: &str, target: &str) -> io::Result<()> {
        self.reachable()?;
        self.inner.link(alias, target)
    }
    fn read_link(&self, alias: &str) -> io::Result<String> {
        self.reachable()?;
        self.inner.read_link(alias)
    }
    fn working_root(&self) -> io::Result<String> {
        self.inner.working_root()
    }
    fn normalize(&self, path: &str) -> io::Result<String> {
        self.inner.normalize(path)
    }
    fn normalize_allow_missing(&self, path: &str) -> io::Result<String> {
        self.inner.normalize_allow_missing(path)
    }
    fn join_path(&self, dir: &str, name: &str) -> String {
        self.inner.join_path(dir, name)
    }
    fn tail_read(
        &self,
        path: &str,
        cursor: u64,
        block_ms: u64,
    ) -> io::Result<(Vec<u8>, u64, bool)> {
        self.reachable()?;
        self.inner.tail_read(path, cursor, block_ms)
    }
}
