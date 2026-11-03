//! A conversation reader's durable position, written only by its current
//! holder.
//!
//! A reader holds its conversation through a backend lock and records how far
//! it has read in a register beside the transcript. On a backend whose lock
//! excludes a previous holder until that process exits, a plain store is
//! already safe: a frozen holder still owns the lock, so nobody else writes.
//! An expiring lease is weaker. A holder frozen past its lease can resume
//! after another instance has taken over and overwrite the newer position with
//! its older one, moving the cursor backwards.
//!
//! Where the backend offers an atomic conditional write, every write here is
//! conditional on the version this holder last wrote. A new holder first
//! writes a fence (the next epoch) against the version it read, so a resumed
//! holder's next write finds a newer version and is refused: nothing visible
//! changes, its delivery stays unacknowledged, and it goes back through
//! acquire. Nobody ever forces a write, and there is no fallback to an
//! unconditional one.
//!
//! Which of the three applies is decided once, when the reader is claimed,
//! and reported as a [`CursorGuarantee`].

use std::io;
use std::sync::Arc;

use crate::fs_backend::{FsBackend, VersionToken, WriteOutcome};
use crate::mailbox::ReaderRegister;

/// What protects a reader's position from a holder that resumed after losing
/// its seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorGuarantee {
    /// The lock excludes a previous holder until it exits, so a plain store is
    /// already fenced.
    Fenced,
    /// Every write is conditional on this holder's last version.
    FencedCas,
    /// Neither: an expiring lease and no conditional write. Delivery is
    /// at-least-once, and a holder frozen past its lease may move the cursor
    /// back on resume.
    AtLeastOnce,
}

impl CursorGuarantee {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fenced => "fenced",
            Self::FencedCas => "fenced_cas",
            Self::AtLeastOnce => "at_least_once",
        }
    }
}

/// The register a claimed reader writes, and how.
pub(crate) struct RegisterCursor {
    backend: Arc<dyn FsBackend>,
    path: String,
    holder: String,
    epoch: u64,
    /// The version this holder last wrote, when writes are conditional.
    token: Option<VersionToken>,
    guarantee: CursorGuarantee,
}

impl RegisterCursor {
    /// Which claim this cursor writes as.
    ///
    /// Exposed for diagnostics only — a delivery stall is attributable to the
    /// claim that failed to advance, and the tail loop cannot name it otherwise
    /// once the register is reached through this type.
    pub(crate) fn holder(&self) -> &str {
        &self.holder
    }

    /// Take the register at `path` for `holder`, returning the cursor and the
    /// offset to resume from. `Ok(None)` means another claimer won this round;
    /// the caller retries the way it would after losing the lock.
    ///
    /// Call it only while holding the reader's lock.
    pub(crate) fn claim(
        backend: Arc<dyn FsBackend>,
        path: String,
        holder: String,
        lease_expires_at: u64,
    ) -> Result<Option<(Self, u64)>, String> {
        let plain = |guarantee| -> Result<Option<(Self, u64)>, String> {
            let offset = load_plain(backend.as_ref(), &path)?.map_or(0, |r| r.read_offset);
            let cursor = Self {
                backend: Arc::clone(&backend),
                path: path.clone(),
                holder: holder.clone(),
                epoch: 0,
                token: None,
                guarantee,
            };
            cursor.store_plain(offset, lease_expires_at)?;
            Ok(Some((cursor, offset)))
        };
        if backend.fenced_locks() {
            return plain(CursorGuarantee::Fenced);
        }
        let current = match backend.read_versioned(&path) {
            Ok(current) => current,
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                return plain(CursorGuarantee::AtLeastOnce)
            }
            Err(e) => return Err(format!("read reader register {path}: {e}")),
        };
        let (previous, expected) = match current {
            Some((raw, token)) => (parse(&path, &raw)?, Some(token)),
            None => (None, None),
        };
        let previous = previous.unwrap_or_default();
        let epoch = previous
            .epoch
            .checked_add(1)
            .ok_or_else(|| format!("reader register {path}: claim epoch exhausted"))?;
        let fence = ReaderRegister {
            holder: holder.clone(),
            lease_expires_at,
            read_offset: previous.read_offset,
            epoch,
        };
        let token = match backend
            .write_if_version(&path, expected.as_ref(), &encode(&path, &fence)?)
            .map_err(|e| format!("fence reader register {path}: {e}"))?
        {
            WriteOutcome::Committed(token) => token,
            WriteOutcome::Conflict { .. } => return Ok(None),
            // Proceed only if what is there now is this very fence.
            WriteOutcome::Unknown => match read_back(backend.as_ref(), &path)? {
                Some((register, token)) if same_fence(&register, &fence) => token,
                _ => return Ok(None),
            },
        };
        let offset = fence.read_offset;
        Ok(Some((
            Self {
                backend,
                path,
                holder,
                epoch: fence.epoch,
                token: Some(token),
                guarantee: CursorGuarantee::FencedCas,
            },
            offset,
        )))
    }

    #[must_use]
    pub(crate) fn guarantee(&self) -> CursorGuarantee {
        self.guarantee
    }

    /// Record `offset` as read.
    ///
    /// # Errors
    /// The write failed, or on a conditional cursor a newer holder fenced this
    /// one out. Either way the delivery stays unacknowledged.
    pub(crate) fn commit(&mut self, offset: u64, lease_expires_at: u64) -> Result<(), String> {
        let Some(expected) = self.token.clone() else {
            return self.store_plain(offset, lease_expires_at);
        };
        let register = ReaderRegister {
            holder: self.holder.clone(),
            lease_expires_at,
            read_offset: offset,
            epoch: self.epoch,
        };
        let path = &self.path;
        let fenced_out = || {
            format!(
                "reader {path} was fenced out by a newer holder; delivery remains unacknowledged"
            )
        };
        let token = match self
            .backend
            .write_if_version(path, Some(&expected), &encode(path, &register)?)
            .map_err(|e| format!("write reader register {path}: {e}"))?
        {
            WriteOutcome::Committed(token) => token,
            WriteOutcome::Conflict { .. } => return Err(fenced_out()),
            WriteOutcome::Unknown => match read_back(self.backend.as_ref(), path)? {
                Some((stored, token)) if stored == register => token,
                Some((stored, _)) if !same_holder(&stored, &register) => return Err(fenced_out()),
                _ => {
                    return Err(format!(
                        "reader register {path}: commit outcome unknown and not visible; \
                         delivery remains unacknowledged"
                    ))
                }
            },
        };
        self.token = Some(token);
        Ok(())
    }

    fn store_plain(&self, offset: u64, lease_expires_at: u64) -> Result<(), String> {
        let path = &self.path;
        // Cursor replacement must not replace the inode holding the OS lock.
        if let Some((dir, _)) = path.rsplit_once('/') {
            self.backend
                .create_dir_all(dir)
                .map_err(|e| format!("create reader directory {dir}: {e}"))?;
        }
        let register = ReaderRegister {
            holder: self.holder.clone(),
            lease_expires_at,
            read_offset: offset,
            epoch: 0,
        };
        self.backend
            .write_atomic(path, &encode(path, &register)?)
            .map_err(|e| format!("write reader register {path}: {e}"))
    }
}

/// The register at `path` as a plain read, or `None` if this agent has never
/// read the conversation.
pub(crate) fn load_plain(
    backend: &dyn FsBackend,
    path: &str,
) -> Result<Option<ReaderRegister>, String> {
    match backend.read(path) {
        Ok(raw) => parse(path, &raw),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read reader register {path}: {e}")),
    }
}

fn read_back(
    backend: &dyn FsBackend,
    path: &str,
) -> Result<Option<(ReaderRegister, VersionToken)>, String> {
    match backend
        .read_versioned(path)
        .map_err(|e| format!("re-read reader register {path}: {e}"))?
    {
        Some((raw, token)) => Ok(parse(path, &raw)?.map(|register| (register, token))),
        None => Ok(None),
    }
}

fn parse(path: &str, raw: &[u8]) -> Result<Option<ReaderRegister>, String> {
    if raw.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(raw)
        .map(Some)
        .map_err(|e| format!("reader register {path}: {e}"))
}

fn encode(path: &str, register: &ReaderRegister) -> Result<Vec<u8>, String> {
    serde_json::to_vec(register).map_err(|e| format!("encode reader register {path}: {e}"))
}

fn same_holder(a: &ReaderRegister, b: &ReaderRegister) -> bool {
    a.holder == b.holder && a.epoch == b.epoch
}

fn same_fence(stored: &ReaderRegister, fence: &ReaderRegister) -> bool {
    same_holder(stored, fence) && stored.read_offset == fence.read_offset
}
