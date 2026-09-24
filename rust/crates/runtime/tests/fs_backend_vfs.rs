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
use kernel::meta_store::DT_LINK;
use runtime::{
    edit_file, glob_search, grep_search, read_file, write_file, FsBackend, GrepSearchInput,
    KernelFsBackend, Session, SessionStore,
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
fn oversized_tool_output_offloads_onto_the_vfs() {
    // Proof that the offload path is fully backend-agnostic and needs NOTHING
    // new from nexus: point a session's persistence + FsBackend at the VFS, run
    // the real `offload_tool_result`, and read the blob back through the same
    // kernel. The write lands as a regular file (a DT_FILE) on the VFS via the
    // exact `create_dir_all` + `write_atomic` (→ sys_setattr/sys_stat/sys_write)
    // the local backend uses; read-more's `fs.read` (→ sys_read) round-trips it.
    // The VFS root (where sessions/offload live) is chosen here on the
    // sudocode side — nexus only stores what it is told.
    let kernel = kernel_with_root_backend();
    let fs: Arc<dyn FsBackend> = Arc::new(vfs_backend(&kernel));

    let session = Session::new()
        .with_persistence_path("/ws/sessions/sid-1.jsonl")
        .with_fs_backend(Arc::clone(&fs));

    let id = "toolu_vfs_1";
    let body = "L".repeat(40_000); // > the 30 000-byte bash offload threshold
    let (path, size) = session
        .offload_tool_result(id, body.as_bytes())
        .expect("offload should write to the VFS");

    assert_eq!(size, body.len() as u64);
    // The blob lives on the VFS, never on the host filesystem.
    assert!(
        !std::path::Path::new(&path).exists(),
        "offload must not touch the host filesystem"
    );
    // Byte-exact read-back through the kernel backend (this is exactly what
    // read_tool_output does under the hood: fs.read → sys_read).
    let read_back = fs.read(&path).expect("VFS read of offloaded blob");
    assert_eq!(read_back, body.as_bytes());
}

#[test]
fn create_append_log_without_federation_yields_a_durable_regular_file() {
    // A durable "wal" DT_STREAM needs federation (NEXUS_PEERS), absent on a
    // bare test kernel. `create_append_log` must then degrade to a DT_REG —
    // durable on the local metastore — and NEVER a bounded, node-local
    // "memory" stream that would silently lose the transcript on restart.
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    let path = "/ws/sessions/sid-2.jsonl";

    fs.create_append_log(path, 0)
        .expect("create_append_log should succeed");
    assert!(
        !fs.is_append_stream(path).unwrap(),
        "no federation → transcript degrades to a regular file, not a stream"
    );

    // The regular-file append path (read-concat-write) round-trips unchanged.
    fs.append(path, b"line-1\n").unwrap();
    fs.append(path, b"line-2\n").unwrap();
    assert_eq!(fs.read(path).unwrap(), b"line-1\nline-2\n");
}

#[test]
fn dt_stream_append_frames_read_back_deframed() {
    // A DT_STREAM created directly on the kernel (node-local, federation-free)
    // stands in for the durable wal stream: the framing contract that
    // `KernelFsBackend` append/read relies on is identical. Each append is one
    // framed record; read walks every frame to the tail and concatenates the
    // deframed payloads back into the original append byte stream.
    let kernel = kernel_with_root_backend();
    let path = "/ws/transcript-stream.jsonl";
    kernel
        .create_stream(path, 64 * 1024)
        .expect("create DT_STREAM");

    let fs = vfs_backend(&kernel);
    assert!(
        fs.is_append_stream(path).unwrap(),
        "an entry created as a DT_STREAM reports as an append-log"
    );

    fs.append(path, b"{\"a\":1}\n").unwrap();
    fs.append(path, b"{\"b\":2}\n").unwrap();
    assert_eq!(
        fs.read(path).unwrap(),
        b"{\"a\":1}\n{\"b\":2}\n",
        "reading a DT_STREAM reproduces the appended records in order"
    );
}

#[test]
fn kernel_backend_imposes_flat_sessions_root_and_can_link() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);

    // nexus imposes the flat, session-id-keyed /sessions/ namespace.
    assert_eq!(
        fs.managed_root(runtime::ManagedRoot::Sessions).as_deref(),
        Some("/sessions")
    );

    // link() creates a DT_LINK pointer (the /agents/{name}/sessions/<sid> index).
    fs.link("/agents/alice/sessions/sid-1", "/sessions/sid-1")
        .expect("link should create a DT_LINK");
    let st = kernel
        .sys_stat("/agents/alice/sessions/sid-1", "root")
        .expect("linked path should stat");
    assert_eq!(st.entry_type, DT_LINK, "alias is a DT_LINK");
    assert_eq!(st.link_target.as_deref(), Some("/sessions/sid-1"));
    // read_link follows the DT_LINK back to its target — the follow half of
    // link(), matching StdFsBackend::read_link so callers are backend-agnostic.
    assert_eq!(
        fs.read_link("/agents/alice/sessions/sid-1")
            .expect("read_link should resolve the DT_LINK"),
        "/sessions/sid-1"
    );
}

#[test]
fn session_store_roots_at_vfs_sessions_over_kernel_backend() {
    // The anti-forget property: a SessionStore over KernelFsBackend roots
    // sessions at /sessions/ automatically — no code change, just the backend.
    let kernel = kernel_with_root_backend();
    let fs: Arc<dyn FsBackend> = Arc::new(vfs_backend(&kernel));
    let store = SessionStore::from_cwd_with("/ws", Arc::clone(&fs)).expect("store");

    let norm = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    assert_eq!(norm(store.sessions_dir()), "/sessions"); // no .scode, no workspace_hash
    let handle = store.create_handle("sid-42");
    assert_eq!(norm(&handle.path), "/sessions/sid-42/transcript.jsonl");
}

#[test]
fn create_handle_plants_agent_dt_link_when_agent_name_is_set() {
    // The anti-forget property, end to end: a nexus-backed store bound to an
    // agent name plants the /agents/{name}/sessions/<sid> DT_LINK enum-index
    // automatically on session create — so swapping std → nexus + supplying
    // the agent name is all it takes; no separate link wiring to remember.
    let kernel = kernel_with_root_backend();
    let fs: Arc<dyn FsBackend> = Arc::new(vfs_backend(&kernel));
    let store = SessionStore::from_cwd_with("/ws", Arc::clone(&fs))
        .expect("store")
        .with_agent_name("alice");

    let _ = store.create_handle("sid-9");

    let st = kernel
        .sys_stat("/agents/alice/sessions/sid-9", "root")
        .expect("agent enum-index DT_LINK should exist after create");
    assert_eq!(st.entry_type, DT_LINK);
    assert_eq!(st.link_target.as_deref(), Some("/sessions/sid-9"));
}

#[test]
fn create_handle_without_agent_name_plants_no_link() {
    // Standalone (no agent name) creates no DT_LINK — the /agents/ index is a
    // co-host concern, not a standalone one.
    let kernel = kernel_with_root_backend();
    let fs: Arc<dyn FsBackend> = Arc::new(vfs_backend(&kernel));
    let store = SessionStore::from_cwd_with("/ws", Arc::clone(&fs)).expect("store");

    let _ = store.create_handle("sid-10");

    assert!(
        kernel
            .sys_stat("/agents/alice/sessions/sid-10", "root")
            .is_none(),
        "no agent name → no enum-index link"
    );
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
/// Memory is READ through the backend, so a co-hosted agent recalls what is in
/// its own subtree of the VFS.
///
/// The directory already came from `managed_root`; the reading did not. The
/// provider handed `/agents/agent-x/memory` to `std::fs`, which names nothing on
/// any host — so memory was silently empty for every co-hosted agent, with the
/// files sitting in the VFS where nobody looked. The host path is asserted
/// absent for the same reason this file uses `/ws`: a regression back to the
/// host filesystem fails here instead of passing by luck.
#[test]
fn memory_is_read_from_the_agents_own_vfs_subtree() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);

    let dir = fs
        .managed_root(runtime::ManagedRoot::Memory)
        .expect("a co-hosted agent roots memory under itself");
    assert_eq!(dir, "/agents/agent-x/memory");
    fs.create_dir_all(&dir).expect("create the memory dir");
    fs.write(
        &format!("{dir}/MEMORY.md"),
        b"- [One fact](one.md) - the hook
",
    )
    .expect("write the index");
    fs.write(
        &format!("{dir}/one.md"),
        b"---
name: one
description: the one fact
metadata:
  type: project
---

the body
",
    )
    .expect("write the entry");

    let index = runtime::memory::MemoryIndex::load(std::path::Path::new(&dir), &fs)
        .expect("load memory through the backend");
    assert_eq!(
        index.entries().len(),
        1,
        "the entry written into the VFS is the entry recalled"
    );
    assert_eq!(index.entries()[0].name, "one");
    assert!(
        index.index().is_some(),
        "MEMORY.md is read through the backend too, not just the entries"
    );
    assert!(
        !std::path::Path::new(&dir).exists(),
        "and none of it was written to the host filesystem"
    );
}

/// Two co-hosted agents on one daemon keep their own todo list.
///
/// Both halves mattered: the path was derived from the daemon's directory (one
/// file for every agent), and the store itself was a process-wide `OnceLock`
/// (one list object for every agent, whoever resolved a path first).
#[test]
fn two_cohosted_agents_do_not_share_one_todo_list() {
    use runtime::{Todo, TodoStatus, TodoStore};

    let kernel = kernel_with_root_backend();
    let agent = |name: &str| -> Arc<dyn FsBackend> {
        Arc::new(KernelFsBackend::for_agent(
            Arc::clone(&kernel),
            "test-owner",
            "root",
            name,
            "/ws",
        ))
    };
    let (alice, bob) = (agent("alice"), agent("bob"));

    let alice_path = runtime::todo_store_path(&alice).expect("alice's store path");
    let bob_path = runtime::todo_store_path(&bob).expect("bob's store path");
    // `Path::join` writes a host separator; every backend entry point collapses
    // it back to the VFS spelling, so the comparison is against the VFS form.
    let norm = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    assert_eq!(norm(&alice_path), "/agents/alice/.sudocode-todos.json");
    assert_ne!(
        alice_path, bob_path,
        "each agent's list is keyed by the agent, not by a directory they share"
    );

    TodoStore::load(&alice_path, Arc::clone(&alice)).set(vec![Todo {
        content: String::from("Run the tests"),
        status: TodoStatus::InProgress,
        active_form: String::from("Running the tests"),
    }]);

    assert_eq!(
        TodoStore::load(&alice_path, Arc::clone(&alice))
            .list()
            .len(),
        1,
        "a fresh handle over the same path is the same store — the write persisted"
    );
    assert!(
        TodoStore::load(&bob_path, Arc::clone(&bob))
            .list()
            .is_empty(),
        "and bob's list is untouched by alice's write"
    );
}
