//! Integration tests proving the co-hosted file tools hit the VFS
//! in-process through `KernelFsBackend`, never the host filesystem.
//!
//! These exercise `read_file` / `write_file` / `edit_file` / `glob_search`
//! / `grep_search` against a real in-memory `Kernel` (not a mock) so the
//! backend-aware path normalisation and the `readdir`-composed traversal
//! are validated end to end. The paths used (`/ws/…`) do not exist on the
//! host, so a host-`std::fs` regression would surface as a NotFound
//! rather than silently passing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kernel::abc::object_store::{ObjectStore, StorageError, WriteResult};
use kernel::kernel::{Kernel, OperationContext};
use runtime::{
    edit_file, glob_search, grep_search, read_file, write_file, GrepSearchInput, KernelFsBackend,
};

/// Minimal in-memory content backend so a fresh `Kernel` can round-trip
/// regular-file bytes (dirents fall through to the global metastore; only
/// content needs a store). Mirrors the kernel's own `TestObjectStore`.
#[derive(Default)]
struct MemStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

impl ObjectStore for MemStore {
    fn name(&self) -> &str {
        "mem"
    }

    fn write_content(
        &self,
        content: &[u8],
        content_id: &str,
        _ctx: &OperationContext,
        offset: u64,
    ) -> Result<WriteResult, StorageError> {
        let mut b = self.blobs.lock().unwrap();
        let mut data = if offset > 0 {
            b.get(content_id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let start = offset as usize;
        if start > data.len() {
            data.resize(start, 0);
        }
        let end = start + content.len();
        if end > data.len() {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(content);
        let size = data.len() as u64;
        b.insert(content_id.to_string(), data);
        Ok(WriteResult {
            content_id: content_id.to_string(),
            version: content_id.to_string(),
            size,
        })
    }

    fn read_content(
        &self,
        content_id: &str,
        _ctx: &OperationContext,
    ) -> Result<Vec<u8>, StorageError> {
        self.blobs
            .lock()
            .unwrap()
            .get(content_id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(content_id.into()))
    }

    fn get_content_size(&self, content_id: &str) -> Result<u64, StorageError> {
        self.blobs
            .lock()
            .unwrap()
            .get(content_id)
            .map(|d| d.len() as u64)
            .ok_or_else(|| StorageError::NotFound(content_id.into()))
    }
}

/// A fresh kernel with a content-capable root mount.
fn kernel_with_root_backend() -> Arc<Kernel> {
    let kernel = Arc::new(Kernel::new());
    let backend: Arc<dyn ObjectStore> = Arc::new(MemStore::default());
    kernel
        .vfs_router_arc()
        .add_mount("/", "root", Some(backend), false);
    kernel
}

/// A `KernelFsBackend` rooted at `/ws`, over the given kernel.
fn vfs_backend(kernel: &Arc<Kernel>) -> KernelFsBackend<Kernel> {
    KernelFsBackend::for_agent(Arc::clone(kernel), "test-owner", "root", "agent-x", "/ws")
}

fn grep_input(pattern: &str, path: &str) -> GrepSearchInput {
    GrepSearchInput {
        pattern: pattern.to_string(),
        path: Some(path.to_string()),
        glob: None,
        output_mode: Some(String::from("files_with_matches")),
        before: None,
        after: None,
        context_short: None,
        context: None,
        line_numbers: Some(true),
        case_insensitive: Some(false),
        file_type: None,
        head_limit: Some(50),
        offset: Some(0),
        multiline: Some(false),
    }
}

#[test]
fn write_then_read_round_trips_through_the_vfs() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);

    let path = "/ws/notes.txt";
    let write = write_file(&fs, path, "vfs-only content").expect("VFS write should succeed");
    assert_eq!(write.kind, "create");

    let read = read_file(&fs, path, None, None).expect("VFS read should succeed");
    assert_eq!(read.file.content, "vfs-only content");

    // The VFS path is not a host path — a std::fs regression would have
    // created it on disk.
    assert!(
        !std::path::Path::new(path).exists(),
        "co-hosted write must not touch the host filesystem"
    );
}

#[test]
fn edit_file_mutates_vfs_content() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);

    let path = "/ws/code.rs";
    write_file(&fs, path, "let x = alpha;\n").expect("seed write");
    let edited = edit_file(&fs, path, "alpha", "omega", false).expect("VFS edit should succeed");
    assert!(edited.new_string.contains("omega"));

    let read = read_file(&fs, path, None, None).expect("read after edit");
    assert_eq!(read.file.content, "let x = omega;");
}

#[test]
fn relative_paths_resolve_against_the_agent_workspace() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);

    // A relative tool path must land under the agent's workspace root
    // (`/ws`), not the host cwd.
    write_file(&fs, "todo.md", "- ship it").expect("relative write");
    let read = read_file(&fs, "/ws/todo.md", None, None).expect("absolute read back");
    assert_eq!(read.file.content, "- ship it");
}

#[test]
fn glob_and_grep_walk_the_vfs_trie() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    let root = "/ws";

    write_file(&fs, &format!("{root}/a.rs"), "fn a() { needle(); }").unwrap();
    write_file(&fs, &format!("{root}/b.rs"), "fn b() {}").unwrap();
    write_file(&fs, &format!("{root}/notes.txt"), "no code here").unwrap();
    write_file(&fs, &format!("{root}/sub/c.rs"), "fn c() { needle(); }").unwrap();

    // glob: recursive **/*.rs must find all three .rs files (including the
    // nested one) and exclude the .txt — proving the readdir-composed
    // recursive walk descends the VFS trie.
    let globbed = glob_search(&fs, "**/*.rs", Some(root)).expect("VFS glob should succeed");
    assert_eq!(
        globbed.num_files, 3,
        "glob should find a.rs, b.rs, sub/c.rs"
    );

    // grep: content search over the same subtree finds the two files that
    // contain the needle.
    let grepped = grep_search(&fs, &grep_input("needle", root)).expect("VFS grep should succeed");
    assert_eq!(grepped.num_files, 2, "grep should match a.rs and sub/c.rs");
}
