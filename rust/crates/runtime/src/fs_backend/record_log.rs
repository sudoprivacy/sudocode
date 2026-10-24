//! Complete, durable JSONL records. Ordinary `append` has weaker semantics.
//!
//! Native files use a short OS lock on the actual log inode. Nexus adapters use
//! a dedicated WAL stream created with zero retention; an existing stream may
//! only be reopened from the session's trusted initialization record. A stream
//! type alone does not establish its durability or retention configuration.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use kernel::kernel::convenience::KernelConvenience;
use kernel::meta_store::DT_STREAM;
use kernel::Permission;

use super::{kernel_err, KernelFsAccess, KernelFsBackend, NexusVfsFsBackend};

/// A new log must have a fresh, unpublished name. Existing logs must have been
/// initialized through this interface, never through a memory-stream fallback.
/// The owning storage component keeps the log path append-only while clients
/// are admitted; replacing or removing its entry requires retiring those clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLogMode {
    CreateNew,
    Existing,
}

/// Atomic record transport shared by native and Nexus session persistence.
/// Cursors are opaque, monotonically increasing positions owned by the adapter.
/// A successful append is durable; an error may have committed. Retry the same
/// event identity and deduplicate in replay, rather than generating a new event.
pub trait RecordLog: Send + Sync {
    /// Commit one UTF-8 JSON object followed by LF; return the cursor after it.
    fn append(&self, record: &[u8]) -> io::Result<u64>;

    /// Read durable complete records from an adapter cursor without waiting.
    /// An empty result retains the cursor. After an append, a follower may lag;
    /// callers must observe their event before acknowledging its application.
    fn read(&self, cursor: u64) -> io::Result<(Vec<u8>, u64)>;
}

const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn validate(record: &[u8]) -> io::Result<()> {
    if record.len() > MAX_RECORD_BYTES {
        return Err(invalid("session record exceeds 64 MiB"));
    }
    let Some(body) = record.strip_suffix(b"\n") else {
        return Err(invalid("session record must end with LF"));
    };
    if body.contains(&b'\n') {
        return Err(invalid("session append must contain exactly one record"));
    }
    let value: serde_json::Value = serde_json::from_slice(body).map_err(io::Error::other)?;
    if !value.is_object() {
        return Err(invalid("session record must be a JSON object"));
    }
    Ok(())
}

pub(super) struct NativeRecordLog {
    file: Mutex<File>,
}

/// Closing a process releases the OS lock, including after an incomplete write.
struct LockedFile<'a>(MutexGuard<'a, File>);

impl Drop for LockedFile<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl NativeRecordLog {
    pub(super) fn open(path: &str, mode: RecordLogMode) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(mode == RecordLogMode::CreateNew)
            .open(path)?;
        if mode == RecordLogMode::CreateNew {
            file.sync_all()?;
            // Persist the new directory entry as well as its empty contents.
            #[cfg(unix)]
            {
                let parent = std::path::Path::new(path)
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                File::open(parent)?.sync_all()?;
            }
        }
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn lock(&self, exclusive: bool) -> io::Result<LockedFile<'_>> {
        let file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("session record file poisoned"))?;
        if exclusive {
            file.lock()?;
        } else {
            file.lock_shared()?;
        }
        Ok(LockedFile(file))
    }
}

/// Locate the last complete record without re-reading the full transcript.
fn committed_end(file: &mut File) -> io::Result<u64> {
    let mut end = file.metadata()?.len();
    let mut buffer = [0_u8; 8192];
    while end > 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let size = usize::try_from(end - start).map_err(io::Error::other)?;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..size])?;
        if let Some(index) = buffer[..size].iter().rposition(|byte| *byte == b'\n') {
            return Ok(start + index as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

impl RecordLog for NativeRecordLog {
    fn append(&self, record: &[u8]) -> io::Result<u64> {
        validate(record)?;
        let mut locked = self.lock(true)?;
        let file = &mut *locked.0;
        let end = committed_end(file)?;
        // An interrupted writer's unterminated suffix was never a committed
        // record. Remove it under the same lock before the next append.
        file.set_len(end)?;
        file.seek(SeekFrom::Start(end))?;
        file.write_all(record)?;
        file.sync_data()?;
        end.checked_add(record.len() as u64)
            .ok_or_else(|| invalid("session record cursor overflow"))
    }

    fn read(&self, cursor: u64) -> io::Result<(Vec<u8>, u64)> {
        let mut locked = self.lock(false)?;
        let file = &mut *locked.0;
        if cursor > file.metadata()?.len() {
            return Err(invalid(
                "session record log was truncated before its cursor",
            ));
        }
        file.seek(SeekFrom::Start(cursor))?;
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let Some(last) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            if bytes.len() > MAX_RECORD_BYTES {
                return Err(invalid("unterminated session record exceeds 64 MiB"));
            }
            return Ok((Vec::new(), cursor));
        };
        bytes.truncate(last + 1);
        // A previous writer can fail its flush after writing a complete LF.
        // Visibility alone then proves no durability. Sync every newly read
        // complete prefix under the lock before replay may authorize effects.
        file.sync_data()?;
        let next = cursor
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("session record cursor overflow"))?;
        Ok((bytes, next))
    }
}

type AppendFrame = dyn Fn(&[u8]) -> io::Result<u64> + Send + Sync;
type ReadFrame = dyn Fn(u64) -> io::Result<(Vec<u8>, u64)> + Send + Sync;

pub(super) struct WalRecordLog {
    pub append_frame: Box<AppendFrame>,
    pub read_frame: Box<ReadFrame>,
}

impl RecordLog for WalRecordLog {
    fn append(&self, record: &[u8]) -> io::Result<u64> {
        validate(record)?;
        (self.append_frame)(record)
    }

    fn read(&self, cursor: u64) -> io::Result<(Vec<u8>, u64)> {
        let (bytes, next) = (self.read_frame)(cursor)?;
        if bytes.is_empty() {
            if next != cursor {
                return Err(invalid("empty WAL read advanced the session cursor"));
            }
        } else {
            validate(&bytes)?;
            if next <= cursor {
                return Err(invalid("WAL session cursor did not advance"));
            }
        }
        Ok((bytes, next))
    }
}

fn existing_stream(exists: bool, is_stream: bool, mode: RecordLogMode) -> io::Result<()> {
    match (exists, is_stream, mode) {
        (true, _, RecordLogMode::CreateNew) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "new session record log path already exists",
        )),
        (false, _, RecordLogMode::Existing) => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "session record log is missing",
        )),
        (true, false, RecordLogMode::Existing) => {
            Err(invalid("session record log is not a WAL stream"))
        }
        _ => Ok(()),
    }
}

pub(super) fn open_kernel<K>(
    fs: &KernelFsBackend<K>,
    path: &str,
    mode: RecordLogMode,
) -> io::Result<Arc<dyn RecordLog>>
where
    K: KernelConvenience + KernelFsAccess + Send + Sync + 'static,
{
    fs.authorize_target(path, super::ResourceAccessKind::Read)?;
    fs.authorize_target(path, super::ResourceAccessKind::Write)?;
    let path = fs.to_kernel(path)?;
    fs.authorize(&path, Permission::Read)?;
    fs.authorize(&path, Permission::Write)?;
    let target = fs
        .kernel
        .inspect_io_target(&path, Permission::Write, &fs.ctx)
        .map_err(kernel_err)?;
    fs.authorize(&target.path, Permission::Read)?;
    let path = target.path;
    existing_stream(
        target.entry_type.is_some(),
        target.entry_type == Some(DT_STREAM),
        mode,
    )?;
    if mode == RecordLogMode::CreateNew {
        // The session initializer owns this fresh, random, unpublished path.
        // Setattr is not CAS. Never use it to upgrade or replace an old log.
        fs.kernel
            .sys_setattr(
                &path,
                &fs.ctx,
                i32::from(DT_STREAM),
                "",
                None,
                None,
                None,
                "wal",
                &fs.ctx.zone_id,
                false,
                0,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .map_err(kernel_err)?;
    }
    let write_kernel = Arc::clone(&fs.kernel);
    let write_ctx = fs.ctx.clone();
    let write_path = path.clone();
    let read_kernel = Arc::clone(&fs.kernel);
    let read_ctx = fs.ctx.clone();
    Ok(Arc::new(WalRecordLog {
        append_frame: Box::new(move |data| {
            let result = write_kernel
                .sys_write(&write_path, &write_ctx, data, 0)
                .map_err(kernel_err)?;
            if !result.hit {
                return Err(io::Error::other("session WAL append was not handled"));
            }
            // WAL writes return their committed frame sequence, not byte size.
            result
                .size
                .checked_add(1)
                .ok_or_else(|| invalid("session WAL cursor overflow"))
        }),
        read_frame: Box::new(move |cursor| {
            let result = read_kernel
                .sys_read(&path, &read_ctx, 0, cursor)
                .map_err(kernel_err)?;
            if result.entry_type != DT_STREAM {
                return Err(invalid("session WAL was replaced by a non-stream entry"));
            }
            let bytes = result.data.unwrap_or_default();
            if bytes.is_empty() {
                return Ok((bytes, cursor));
            }
            let next = result
                .stream_next_offset
                .ok_or_else(|| invalid("session WAL read has no stream cursor"))?;
            Ok((bytes, next as u64))
        }),
    }))
}

pub(super) fn open_remote(
    fs: &NexusVfsFsBackend,
    path: &str,
    mode: RecordLogMode,
) -> io::Result<Arc<dyn RecordLog>> {
    fs.authorize_target(path, super::ResourceAccessKind::Read)?;
    fs.authorize_target(path, super::ResourceAccessKind::Write)?;
    let read_target = fs.inspect_target(path, super::ResourceAccessKind::Read)?;
    let target = fs.inspect_target(path, super::ResourceAccessKind::Write)?;
    if target.path != read_target.path || target.entry_type != read_target.entry_type {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "session record log target changed during admission",
        ));
    }
    let path = target.path;
    existing_stream(
        target.entry_type.is_some(),
        target.entry_type == Some(i32::from(DT_STREAM)),
        mode,
    )?;
    if mode == RecordLogMode::CreateNew
        && !fs.client.ensure_stream(&path, "wal", 0, &fs.auth_token)?
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "session WAL initializer did not create a new stream",
        ));
    }
    let write_client = Arc::clone(&fs.client);
    let write_token = fs.auth_token.clone();
    let write_path = path.clone();
    let read_client = Arc::clone(&fs.client);
    let read_token = fs.auth_token.clone();
    let read_path = path;
    Ok(Arc::new(WalRecordLog {
        append_frame: Box::new(move |data| {
            write_client
                .stream_write(&write_path, data.to_vec(), &write_token)?
                .checked_add(1)
                .ok_or_else(|| invalid("session WAL cursor overflow"))
        }),
        read_frame: Box::new(move |cursor| {
            let result = read_client.stream_read_at(&read_path, cursor, false, 0, &read_token)?;
            Ok((result.data, result.next_offset))
        }),
    }))
}
