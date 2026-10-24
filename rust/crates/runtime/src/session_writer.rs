//! Serialize durable commits while independent runtimes keep their contexts.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::fs_backend::{FsBackend, FsLock, RecordLogMode};
use crate::session::history::ContextHistory;

pub struct SessionWriter {
    backend: CommitBackend,
    transcript: PathBuf,
    cursor: Mutex<Option<String>>,
    deleted: AtomicBool,
    writes: Mutex<()>,
    presence: Option<WriterPresence>,
    history: Mutex<Option<ActiveHistory>>,
    _legacy_owner: Option<Arc<dyn FsLock>>,
}

struct ActiveHistory {
    context: ContextHistory,
    view_revision: u64,
}

struct WriterPresence {
    fs: Arc<dyn FsBackend>,
    path: PathBuf,
    _lock: Arc<dyn FsLock>,
}

impl Drop for WriterPresence {
    fn drop(&mut self) {
        // Each writer owns a fresh UUID; another writer never reuses this path.
        let _ = self.fs.delete(&self.path.to_string_lossy());
    }
}

enum CommitBackend {
    Native(Arc<dyn FsBackend>),
    Stream(Arc<dyn FsBackend>),
}

pub struct SessionWriteGuard<'a> {
    _operation: MutexGuard<'a, ()>,
    _storage: Option<Arc<dyn FsLock>>,
    _owners: Vec<Arc<dyn FsLock>>,
}

pub(crate) struct SessionPurgeGuard {
    _storage: Arc<dyn FsLock>,
    _owners: Vec<Arc<dyn FsLock>>,
}

impl SessionWriter {
    pub fn acquire(fs: Arc<dyn FsBackend>, transcript: &Path) -> Result<Arc<Self>, String> {
        Self::acquire_native(fs, transcript, true)
    }

    /// A short-lived metadata writer shares the SID commit barrier without
    /// claiming the legacy runtime's exclusive message-writing admission.
    pub(crate) fn acquire_maintenance(
        fs: Arc<dyn FsBackend>,
        transcript: &Path,
    ) -> Result<Arc<Self>, String> {
        Self::acquire_native(fs, transcript, false)
    }

    fn acquire_native(
        fs: Arc<dyn FsBackend>,
        transcript: &Path,
        exclusive: bool,
    ) -> Result<Arc<Self>, String> {
        if !fs.fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        let guard = Self::commit_lock(fs.as_ref(), transcript)?;
        let legacy_owner = exclusive
            .then(|| {
                fs.try_lock(
                    &transcript
                        .with_extension("legacy-writer.lock")
                        .to_string_lossy(),
                    30_000,
                )
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "session_writer_busy".to_owned())
            })
            .transpose()?;
        crate::subagent_completion::prepare_shared_storage(fs.as_ref(), transcript)?;
        let path = transcript.to_string_lossy();
        if fs.exists(&path).map_err(|error| error.to_string())?
            && fs.stat(&path).map_err(|error| error.to_string())?.len > 0
        {
            let bytes = fs.read(&path).map_err(|error| error.to_string())?;
            let contents =
                crate::session_history::decode(&bytes).map_err(|error| error.to_string())?;
            let shared = crate::Session::shared_history_suffix(contents)?.is_some();
            if !shared {
                let converted = crate::Session::upgrade_legacy_transcript(contents)
                    .map_err(|error| error.to_string())?;
                fs.write_atomic(&path, converted.as_bytes())
                    .map_err(|error| error.to_string())?;
            }
        }
        let owners = transcript.with_extension("writers");
        fs.create_dir_all(&owners.to_string_lossy())
            .map_err(|error| error.to_string())?;
        let path = owners.join(format!("{}.lock", uuid::Uuid::new_v4()));
        let presence = WriterPresence {
            _lock: fs
                .try_lock(&path.to_string_lossy(), 30_000)
                .map_err(|error| error.to_string())?
                .ok_or("session_writer_busy")?,
            fs: Arc::clone(&fs),
            path,
        };
        drop(guard);
        Ok(Arc::new(Self {
            backend: CommitBackend::Native(fs),
            transcript: transcript.to_path_buf(),
            cursor: Mutex::default(),
            deleted: AtomicBool::new(false),
            writes: Mutex::new(()),
            presence: Some(presence),
            history: Mutex::default(),
            _legacy_owner: legacy_owner,
        }))
    }

    pub fn acquire_stream(
        fs: Arc<dyn FsBackend>,
        transcript: &Path,
        retention_bytes: u64,
    ) -> Result<Arc<Self>, String> {
        let path = transcript.to_string_lossy();
        fs.create_append_log(&path, retention_bytes)
            .map_err(|error| error.to_string())?;
        if !fs
            .is_append_stream(&path)
            .map_err(|error| error.to_string())?
        {
            return Err("shared_session_requires_atomic_append_storage".into());
        }
        let contents = fs
            .read_to_string(&path)
            .map_err(|error| error.to_string())?;
        if !contents.trim().is_empty()
            && crate::Session::shared_history_suffix(&contents)?.is_none()
        {
            let converted = crate::Session::upgrade_legacy_transcript(&contents)
                .map_err(|error| error.to_string())?;
            fs.append(&path, converted.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        Ok(Arc::new(Self {
            backend: CommitBackend::Stream(fs),
            transcript: transcript.to_path_buf(),
            cursor: Mutex::default(),
            deleted: AtomicBool::new(false),
            writes: Mutex::new(()),
            presence: None,
            history: Mutex::default(),
            _legacy_owner: None,
        }))
    }

    /// Fresh sessions use a dedicated durable record log. Existing logs may
    /// only reopen after the reader has recognized their initialization record.
    pub(crate) fn acquire_history(
        fs: Arc<dyn FsBackend>,
        transcript: &Path,
        seed: &crate::Session,
        source: Option<&str>,
        view_revision: u64,
    ) -> Result<Arc<Self>, String> {
        let native = fs.fenced_locks();
        let _guard = native
            .then(|| Self::commit_lock(fs.as_ref(), transcript))
            .transpose()?;
        if let Some(parent) = transcript.parent() {
            fs.create_dir_all(&parent.to_string_lossy())
                .map_err(|error| error.to_string())?;
        }
        let path = transcript.to_string_lossy();
        let mode = if fs.exists(&path).map_err(|error| error.to_string())? {
            let bytes = fs.read(&path).map_err(|error| error.to_string())?;
            if !crate::session::history::is_record_history(&bytes) {
                return Err("session history initialization binding missing".into());
            }
            RecordLogMode::Existing
        } else {
            RecordLogMode::CreateNew
        };
        if native {
            crate::subagent_completion::prepare_shared_storage(fs.as_ref(), transcript)?;
        }
        let log = fs
            .record_log(&path, mode)
            .map_err(|error| error.to_string())?;
        let context =
            ContextHistory::open(log, Some(seed), source).map_err(|error| error.to_string())?;
        if context
            .session()
            .map_err(|error| error.to_string())?
            .session_id
            != seed.session_id
        {
            return Err("session history identity mismatch".into());
        }
        let presence = if native {
            let owners = transcript.with_extension("writers");
            fs.create_dir_all(&owners.to_string_lossy())
                .map_err(|error| error.to_string())?;
            let path = owners.join(format!("{}.lock", uuid::Uuid::new_v4()));
            Some(WriterPresence {
                _lock: fs
                    .try_lock(&path.to_string_lossy(), 30_000)
                    .map_err(|error| error.to_string())?
                    .ok_or("session_writer_busy")?,
                fs: Arc::clone(&fs),
                path,
            })
        } else {
            None
        };
        Ok(Arc::new(Self {
            backend: if native {
                CommitBackend::Native(fs)
            } else {
                CommitBackend::Stream(fs)
            },
            transcript: transcript.to_path_buf(),
            cursor: Mutex::default(),
            deleted: AtomicBool::new(false),
            writes: Mutex::new(()),
            presence,
            history: Mutex::new(Some(ActiveHistory {
                context,
                view_revision,
            })),
            _legacy_owner: None,
        }))
    }

    pub(crate) fn history_session(&self) -> Result<Option<crate::Session>, String> {
        self.history
            .lock()
            .map_err(|_| "session history poisoned")?
            .as_ref()
            .map(|active| active.context.session().map_err(|error| error.to_string()))
            .transpose()
    }

    pub(crate) fn has_history(&self) -> Result<bool, String> {
        self.history
            .lock()
            .map(|history| history.is_some())
            .map_err(|_| "session history poisoned".into())
    }

    pub(crate) fn message_head(&self) -> Result<Option<String>, String> {
        let history = self
            .history
            .lock()
            .map_err(|_| "session history poisoned")?;
        if let Some(active) = &*history {
            return active
                .context
                .message_head()
                .map(|id| id.map(str::to_owned))
                .map_err(|error| error.to_string());
        }
        drop(history);
        self.history_head()
    }

    pub(crate) fn append_history(
        &self,
        message: &crate::ConversationMessage,
        session: &crate::Session,
    ) -> Result<bool, String> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| "session history poisoned")?;
        let Some(active) = &mut *history else {
            return Ok(false);
        };
        active
            .context
            .append(message, session)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    pub(crate) fn update_history(
        &self,
        message: &crate::ConversationMessage,
    ) -> Result<bool, String> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| "session history poisoned")?;
        let Some(active) = &mut *history else {
            return Ok(false);
        };
        active
            .context
            .update(message)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    pub(crate) fn save_history(
        &self,
        session: &crate::Session,
        view_revision: u64,
    ) -> Result<bool, String> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| "session history poisoned")?;
        let Some(active) = &mut *history else {
            return Ok(false);
        };
        active
            .context
            .save(session, active.view_revision != view_revision)
            .map_err(|error| error.to_string())?;
        active.view_revision = view_revision;
        Ok(true)
    }

    pub(crate) fn finish_history(&self, turn_id: &str) -> Result<(), String> {
        if let Some(active) = &mut *self
            .history
            .lock()
            .map_err(|_| "session history poisoned")?
        {
            active
                .context
                .finish(turn_id)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn observe_head(&self, head: Option<String>) -> Result<(), String> {
        *self
            .cursor
            .lock()
            .map_err(|_| "session history cursor poisoned")? = head;
        Ok(())
    }

    pub(crate) fn history_head(&self) -> Result<Option<String>, String> {
        if let Some(active) = &*self
            .history
            .lock()
            .map_err(|_| "session history poisoned")?
        {
            return active
                .context
                .revision()
                .map(|id| Some(id.into()))
                .map_err(|error| error.to_string());
        }
        self.cursor
            .lock()
            .map(|head| head.clone())
            .map_err(|_| "session history cursor poisoned".into())
    }

    pub(crate) fn commit_lock(
        fs: &dyn FsBackend,
        transcript: &Path,
    ) -> Result<Arc<dyn FsLock>, String> {
        let started = std::time::Instant::now();
        let path = transcript.with_extension("writer.lock");
        loop {
            if let Some(lock) = fs
                .try_lock(&path.to_string_lossy(), 30_000)
                .map_err(|error| error.to_string())?
            {
                return Ok(lock);
            }
            if started.elapsed() >= std::time::Duration::from_secs(5) {
                return Err("session_commit_busy".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if self.deleted.load(Ordering::Acquire) {
            return Err("session_deleted".into());
        }
        match &self.backend {
            CommitBackend::Native(fs) => {
                crate::subagent_completion::check_shared_storage(fs.as_ref(), &self.transcript)?;
            }
            CommitBackend::Stream(fs)
                if !fs
                    .is_append_stream(&self.transcript.to_string_lossy())
                    .map_err(|error| error.to_string())? =>
            {
                return Err("session_storage_lost".into())
            }
            _ => {}
        }
        Ok(())
    }

    /// Serialize transcript writes with journal deletion for this lifetime.
    /// Checking the tombstone before taking this mutex leaves a deletion race.
    pub fn write(&self) -> Result<SessionWriteGuard<'_>, String> {
        let guard = self.writes.lock().map_err(|_| "session writer poisoned")?;
        let storage = if let CommitBackend::Native(fs) = &self.backend {
            Some(Self::commit_lock(fs.as_ref(), &self.transcript)?)
        } else {
            None
        };
        self.check()?;
        if let CommitBackend::Native(fs) = &self.backend {
            fs.recover_append_log(&self.transcript.to_string_lossy())
                .map_err(|error| error.to_string())?;
        }
        Ok(SessionWriteGuard {
            _operation: guard,
            _storage: storage,
            _owners: Vec::new(),
        })
    }

    pub(crate) fn purge_write(&self) -> Result<SessionWriteGuard<'_>, String> {
        let mut guard = self.write()?;
        if let CommitBackend::Native(fs) = &self.backend {
            guard._owners = Self::exclude_writers(
                fs.as_ref(),
                &self.transcript,
                self.presence
                    .as_ref()
                    .map(|presence| presence.path.as_path()),
            )?;
        }
        Ok(guard)
    }

    pub(crate) fn purge_lock(
        fs: &dyn FsBackend,
        transcript: &Path,
    ) -> Result<SessionPurgeGuard, String> {
        let storage = Self::commit_lock(fs, transcript)?;
        let owners = Self::exclude_writers(fs, transcript, None)?;
        Ok(SessionPurgeGuard {
            _storage: storage,
            _owners: owners,
        })
    }

    // The caller holds the SID commit lock, excluding new owner registration.
    fn exclude_writers(
        fs: &dyn FsBackend,
        transcript: &Path,
        own: Option<&Path>,
    ) -> Result<Vec<Arc<dyn FsLock>>, String> {
        if !fs.fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        let directory = transcript.with_extension("writers");
        if !fs
            .exists(&directory.to_string_lossy())
            .map_err(|error| error.to_string())?
        {
            return Ok(Vec::new());
        }
        let mut owners = Vec::new();
        for entry in fs
            .readdir(&directory.to_string_lossy())
            .map_err(|error| error.to_string())?
        {
            if entry.is_dir
                || Path::new(&entry.name).extension() != Some(std::ffi::OsStr::new("lock"))
            {
                continue;
            }
            let path = directory.join(entry.name);
            if own == Some(path.as_path()) {
                continue;
            }
            owners.push(
                fs.try_lock(&path.to_string_lossy(), 30_000)
                    .map_err(|error| error.to_string())?
                    .ok_or("session_writer_busy")?,
            );
        }
        Ok(owners)
    }

    pub fn retire(&self) {
        self.deleted.store(true, Ordering::Release);
    }
}
