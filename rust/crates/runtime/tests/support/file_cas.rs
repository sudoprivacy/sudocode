//! A test-only [`FsBackend`] with a node's lock and write semantics, on host
//! files, usable from several processes at once.
//!
//! Host files normally take the disk path: an OS lock that excludes a frozen
//! holder until it exits (`fenced_locks() == true`). A node is different: its
//! locks are leases that expire, and the S6 cursor is protected only by a
//! conditional write. This backend reproduces that on a temp directory so the
//! fencing logic can be exercised without a daemon:
//!
//! - `fenced_locks()` is `false`, and `try_lock` hands out leases that expire
//!   (`<lock>` holds `{owner, expires_at}`, guarded by an OS lock on
//!   `<lock>.guard`). [`expire_leases_under`] ends every lease at once, the way
//!   a holder frozen past its TTL would see it.
//! - `read_versioned` / `write_if_version` keep a never-reused generation in
//!   `<path>.ver`, compared and replaced under an OS lock on `<path>.casguard`.
//! - A gate directory, when given, holds a conditional write while
//!   `<gate>/hold` exists, after touching `<gate>/entered`, and touches
//!   `<gate>/left` once the held write has returned. That parks a holder
//!   between its lease check and its write, the window fencing has to cover,
//!   and tells the test when the parked write has been decided.
//! - [`FileCasBackend::inject_unknown`] makes the next conditional write answer
//!   `Unknown`, with or without committing it.
//!
//! Everything else is the host filesystem, unchanged.
//!
//! Included with `#[path = "support/file_cas.rs"] mod file_cas;`.
#![allow(dead_code)]

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::fs_backend::{
    FsBackend, FsDirEntry, FsLock, FsMetadata, StdFsBackend, VersionToken, WriteOutcome,
};

/// No fault armed.
const UNKNOWN_OFF: u8 = 0;
/// The next conditional write commits, then answers `Unknown`.
const UNKNOWN_AFTER_COMMIT: u8 = 1;
/// The next conditional write answers `Unknown` without committing.
const UNKNOWN_WITHOUT_COMMIT: u8 = 2;

pub struct FileCasBackend {
    inner: StdFsBackend,
    conditional_writes: bool,
    gate: Option<PathBuf>,
    unknown: AtomicU8,
}

impl FileCasBackend {
    /// Expiring leases and conditional writes.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: StdFsBackend,
            conditional_writes: true,
            gate: None,
            unknown: AtomicU8::new(UNKNOWN_OFF),
        }
    }

    /// Expiring leases and no conditional write: a node before N2.
    #[must_use]
    pub fn without_conditional_writes() -> Self {
        Self {
            conditional_writes: false,
            ..Self::new()
        }
    }

    /// Hold conditional writes at `gate` while `<gate>/hold` exists.
    #[must_use]
    pub fn with_gate(mut self, gate: impl Into<PathBuf>) -> Self {
        self.gate = Some(gate.into());
        self
    }

    /// Make the next conditional write answer `Unknown`; it commits first when
    /// `committed` is true.
    pub fn inject_unknown(&self, committed: bool) {
        let mode = if committed {
            UNKNOWN_AFTER_COMMIT
        } else {
            UNKNOWN_WITHOUT_COMMIT
        };
        self.unknown.store(mode, Ordering::SeqCst);
    }

    /// Park while the gate is held. Returns the gate when this call was
    /// parked, so the caller can mark it left.
    fn wait_at_gate(&self) -> Option<&Path> {
        let gate = self.gate.as_deref()?;
        let hold = gate.join("hold");
        if !hold.exists() {
            return None;
        }
        let _ = fs::write(gate.join("entered"), b"");
        while hold.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(gate)
    }

    fn conditional_write(
        &self,
        path: &str,
        expected: Option<&VersionToken>,
        data: &[u8],
    ) -> io::Result<WriteOutcome> {
        let fault = self.unknown.swap(UNKNOWN_OFF, Ordering::SeqCst);
        if fault == UNKNOWN_WITHOUT_COMMIT {
            return Ok(WriteOutcome::Unknown);
        }
        let _guard = os_lock(&sidecar(path, ".casguard"))?;
        let current = current_generation(path)?;
        if current.map(token).as_ref() != expected {
            return Ok(WriteOutcome::Conflict {
                current: current.map(token),
            });
        }
        // The generation counter survives the data, so it is never reused.
        let last = fs::read_to_string(sidecar(path, ".ver"))
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .or(current)
            .unwrap_or(0);
        let next = last + 1;
        self.inner.write_atomic(path, data)?;
        fs::write(sidecar(path, ".ver"), next.to_string())?;
        if fault == UNKNOWN_AFTER_COMMIT {
            return Ok(WriteOutcome::Unknown);
        }
        Ok(WriteOutcome::Committed(token(next)))
    }
}

/// Hold a test receiver's delivery while `<hold>` exists, after touching
/// `<hold>.entered`. Lets a test stop a holder after its claim and before its
/// commit.
pub fn wait_while_held(hold: &Path) {
    if !hold.exists() {
        return;
    }
    let mut entered = hold.as_os_str().to_owned();
    entered.push(".entered");
    let _ = fs::write(PathBuf::from(entered), b"");
    while hold.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
}

impl Default for FileCasBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// End every lease under `root` now, as if each holder had been frozen past
/// its TTL. Holders are not told; they find out on their next renewal.
pub fn expire_leases_under(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            expire_leases_under(&path);
        } else if path.extension().is_some_and(|e| e == "lock") {
            let _guard = os_lock(&guard_path(&path));
            if let Some(mut lease) = read_lease(&path) {
                lease.expires_at = 0;
                let _ = write_lease(&path, &lease);
            }
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Lease {
    owner: String,
    expires_at: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn guard_path(lock: &Path) -> PathBuf {
    let mut name = lock.as_os_str().to_owned();
    name.push(".guard");
    PathBuf::from(name)
}

fn sidecar(path: &str, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{path}{suffix}"))
}

/// An exclusive OS lock on `path`, held until the returned file drops.
fn os_lock(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

fn read_lease(path: &Path) -> Option<Lease> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_lease(path: &Path, lease: &Lease) -> io::Result<()> {
    fs::write(path, serde_json::to_vec(lease).map_err(io::Error::other)?)
}

fn unique_owner() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_ms(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

struct FileLease {
    path: PathBuf,
    owner: String,
    ttl_ms: u64,
}

impl FsLock for FileLease {
    fn renew(&self) -> io::Result<bool> {
        let _guard = os_lock(&guard_path(&self.path))?;
        match read_lease(&self.path) {
            Some(lease) if lease.owner == self.owner && lease.expires_at > now_ms() => {
                write_lease(
                    &self.path,
                    &Lease {
                        owner: self.owner.clone(),
                        expires_at: now_ms() + self.ttl_ms,
                    },
                )?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl Drop for FileLease {
    fn drop(&mut self) {
        let Ok(_guard) = os_lock(&guard_path(&self.path)) else {
            return;
        };
        if read_lease(&self.path).is_some_and(|lease| lease.owner == self.owner) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn current_generation(path: &str) -> io::Result<Option<u64>> {
    if !Path::new(path).exists() {
        return Ok(None);
    }
    match fs::read_to_string(sidecar(path, ".ver")) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        // Written before this backend versioned it: the first generation.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Some(0)),
        Err(e) => Err(e),
    }
}

fn token(generation: u64) -> VersionToken {
    VersionToken(generation.to_string().into_bytes())
}

impl FsBackend for FileCasBackend {
    fn fenced_locks(&self) -> bool {
        false
    }

    fn try_lock(&self, path: &str, ttl_ms: u64) -> io::Result<Option<Arc<dyn FsLock>>> {
        let lock = PathBuf::from(path);
        let _guard = os_lock(&guard_path(&lock))?;
        if read_lease(&lock).is_some_and(|lease| lease.expires_at > now_ms()) {
            return Ok(None);
        }
        let owner = unique_owner();
        write_lease(
            &lock,
            &Lease {
                owner: owner.clone(),
                expires_at: now_ms() + ttl_ms,
            },
        )?;
        Ok(Some(Arc::new(FileLease {
            path: lock,
            owner,
            ttl_ms,
        })))
    }

    fn read_versioned(&self, path: &str) -> io::Result<Option<(Vec<u8>, VersionToken)>> {
        if !self.conditional_writes {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "test backend without conditional writes",
            ));
        }
        let _guard = os_lock(&sidecar(path, ".casguard"))?;
        match current_generation(path)? {
            Some(generation) => Ok(Some((fs::read(path)?, token(generation)))),
            None => Ok(None),
        }
    }

    fn write_if_version(
        &self,
        path: &str,
        expected: Option<&VersionToken>,
        data: &[u8],
    ) -> io::Result<WriteOutcome> {
        if !self.conditional_writes {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "test backend without conditional writes",
            ));
        }
        let parked = self.wait_at_gate();
        let outcome = self.conditional_write(path, expected, data);
        if let Some(gate) = parked {
            let _ = fs::write(gate.join("left"), b"");
        }
        outcome
    }

    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.inner.write(path, data)
    }
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.inner.append(path, data)
    }
    fn delete(&self, path: &str) -> io::Result<()> {
        self.inner.delete(path)
    }
    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        self.inner.stat(path)
    }
    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        self.inner.readdir(path)
    }
    fn exists(&self, path: &str) -> io::Result<bool> {
        self.inner.exists(path)
    }
    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.inner.canonicalize(path)
    }
    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        self.inner.symlink_metadata(path)
    }
    fn write_atomic(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.inner.write_atomic(path, data)
    }
    fn create_append_log(&self, path: &str, retention: u64) -> io::Result<()> {
        self.inner.create_append_log(path, retention)
    }
    fn is_append_stream(&self, path: &str) -> io::Result<bool> {
        self.inner.is_append_stream(path)
    }
    fn link(&self, alias: &str, target: &str) -> io::Result<()> {
        self.inner.link(alias, target)
    }
    fn read_link(&self, alias: &str) -> io::Result<String> {
        self.inner.read_link(alias)
    }
    fn tail_read(
        &self,
        path: &str,
        cursor: u64,
        block_ms: u64,
    ) -> io::Result<(Vec<u8>, u64, bool)> {
        self.inner.tail_read(path, cursor, block_ms)
    }
}
