//! Bounded, lossless response evidence on the session's own filesystem.
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use flate2::{write::GzEncoder, Compression};
use serde_json::json;
use telemetry::{ResponseArchive, ResponseCaptureEnd, ResponseCaptureMetadata, ResponseRecorder};

use crate::fs_backend::FsLock;
use crate::json::JsonValue;
use crate::{ConfigError, FsBackend, Session};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseRecordingConfig {
    pub enabled: bool,
    pub max_response_bytes: u64,
    pub max_session_bytes: u64,
}

impl Default for ResponseRecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_response_bytes: 32 * 1024 * 1024,
            max_session_bytes: 256 * 1024 * 1024,
        }
    }
}

impl ResponseRecordingConfig {
    pub(crate) fn parse(value: Option<&JsonValue>) -> Result<Self, ConfigError> {
        let mut config = Self::default();
        let Some(value) = value else {
            return Ok(config);
        };
        let object = value
            .as_object()
            .ok_or_else(|| ConfigError::Parse("responseRecording must be an object".into()))?;
        if let Some(value) = object.get("enabled") {
            config.enabled = value.as_bool().ok_or_else(|| {
                ConfigError::Parse("responseRecording.enabled must be boolean".into())
            })?;
        }
        for (key, destination) in [
            ("maxResponseBytes", &mut config.max_response_bytes),
            ("maxSessionBytes", &mut config.max_session_bytes),
        ] {
            if let Some(value) = object.get(key) {
                let number = value.as_i64().filter(|value| *value > 0).ok_or_else(|| {
                    ConfigError::Parse(format!(
                        "responseRecording.{key} must be a positive integer"
                    ))
                })?;
                *destination =
                    u64::try_from(number).map_err(|error| ConfigError::Parse(error.to_string()))?;
            }
        }
        Ok(config)
    }
}

/// Exclude response attachment deletion until every recorder has finalized.
/// This lock never takes the transcript commit barrier.
pub(crate) struct ResponseArchiveWriter {
    lock: Arc<dyn FsLock>,
    writes: Mutex<()>,
}

impl ResponseArchiveWriter {
    pub(crate) fn acquire(fs: &dyn FsBackend, transcript: &Path) -> Result<Arc<Self>, String> {
        if !fs.fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        crate::subagent_completion::check_response_storage(fs, transcript)?;
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
            // A completed response can still be finalizing its gzip tail.
            std::thread::sleep(Duration::from_millis(5));
        };
        crate::subagent_completion::check_response_storage(fs, transcript)?;
        Ok(Arc::new(Self {
            lock,
            writes: Mutex::new(()),
        }))
    }

    fn check(&self) -> Result<(), String> {
        if !self.lock.renew().map_err(|error| error.to_string())? {
            return Err("session_writer_lost".into());
        }
        Ok(())
    }

    fn write(&self) -> Result<MutexGuard<'_, ()>, String> {
        let guard = self.writes.lock().map_err(|_| "response writer poisoned")?;
        self.check()?;
        Ok(guard)
    }
}

pub struct SessionResponseArchive {
    fs: Arc<dyn FsBackend>,
    writer: Mutex<Weak<ResponseArchiveWriter>>,
    transcript: PathBuf,
    directory: PathBuf,
    session_id: String,
    config: ResponseRecordingConfig,
    used: Arc<Mutex<Option<u64>>>,
}

impl SessionResponseArchive {
    pub fn for_session(
        session: &Session,
        config: &ResponseRecordingConfig,
    ) -> io::Result<Option<Arc<dyn ResponseArchive>>> {
        if !config.enabled {
            return Ok(None);
        }
        let Some(path) = session.persistence_path() else {
            return Ok(None);
        };
        let directory = Session::response_captures_dir_for(path, &session.session_id)
            .ok_or_else(|| io::Error::other("session response directory unavailable"))?;
        Ok(Some(Arc::new(Self {
            fs: session.fs_handle(),
            writer: Mutex::new(Weak::new()),
            transcript: path.to_path_buf(),
            directory,
            session_id: session.session_id.clone(),
            config: config.clone(),
            used: Arc::new(Mutex::new(None)),
        })))
    }

    #[inline]
    fn capture_writer(&self) -> io::Result<Arc<ResponseArchiveWriter>> {
        let mut current = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("response writer poisoned"))?;
        if let Some(writer) = current.upgrade() {
            writer.check().map_err(io::Error::other)?;
            return Ok(writer);
        }
        if !self.fs.exists(&self.transcript.to_string_lossy())? {
            return Err(io::Error::other("session_deleted"));
        }
        let writer = ResponseArchiveWriter::acquire(self.fs.as_ref(), &self.transcript)
            .map_err(io::Error::other)?;
        *current = Arc::downgrade(&writer);
        Ok(writer)
    }

    fn initialize_budget(&self) -> io::Result<()> {
        let mut used = self
            .used
            .lock()
            .map_err(|_| io::Error::other("response budget poisoned"))?;
        if used.is_some() {
            return Ok(());
        }
        let directory = self.directory.to_string_lossy();
        let mut bytes = 0_u64;
        let entries = match self.fs.readdir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error),
        };
        for entry in entries {
            if !entry.is_dir {
                bytes = bytes.saturating_add(
                    self.fs
                        .stat(&self.directory.join(entry.name).to_string_lossy())?
                        .len,
                );
            }
        }
        *used = Some(bytes);
        Ok(())
    }
}

impl ResponseArchive for SessionResponseArchive {
    fn begin(&self, metadata: ResponseCaptureMetadata) -> io::Result<Box<dyn ResponseRecorder>> {
        let writer = self.capture_writer()?;
        self.initialize_budget()?;
        let write_guard = writer.write().map_err(io::Error::other)?;
        self.fs.create_dir_all(&self.directory.to_string_lossy())?;
        {
            let mut used = self
                .used
                .lock()
                .map_err(|_| io::Error::other("response budget poisoned"))?;
            let current = used.unwrap_or(0);
            // Reserve room for initial/final metadata and its atomic temporary.
            // Existing evidence is never deleted to admit a newer response.
            let reserved = current.saturating_add(16 * 1024);
            if reserved >= self.config.max_session_bytes {
                return Err(io::Error::other("session_response_budget_exhausted"));
            }
            *used = Some(reserved);
        }
        let stem = format!(
            "{}-{}",
            Session::sanitize_offload_id(&metadata.request_id),
            uuid::Uuid::new_v4()
        );
        let body_path = self.directory.join(format!("{stem}.sse.gz"));
        let metadata_path = self.directory.join(format!("{stem}.json"));
        self.fs.create_append_log(&body_path.to_string_lossy(), 0)?;
        drop(write_guard);
        let output = ArchiveWriter {
            fs: Arc::clone(&self.fs),
            writer: Arc::clone(&writer),
            path: body_path,
            used: Arc::clone(&self.used),
            limit: self.config.max_session_bytes,
        };
        let recording = SessionResponseRecorder {
            encoder: Some(GzEncoder::new(output, Compression::fast())),
            fs: Arc::clone(&self.fs),
            writer: Arc::clone(&writer),
            metadata_path,
            session_id: self.session_id.clone(),
            metadata,
            observed: 0,
            saved: 0,
            raw_limit: self.config.max_response_bytes,
            error: None,
            finished: false,
        };
        {
            let _guard = writer.write().map_err(io::Error::other)?;
            recording.write_metadata("recording", false)?;
        }
        Ok(Box::new(recording))
    }
}

struct ArchiveWriter {
    fs: Arc<dyn FsBackend>,
    writer: Arc<ResponseArchiveWriter>,
    path: PathBuf,
    used: Arc<Mutex<Option<u64>>>,
    limit: u64,
}

impl Write for ArchiveWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _guard = self.writer.write().map_err(io::Error::other)?;
        let mut used = self
            .used
            .lock()
            .map_err(|_| io::Error::other("response budget poisoned"))?;
        let current = used.unwrap_or(0);
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if current.saturating_add(length) > self.limit {
            return Err(io::Error::other("session_response_budget_exhausted"));
        }
        self.fs.append(&self.path.to_string_lossy(), bytes)?;
        *used = Some(current.saturating_add(length));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SessionResponseRecorder {
    encoder: Option<GzEncoder<ArchiveWriter>>,
    fs: Arc<dyn FsBackend>,
    writer: Arc<ResponseArchiveWriter>,
    metadata_path: PathBuf,
    session_id: String,
    metadata: ResponseCaptureMetadata,
    observed: u64,
    saved: u64,
    raw_limit: u64,
    error: Option<String>,
    finished: bool,
}

impl fmt::Debug for SessionResponseRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionResponseRecorder")
            .field("request_id", &self.metadata.request_id)
            .finish_non_exhaustive()
    }
}

impl SessionResponseRecorder {
    fn write_metadata(&self, reason: &str, gzip_complete: bool) -> io::Result<()> {
        let value = json!({
            "version": 1, "session_id": self.session_id, "response": self.metadata, "end_reason": reason,
            "observed_bytes": self.observed, "saved_bytes": self.saved,
            "saved_all_observed_bytes": self.observed == self.saved && self.error.is_none(),
            "gzip_complete": gzip_complete, "capture_error": self.error,
            "truncated": self.saved < self.observed,
            "max_response_bytes": self.raw_limit,
            "scope": "HTTP response body bytes consumed before provider parsing; excludes authorization headers",
        });
        self.fs.write_atomic(
            &self.metadata_path.to_string_lossy(),
            value.to_string().as_bytes(),
        )
    }
}

impl ResponseRecorder for SessionResponseRecorder {
    fn record(&mut self, bytes: &[u8]) {
        self.observed = self
            .observed
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let length = bytes
            .len()
            .min(usize::try_from(self.raw_limit.saturating_sub(self.saved)).unwrap_or(0));
        let Some(encoder) = self.encoder.as_mut() else {
            return;
        };
        if self.error.is_some() || length == 0 {
            return;
        }
        let result = encoder
            .write_all(&bytes[..length])
            .and_then(|()| encoder.flush());
        match result {
            Ok(()) => self.saved += u64::try_from(length).unwrap_or(0),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn finish(&mut self, end: ResponseCaptureEnd<'_>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.observed = end.observed_bytes;
        if let Some(error) = end.capture_error {
            self.error.get_or_insert_with(|| error.to_owned());
        }
        let complete = self
            .encoder
            .take()
            .is_some_and(|encoder| match encoder.finish() {
                Ok(_) => true,
                Err(error) => {
                    self.error = Some(error.to_string());
                    false
                }
            });
        // Serialize the final metadata with deletion, without failing inference.
        if let Ok(_guard) = self.writer.write() {
            let _ = self.write_metadata(end.reason, complete);
        }
    }
}

impl Drop for SessionResponseRecorder {
    fn drop(&mut self) {
        self.finish(ResponseCaptureEnd {
            reason: "body_reader_dropped",
            observed_bytes: self.observed,
            capture_error: None,
        });
    }
}
