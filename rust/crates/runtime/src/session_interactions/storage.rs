//! Bounded sidecar snapshots under the existing session commit barrier.
//! Native storage compares its last snapshot while that OS lock is held.
//! Remote storage uses conditional writes; unsupported backends fail closed.

use sha2::{Digest, Sha256};

use crate::fs_backend::{FsBackend, VersionToken, WriteOutcome};
use crate::session_writer::SessionWriteGuard;

#[derive(PartialEq, Eq)]
pub(super) enum Version {
    Native(Option<[u8; 32]>),
    Conditional(Option<VersionToken>),
}

fn bounded(bytes: Option<Vec<u8>>) -> Result<Option<Vec<u8>>, String> {
    if bytes
        .as_ref()
        .is_some_and(|bytes| bytes.len() > super::MAX_BYTES)
    {
        return Err("interaction journal exceeds its storage limit".into());
    }
    Ok(bytes)
}

fn native_read(fs: &dyn FsBackend, path: &str) -> Result<Option<Vec<u8>>, String> {
    match fs.stat(path) {
        Ok(metadata) if metadata.len > super::MAX_BYTES as u64 => {
            return Err("interaction journal exceeds its storage limit".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    }
    bounded(Some(fs.read(path).map_err(|error| error.to_string())?))
}

pub(super) fn read(
    fs: &dyn FsBackend,
    path: &str,
    _write: &SessionWriteGuard<'_>,
) -> Result<(Option<Vec<u8>>, Version), String> {
    if fs.fenced_locks() {
        let bytes = native_read(fs, path)?;
        let digest = bytes.as_ref().map(|bytes| Sha256::digest(bytes).into());
        return Ok((bytes, Version::Native(digest)));
    }
    let (bytes, version) = fs
        .read_versioned(path)
        .map_err(|error| error.to_string())?
        .map_or((None, None), |(bytes, version)| {
            (Some(bytes), Some(version))
        });
    Ok((bounded(bytes)?, Version::Conditional(version)))
}

pub(super) fn write(
    fs: &dyn FsBackend,
    path: &str,
    expected: &Version,
    bytes: &[u8],
    _write: &SessionWriteGuard<'_>,
) -> Result<Version, String> {
    match expected {
        Version::Native(previous) => {
            // All native session writers hold the same OS commit lock here.
            // A separately loaded context must not replace a newer decision.
            let current =
                native_read(fs, path)?.map(|bytes| <[u8; 32]>::from(Sha256::digest(bytes)));
            if &current != previous {
                return Err("interaction storage changed; reopen the session".into());
            }
            fs.write_atomic(path, bytes)
                .map_err(|error| error.to_string())?;
            Ok(Version::Native(Some(Sha256::digest(bytes).into())))
        }
        Version::Conditional(version) => {
            match fs
                .write_if_version(path, version.as_ref(), bytes)
                .map_err(|error| error.to_string())?
            {
                WriteOutcome::Committed(version) => Ok(Version::Conditional(Some(version))),
                WriteOutcome::Conflict { .. } => {
                    Err("interaction storage changed; reopen the session".into())
                }
                WriteOutcome::Unknown => {
                    Err("interaction commit outcome is unknown; reopen the session".into())
                }
            }
        }
    }
}
