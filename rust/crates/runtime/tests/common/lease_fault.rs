//! A TEST-ONLY fault boundary around a real node backend.
//!
//! Every fault here is INJECTED by this file and nowhere else: the daemon, the
//! gRPC client, the mailbox and the lease code under test are all real. The
//! wrapper sits between the mailbox and the real [`FsBackend`], so what it can
//! fake is exactly what a flaky link or a partition would look like from this
//! process, while the node keeps its own clock and its own lease table.
//!
//! It also records what the receiver seat (`…/receiver.lock`) did, with
//! monotonic instants, so a test can assert on timing it would otherwise have
//! to guess.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use runtime::fs_backend::{FsBackend, FsDirEntry, FsLock, FsMetadata, ManagedRoot};

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
pub enum SeatEvent {
    Acquired(Instant),
    Released { at: Instant, reached_node: bool },
    Renewed(Renewal),
}

/// The switch a test flips, and what it observed.
pub struct Probe {
    fault: Mutex<Fault>,
    seat: Mutex<Vec<SeatEvent>>,
    /// Locks dropped while partitioned. Held, not released, so the node keeps
    /// them until their TTL runs out.
    leaked: Mutex<Vec<Arc<dyn FsLock>>>,
}

impl Probe {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            fault: Mutex::new(Fault::None),
            seat: Mutex::new(Vec::new()),
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

    fn record(&self, event: SeatEvent) {
        self.seat.lock().unwrap().push(event);
    }

    pub fn seat_events(&self) -> Vec<SeatEvent> {
        self.seat.lock().unwrap().clone()
    }

    /// Whether this instance currently holds the seat, as far as it knows.
    pub fn holds_seat(&self) -> bool {
        let events = self.seat_events();
        let acquired = events
            .iter()
            .filter(|e| matches!(e, SeatEvent::Acquired(_)))
            .count();
        let released = events
            .iter()
            .filter(|e| matches!(e, SeatEvent::Released { .. }))
            .count();
        acquired > released
    }

    pub fn acquisitions(&self) -> Vec<Instant> {
        self.seat_events()
            .into_iter()
            .filter_map(|e| match e {
                SeatEvent::Acquired(at) => Some(at),
                _ => None,
            })
            .collect()
    }

    pub fn releases(&self) -> Vec<(Instant, bool)> {
        self.seat_events()
            .into_iter()
            .filter_map(|e| match e {
                SeatEvent::Released { at, reached_node } => Some((at, reached_node)),
                _ => None,
            })
            .collect()
    }

    pub fn renewals(&self) -> Vec<Renewal> {
        self.seat_events()
            .into_iter()
            .filter_map(|e| match e {
                SeatEvent::Renewed(r) => Some(r),
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
            || (self.seat && self.probe.fault() == Fault::SeatRenewFailsFast);
        if fail_fast {
            if self.seat {
                self.probe.record(SeatEvent::Renewed(Renewal {
                    requested,
                    returned: Instant::now(),
                    outcome: Outcome::Injected,
                }));
            }
            return Err(injected("renewal"));
        }
        let result = self.inner.as_ref().expect("lock held").renew();
        if self.seat {
            if let Some(delay) = self.probe.take_late() {
                std::thread::sleep(delay);
            }
            self.probe.record(SeatEvent::Renewed(Renewal {
                requested,
                returned: Instant::now(),
                outcome: match result {
                    Ok(true) => Outcome::Held,
                    Ok(false) => Outcome::Lost,
                    Err(_) => Outcome::Error,
                },
            }));
        }
        result
    }
}

impl Drop for FaultyLock {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let reached_node = !self.probe.partitioned();
        if reached_node {
            drop(inner);
        } else {
            self.probe.leaked.lock().unwrap().push(inner);
        }
        if self.seat {
            self.probe.record(SeatEvent::Released {
                at: Instant::now(),
                reached_node,
            });
        }
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
        let Some(lock) = self.inner.try_lock(path, ttl_ms)? else {
            return Ok(None);
        };
        let seat = is_seat(path);
        if seat {
            self.probe.record(SeatEvent::Acquired(Instant::now()));
        }
        Ok(Some(Arc::new(FaultyLock {
            inner: Some(lock),
            probe: Arc::clone(&self.probe),
            seat,
        })))
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
