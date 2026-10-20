//! An exact transcript and its completion journal have one writer lifetime.
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::fs_backend::{FsBackend, FsLock};

pub const JOURNAL_VERSION: u32 = 2;

pub struct SessionWriter {
    lock: Arc<dyn FsLock>,
    deleted: AtomicBool,
    writes: Mutex<()>,
}

impl SessionWriter {
    pub fn acquire(fs: &dyn FsBackend, transcript: &Path) -> Result<Arc<Self>, String> {
        if !fs.fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        // A v1 writer never took this lock. Acquiring it cannot authorize a
        // live conversion. This unreleased development format is preserved and rejected;
        // the first released completion journal uses the shared writer protocol.
        Self::check_journal(fs, transcript)?;
        let lock = fs
            .try_lock(
                &transcript.with_extension("writer.lock").to_string_lossy(),
                30_000,
            )
            .map_err(|e| e.to_string())?
            .ok_or("session_writer_busy")?;
        Self::check_journal(fs, transcript)?;
        Ok(Self::from_lock(lock))
    }

    /// Response attachments have their own writer namespace. Cooperating
    /// transcript writers remain free to update history and billing metadata.
    #[inline]
    pub(crate) fn acquire_response_archive(
        fs: &dyn FsBackend,
        transcript: &Path,
    ) -> Result<Arc<Self>, String> {
        if !fs.fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        Self::check_journal(fs, transcript)?;
        let path = transcript.with_extension("responses.lock");
        let deadline = Instant::now() + Duration::from_secs(1);
        let lock = loop {
            if let Some(lock) = fs
                .try_lock(&path.to_string_lossy(), 30_000)
                .map_err(|error| error.to_string())?
            {
                break lock;
            }
            if Instant::now() >= deadline {
                return Err("session_writer_busy".into());
            }
            // A just-completed response can still be finalizing its gzip tail.
            // The worker does not need the transcript lock held by purge.
            std::thread::sleep(Duration::from_millis(5));
        };
        Self::check_journal(fs, transcript)?;
        Ok(Self::from_lock(lock))
    }

    #[inline]
    fn from_lock(lock: Arc<dyn FsLock>) -> Arc<Self> {
        Arc::new(Self {
            lock,
            deleted: AtomicBool::new(false),
            writes: Mutex::new(()),
        })
    }

    #[inline]
    fn check_journal(fs: &dyn FsBackend, transcript: &Path) -> Result<(), String> {
        let journal = transcript.with_extension("completions.json");
        if fs
            .exists(&journal.to_string_lossy())
            .map_err(|error| error.to_string())?
        {
            let value: serde_json::Value = serde_json::from_slice(
                &fs.read(&journal.to_string_lossy())
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            if value["deleted"].is_string() {
                return Err("session_deleted".into());
            }
            if value["version"].as_u64() != Some(u64::from(JOURNAL_VERSION)) {
                return Err("legacy_writer_unconfirmed".into());
            }
            crate::subagent_completion::validate_journal(value)?;
        }
        Ok(())
    }

    pub fn check(&self) -> Result<(), String> {
        if self.deleted.load(Ordering::Acquire) {
            return Err("session_deleted".into());
        }
        if !self.lock.renew().map_err(|e| e.to_string())? {
            return Err("session_writer_lost".into());
        }
        Ok(())
    }

    /// Serialize transcript writes with journal deletion for this lifetime.
    /// Checking the tombstone before taking this mutex leaves a deletion race.
    pub fn write(&self) -> Result<MutexGuard<'_, ()>, String> {
        let guard = self.writes.lock().map_err(|_| "session writer poisoned")?;
        self.check()?;
        Ok(guard)
    }

    pub fn retire(&self) {
        self.deleted.store(true, Ordering::Release);
    }
}
