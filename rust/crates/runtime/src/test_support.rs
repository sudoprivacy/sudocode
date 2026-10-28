//! Fixtures shared by integration-test binaries.
//!
//! In the crate rather than under `tests/` because an integration test is its
//! own binary: `tests/common/mod.rs` is reachable only from the same crate's
//! tests, and these fixtures are needed from another crate's tests too. Nothing
//! in the product calls this module.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use kernel::abc::object_store::{ObjectStore, StorageError, WriteResult};
use kernel::kernel::OperationContext;

/// Return a fixture path unique across parallel calls and processes.
///
/// Clock resolution alone cannot distinguish concurrent tests. The caller
/// creates and removes the directory.
#[must_use]
pub fn temp_dir_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// An in-memory [`ObjectStore`] so `DT_REG` **content** round-trips under test.
///
/// A mount with no backend carries metadata only: a `DT_REG` write "succeeds"
/// and the read comes back not-found. Anything that keeps bytes in a regular
/// entry needs a real content backend behind its mount — a reader register
/// most of all, since without one every read position is zero and every
/// restart replays. Production has one (host-fs at `/`); a test has to say so.
#[derive(Default)]
pub struct MemObjectStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl ObjectStore for MemObjectStore {
    fn name(&self) -> &'static str {
        "memtest"
    }

    fn write_content(
        &self,
        content: &[u8],
        content_id: &str,
        _ctx: &OperationContext,
        offset: u64,
    ) -> Result<WriteResult, StorageError> {
        let mut blobs = self
            .blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = blobs.entry(content_id.to_string()).or_default();
        if offset == 0 {
            entry.clear();
        }
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        let end = off.saturating_add(content.len());
        if entry.len() < end {
            entry.resize(end, 0);
        }
        entry[off..end].copy_from_slice(content);
        Ok(WriteResult {
            content_id: content_id.to_string(),
            version: String::new(),
            size: entry.len() as u64,
        })
    }

    fn read_content(
        &self,
        content_id: &str,
        _ctx: &OperationContext,
    ) -> Result<Vec<u8>, StorageError> {
        self.blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(content_id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(content_id.to_string()))
    }

    /// Path-addressed, like the production host-fs backend: a rename moves the
    /// bytes from one key to the other, replacing whatever the destination held.
    fn rename(&self, old_path: &str, new_path: &str) -> Result<(), StorageError> {
        let mut blobs = self
            .blobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let data = blobs
            .remove(old_path)
            .ok_or_else(|| StorageError::NotFound(old_path.to_string()))?;
        blobs.insert(new_path.to_string(), data);
        Ok(())
    }
}
