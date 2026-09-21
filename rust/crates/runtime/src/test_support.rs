//! Fixtures shared by integration-test binaries.
//!
//! In the crate rather than under `tests/` because an integration test is its
//! own binary: `tests/common/mod.rs` is reachable only from the same crate's
//! tests, and these fixtures are needed from another crate's tests too. Nothing
//! in the product calls this module.

use std::collections::HashMap;
use std::sync::Mutex;

use kernel::abc::object_store::{ObjectStore, StorageError, WriteResult};
use kernel::kernel::OperationContext;

/// An in-memory [`ObjectStore`] so DT_REG **content** round-trips under test.
///
/// A mount with no backend carries metadata only: a DT_REG write "succeeds"
/// and the read comes back not-found. Anything that keeps bytes in a regular
/// entry needs a real content backend behind its mount — a reader register
/// most of all, since without one every read position is zero and every
/// restart replays. Production has one (host-fs at `/`); a test has to say so.
#[derive(Default)]
pub struct MemObjectStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl ObjectStore for MemObjectStore {
    fn name(&self) -> &str {
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
}
