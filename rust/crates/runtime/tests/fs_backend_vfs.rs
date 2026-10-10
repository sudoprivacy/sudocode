//! Integration tests for file tools over the in-process kernel and TCP VFS host.
//!
//! These exercise `read_file` / `write_file` / `edit_file` / `glob_search`
//! / `grep_search` against a real in-memory `Kernel` (not a mock) so the
//! backend-aware path normalisation, authorization, and `readdir` traversal
//! are validated end to end. The paths used (`/ws/…`) do not exist on the
//! host, so a host-`std::fs` regression would surface as a NotFound
//! rather than silently passing.
//!
//! One test here is deliberately NOT about the VFS: `link` is a contract both
//! backends answer, and the kernel half used to claim parity with the host half
//! in a comment while nothing exercised it. The two live next to each other so
//! the claim is checked rather than asserted.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kernel::abc::object_store::{ObjectStore, StorageError, WriteResult};
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::{Kernel, KernelError, OperationContext};
use kernel::meta_store::DT_LINK;
use kernel::{Permission, PermissionProvider};
use runtime::zone_context::{
    ContextSource, HostZoneContext, ResourceAccessKind, RuntimeResourceAuthorizer,
    ENV_NEXUS_DELEGATION_REF, ENV_NEXUS_RESOURCE_SCOPE,
};
use runtime::{
    edit_file, glob_search, grep_search, read_file, write_file, FsBackend, GrepSearchInput,
    KernelFsBackend, NexusVfsFsBackend, Session, SessionStore,
};
use sudo_contracts::ResourceRef;

/// Minimal in-memory content backend so a fresh `Kernel` can round-trip
/// regular-file bytes (dirents fall through to the global metastore; only
/// content needs a store). Mirrors the kernel's own `TestObjectStore`.
#[derive(Default)]
struct MemStore {
    blobs: Mutex<HashMap<String, Vec<u8>>>,
}

struct CountingAllow(Arc<AtomicUsize>);

impl PermissionProvider for CountingAllow {
    fn check(
        &self,
        _path: &str,
        _route: Option<&kernel::core::vfs_router::RouteResult>,
        _permission: Permission,
        _ctx: &OperationContext,
    ) -> Result<(), KernelError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
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
    kernel_with_backend("root")
}

fn kernel_with_backend(zone_id: &str) -> Arc<Kernel> {
    let kernel = Arc::new(Kernel::new());
    let backend: Arc<dyn ObjectStore> = Arc::new(MemStore::default());
    kernel
        .vfs_router_arc()
        .add_mount("/", zone_id, Some(backend), false);
    kernel
}

/// A `KernelFsBackend` rooted at `/ws`, over the given kernel. The context
/// names agent-x: the agents-base subtrees (`managed_root`) are keyed by the
/// context's agent identity, and a context without one roots nothing.
fn vfs_backend(kernel: &Arc<Kernel>) -> KernelFsBackend<Kernel> {
    KernelFsBackend::new(
        Arc::clone(kernel),
        OperationContext::new("system", "root", true, Some("agent-x"), true),
        "/ws",
    )
}

#[test]
fn p1a_resource_targets_require_the_same_delegated_zone_for_cohost_and_subprocess() {
    let kernel = kernel_with_backend("tenant-zone");
    let permission_checks = Arc::new(AtomicUsize::new(0));
    let provider: Arc<Box<dyn PermissionProvider>> =
        Arc::new(Box::new(CountingAllow(Arc::clone(&permission_checks))));
    kernel.set_permission_provider(provider);

    let without_delegation = KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "tenant-zone",
        "agent-x",
        "/ws",
    );
    let denied = without_delegation.write("/ws/denied.txt", b"denied");
    assert_eq!(
        denied
            .expect_err("a non-root runtime without delegation must fail closed")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );

    let mut descriptor = AgentDescriptor {
        pid: "pid-zone-p1a".to_string(),
        name: "agent-zone-p1a".to_string(),
        owner_id: "test-owner".to_string(),
        zone_id: "tenant-zone".to_string(),
        ..AgentDescriptor::default()
    };
    descriptor.labels.insert(
        ENV_NEXUS_DELEGATION_REF.to_string(),
        "dlg-short-lived".to_string(),
    );
    let scope = serde_json::json!({
        "schema_version": 1,
        "zone_id": "tenant-zone",
        "rules": [
            {"capability": "zone.data.read", "resource_prefixes": ["/ws"]},
            {"capability": "zone.data.write", "resource_prefixes": ["/ws"]}
        ]
    })
    .to_string();
    descriptor
        .labels
        .insert(ENV_NEXUS_RESOURCE_SCOPE.to_string(), scope.clone());
    let cohost = HostZoneContext::from_planted_descriptor(&descriptor);
    let subprocess = HostZoneContext::from_parts_with_scope(
        "tenant-zone",
        Some("https://nexus.example/v2".to_string()),
        Some("dlg-short-lived".to_string()),
        Some(&scope),
        ContextSource::UnverifiedDelegationRef,
    );
    assert_eq!(cohost.delegation_ref(), Some("dlg-short-lived"));
    assert_eq!(
        subprocess.nexus_v2_base_url(),
        Some("https://nexus.example/v2")
    );
    assert_eq!(subprocess.delegation_ref(), cohost.delegation_ref());
    let own_ref = ResourceRef {
        api_version: "common.sudo.dev/v1".to_string(),
        kind: "ResourceRef".to_string(),
        zone_id: "tenant-zone".to_string(),
        path: "/ws/allowed.txt".to_string(),
        version: None,
        digest: None,
        media_type: None,
        size_bytes: None,
    };
    // The cohost context is trusted-local — a descriptor planted by the
    // trusted ManagedAgentService is authority by construction — so a
    // ResourceRef inside its zone and scope is AUTHORIZED, enforced to the
    // descriptor's scope rules. The subprocess context is host-injected env
    // without a server-verifiable credential and stays fail-closed.
    assert!(cohost.authorize_resource_ref(&own_ref).is_ok());
    assert!(matches!(
        subprocess.authorize_resource_ref(&own_ref),
        Err(runtime::zone_context::ZoneAuthError::DelegationInvalid)
    ));

    let backend = KernelFsBackend::for_agent_descriptor(Arc::clone(&kernel), &descriptor, "/ws");
    backend
        .write("/ws/allowed.txt", b"zone-scoped")
        .expect("a planted descriptor with a /ws write rule authorizes the write");
    assert!(
        permission_checks.load(Ordering::Relaxed) > 0,
        "an authorized write reaches the kernel's permission provider"
    );

    let foreign_ref = ResourceRef {
        zone_id: "other-zone".to_string(),
        ..own_ref
    };
    assert!(cohost.authorize_resource_ref(&foreign_ref).is_err());
    assert!(subprocess.authorize_resource_ref(&foreign_ref).is_err());

    let malformed_host = HostZoneContext::from_trusted_parts(
        "INVALID ZONE",
        Some("https://nexus.example/v2".to_string()),
        Some("dlg-short-lived".to_string()),
        ContextSource::UnverifiedDelegationRef,
    );
    assert!(malformed_host.authorize_path("/ws/denied.txt").is_err());
}

#[test]
fn trusted_local_scope_enforces_capability_and_canonical_prefixes() {
    let scope = serde_json::json!({
        "schema_version": 1,
        "zone_id": "tenant-zone",
        "rules": [
            {"capability": "zone.data.read", "resource_prefixes": ["/proc/pid/workspace"]}
        ]
    })
    .to_string();
    let context = HostZoneContext::from_parts_with_scope(
        "tenant-zone",
        None,
        None,
        Some(&scope),
        ContextSource::TrustedLocal,
    );
    let resource = ResourceRef {
        api_version: "common.sudo.dev/v1".to_string(),
        kind: "ResourceRef".to_string(),
        zone_id: "tenant-zone".to_string(),
        path: "/proc/pid/workspace/file.txt".to_string(),
        version: None,
        digest: None,
        media_type: None,
        size_bytes: None,
    };
    let authorizer = RuntimeResourceAuthorizer::new(&context);
    assert!(authorizer
        .authorize(ResourceAccessKind::Read, "zone.data.read", &resource)
        .is_ok());
    assert!(matches!(
        authorizer.authorize(ResourceAccessKind::Write, "zone.data.write", &resource),
        Err(runtime::zone_context::ZoneAuthError::OutOfScope(_))
    ));
    let trailing = ResourceRef {
        path: "/proc/pid/workspace/".to_string(),
        ..resource
    };
    assert!(matches!(
        authorizer.authorize(ResourceAccessKind::Read, "zone.data.read", &trailing),
        Err(runtime::zone_context::ZoneAuthError::InvalidPath(_))
    ));
}

#[test]
fn every_remote_backend_constructor_fails_before_rpc() {
    fn denied<T>(result: std::io::Result<T>) {
        match result {
            Ok(_) => panic!("unverified remote access must fail closed"),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied),
        }
    }

    let client = nexus_vfs_client::NexusVfsClient::connect("http://127.0.0.1:9")
        .expect("lazy client construction");
    let backend = NexusVfsFsBackend::new(client, "arbitrary-api-key".to_string());
    denied(backend.read("/agents/a/inbox"));
    denied(backend.write("/agents/a/inbox", b"x"));
    denied(backend.append("/agents/a/inbox", b"x"));
    denied(backend.stat("/agents/a/inbox"));
    denied(backend.readdir("/agents"));
    denied(backend.exists("/agents/a/inbox"));
    denied(backend.rename("/agents/a/inbox", "/agents/b/inbox"));
    denied(backend.link("/agents/a/alias", "/agents/a/target"));

    let shared = Arc::new(
        nexus_vfs_client::NexusVfsClient::connect("http://127.0.0.1:9")
            .expect("lazy shared client construction"),
    );
    let from_arc = NexusVfsFsBackend::from_arc(Arc::clone(&shared), String::new());
    denied(from_arc.read("/agents/a/inbox"));

    let mailbox = runtime::mailbox::Mailbox::over_nexus(shared, "sender", "any-token");
    let delivery = mailbox.send(runtime::agent_mailbox::MailboxEnvelope {
        from: "sender".to_string(),
        to: "receiver".to_string(),
        body: "must not reach RPC".to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: "message".to_string(),
        request_id: None,
    });
    assert!(
        delivery.is_err(),
        "mailbox production construction must fail closed"
    );
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

/// The host backend answers `link` with a REAL OS link, needing no privilege.
///
/// The other half of the parity the test above claims. It is asserted here
/// because the kernel half cannot show it: `KernelFsBackend` plants a DT_LINK,
/// which says nothing about what `StdFsBackend` does on a host filesystem, and
/// the co-host and a plain `scode` have to behave the same way — the chat-list
/// index is a pointer at a conversation root on both.
///
/// A DIRECTORY target is the case that matters, and is the reason this passes
/// on Windows without elevation. A native symlink there needs
/// `SeCreateSymbolicLinkPrivilege` (admin, or Developer Mode); a junction does
/// not, works for directories, and is a reparse point — so `symlink_metadata`
/// reports `is_symlink` for it exactly as it does for a Unix symlink. One
/// assertion therefore covers both platforms, and the daemon never needs a UAC
/// prompt to index a conversation.
///
/// `is_symlink` is the binding assertion: with the platform call removed and
/// only the pointer-file fallback left, `read_link` still round-trips, so every
/// other assertion here passes against an implementation that creates no link
/// at all.
#[test]
fn host_backend_links_a_directory_without_a_privilege() {
    use runtime::StdFsBackend;

    let root = tmp_dir("host-link");
    let target = root.join("conv-root");
    std::fs::create_dir_all(&target).expect("target dir");
    let alias = root.join("idx");
    let (alias, target) = (
        alias.to_string_lossy().into_owned(),
        target.to_string_lossy().into_owned(),
    );

    let fs = StdFsBackend;
    fs.link(&alias, &target).expect("link a directory");

    assert!(
        fs.symlink_metadata(&alias)
            .expect("lstat the alias")
            .is_symlink,
        "a directory target must get a real OS link — a symlink on Unix, a \
         junction on Windows, neither of which needs a privilege"
    );
    assert_eq!(
        fs.read_link(&alias).expect("read_link the alias"),
        target,
        "and it must resolve back to what it points at"
    );
    // The NAME is the index contract: it has to be listable whichever shape the
    // platform produced, because that listing IS the chat list.
    assert!(
        fs.readdir(&root.to_string_lossy())
            .expect("list the directory the index lives in")
            .iter()
            .any(|e| e.name == "idx"),
        "the index must appear in a listing by name"
    );

    std::fs::remove_dir_all(&root).ok();
}

/// A private temp directory per test.
///
/// Keyed on a counter rather than the clock: these tests run in parallel, and a
/// nanosecond stamp is not unique on every platform — two tests that take the
/// same one share a directory and delete each other's fixtures.
fn tmp_dir(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "scode-{label}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).expect("temp dir");
    p
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
        Arc::new(KernelFsBackend::for_agent_descriptor(
            Arc::clone(&kernel),
            &AgentDescriptor {
                pid: format!("pid-{name}"),
                name: name.to_string(),
                owner_id: "test-owner".to_string(),
                zone_id: "root".to_string(),
                ..AgentDescriptor::default()
            },
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
/// Two co-hosted agents plan in their own workspaces.
///
/// The plan file resolved from the PROCESS working directory, which for a
/// co-hosted agent is the daemon's — one file for every agent on that daemon, so
/// two of them planning at once overwrote each other and the plan the user was
/// asked to approve was not necessarily the one the agent wrote.
#[test]
fn a_cohosted_agents_plan_lives_in_its_own_workspace() {
    // This test asserts the workspace branch of plan resolution: with no
    // `$SUDOCODE_PLAN_FILE`, the plan lands in the agent's own working root on
    // its own filesystem. That env var takes precedence when set (correct
    // product behavior — a live CLI points it at the session's plan), so run
    // this under the var unset regardless of the ambient environment. Without
    // this the test passes in CI (clean env) but fails inside a live scode,
    // whose process has the var pointing at its own session plan.
    struct UnsetPlanFileEnv(Option<String>);
    impl Drop for UnsetPlanFileEnv {
        fn drop(&mut self) {
            match &self.0 {
                Some(prev) => std::env::set_var("SUDOCODE_PLAN_FILE", prev),
                None => std::env::remove_var("SUDOCODE_PLAN_FILE"),
            }
        }
    }
    let _plan_env = UnsetPlanFileEnv(std::env::var("SUDOCODE_PLAN_FILE").ok());
    std::env::remove_var("SUDOCODE_PLAN_FILE");

    let kernel = kernel_with_root_backend();
    let agent = |name: &str, workspace: &str| -> Arc<dyn FsBackend> {
        Arc::new(KernelFsBackend::for_agent_descriptor(
            Arc::clone(&kernel),
            &AgentDescriptor {
                pid: format!("pid-{name}"),
                name: name.to_string(),
                owner_id: "test-owner".to_string(),
                zone_id: "root".to_string(),
                ..AgentDescriptor::default()
            },
            workspace.to_string(),
        ))
    };
    let alice = agent("alice", "/proc/1/workspace");
    let bob = agent("bob", "/proc/2/workspace");

    let path = runtime::plan_store::write_plan(
        "## Alice
1. ship it",
        &alice,
    )
    .expect("a co-hosted agent should be able to write its plan");
    assert_eq!(
        path.to_string_lossy().replace('\\', "/"),
        "/proc/1/workspace/.sudocode/plan.md",
        "the plan belongs in the agent's own workspace"
    );
    assert!(
        !std::path::Path::new(&path).exists(),
        "and not on the host filesystem"
    );

    assert_eq!(
        runtime::plan_store::read_plan(&alice).as_deref(),
        Some(
            "## Alice
1. ship it"
        ),
        "it reads back through the same filesystem it was written to"
    );
    assert_eq!(
        runtime::plan_store::read_plan(&bob),
        None,
        "and the agent next to it has no plan at all"
    );
}

/// A path outside every mount reads back a transparent error, not a bare
/// not-found. The mount table covers `/mnt`; a read under `/elsewhere` routes
/// to no mount, and the backend uses `is_mounted` to say so — the message
/// names the fix (mount its root) instead of implying the file is missing.
#[test]
fn a_path_outside_all_mounts_reads_a_transparent_error() {
    let kernel = Arc::new(Kernel::new());
    let backend: Arc<dyn ObjectStore> = Arc::new(MemStore::default());
    // Mount a SUBTREE, not `/`, so `/elsewhere` is genuinely unmounted.
    kernel
        .vfs_router_arc()
        .add_mount("/mnt", "root", Some(backend), false);
    let fs = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &AgentDescriptor {
            pid: "pid-agent-x".to_string(),
            name: "agent-x".to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..AgentDescriptor::default()
        },
        "/mnt",
    );

    let err = fs
        .read("/elsewhere/secret.txt")
        .expect_err("a path under no mount must be an error");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(
        err.to_string().contains("outside this session's mounts"),
        "unmounted path must read as a transparent mounts error, got: {err}",
    );
}

#[derive(Default)]
struct FilePolicy {
    denied: Mutex<HashSet<String>>,
    revoke_after_read: Mutex<HashSet<String>>,
    read_denied: Mutex<HashSet<String>>,
    agent: Mutex<Option<String>>,
    broken: Mutex<Option<String>>,
    calls: Mutex<Vec<(String, Permission, OperationContext, Option<String>)>>,
}

impl FilePolicy {
    fn deny(&self, path: &str) {
        self.denied.lock().unwrap().insert(path.to_string());
    }
}

impl PermissionProvider for FilePolicy {
    fn check(
        &self,
        path: &str,
        route: Option<&kernel::vfs_router::RouteResult>,
        permission: Permission,
        ctx: &OperationContext,
    ) -> Result<(), KernelError> {
        self.calls.lock().unwrap().push((
            path.to_string(),
            permission,
            ctx.clone(),
            route.map(|route| route.zone_id.clone()),
        ));
        if self.broken.lock().unwrap().as_deref() == Some(path) {
            return Err(KernelError::IOError("policy store unavailable".into()));
        }
        let agent = self.agent.lock().unwrap();
        let wrong_subject = agent.as_deref().is_some_and(|agent| {
            ctx.subject_type != "agent" || ctx.subject_id.as_deref() != Some(agent)
        });
        if wrong_subject
            || self.denied.lock().unwrap().contains(path)
            || (permission == Permission::Read && self.read_denied.lock().unwrap().contains(path))
        {
            return Err(KernelError::PermissionDenied(path.to_string()));
        }
        if permission == Permission::Read && self.revoke_after_read.lock().unwrap().remove(path) {
            self.deny(path);
        }
        Ok(())
    }
}

fn install_file_policy(kernel: &Kernel) -> Arc<FilePolicy> {
    let policy = Arc::new(FilePolicy::default());
    let provider: Box<dyn PermissionProvider> = Box::new(SharedFilePolicy(Arc::clone(&policy)));
    kernel.set_permission_provider(Arc::new(provider));
    policy
}

struct SharedFilePolicy(Arc<FilePolicy>);

impl PermissionProvider for SharedFilePolicy {
    fn check(
        &self,
        path: &str,
        route: Option<&kernel::vfs_router::RouteResult>,
        permission: Permission,
        ctx: &OperationContext,
    ) -> Result<(), KernelError> {
        self.0.check(path, route, permission, ctx)
    }
}

#[test]
fn metadata_operations_observe_read_denial() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.write("secret.txt", b"secret").unwrap();
    fs.link("secret-link", "/ws/secret.txt").unwrap();
    let policy = install_file_policy(&kernel);
    policy.deny("/ws/secret.txt");
    policy.deny("/ws/secret-link");
    policy.deny("/ws");

    let results = [
        fs.stat("secret.txt").map(|_| ()),
        fs.exists("secret.txt").map(|_| ()),
        fs.readdir("/ws").map(|_| ()),
        fs.symlink_metadata("secret-link").map(|_| ()),
        fs.read_link("secret-link").map(|_| ()),
        fs.is_append_stream("secret.txt").map(|_| ()),
    ];
    for result in results {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
    assert!(fs.exists("missing.txt").is_ok_and(|exists| !exists));
}

#[test]
fn discovery_filters_children_and_rechecks_revocation() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.create_dir_all("nested").unwrap();
    fs.create_dir_all("private").unwrap();
    for path in [
        "visible.txt",
        "secret.txt",
        "nested/visible.txt",
        "private/secret.txt",
    ] {
        fs.write(path, b"needle").unwrap();
    }
    let policy = install_file_policy(&kernel);
    policy.deny("/ws/secret.txt");
    policy.deny("/ws/private");

    let mut names: Vec<_> = fs
        .readdir("/ws")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, ["nested", "visible.txt"]);
    let glob = glob_search(&fs, "**/*.txt", None).unwrap();
    assert_eq!(glob.num_files, 2);
    assert!(glob.filenames.iter().all(|path| !path.contains("secret")));
    let grep = grep_search(&fs, &grep_input("needle", "/ws")).unwrap();
    assert_eq!(grep.num_files, 2);

    policy.deny("/ws/visible.txt");
    assert_eq!(glob_search(&fs, "**/*.txt", None).unwrap().num_files, 1);
    assert_eq!(
        grep_search(&fs, &grep_input("needle", "/ws"))
            .unwrap()
            .num_files,
        1
    );
    policy.deny("/ws");
    assert_eq!(
        glob_search(&fs, "**/*.txt", None).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        grep_search(&fs, &grep_input("needle", "/ws"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn discovery_reports_policy_failure() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.write("visible.txt", b"needle").unwrap();
    let policy = install_file_policy(&kernel);
    *policy.broken.lock().unwrap() = Some("/ws/visible.txt".into());
    let error = fs.readdir("/ws").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error.to_string().contains("policy store unavailable"));
}

#[test]
fn metadata_policy_receives_agent_subject_and_owning_zone() {
    let kernel = kernel_with_root_backend();
    let fs = KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "lookup-zone",
        "agent-x",
        "/ws",
    );
    fs.write("/ws/file.txt", b"root mount").unwrap();
    let policy = install_file_policy(&kernel);
    fs.stat("/ws/file.txt").unwrap();
    let calls = policy.calls.lock().unwrap();
    let (path, permission, ctx, zone) = calls.last().expect("metadata must consult the policy");
    assert_eq!(path, "/ws/file.txt");
    assert_eq!(*permission, Permission::Read);
    assert_eq!(ctx.user_id, "test-owner");
    assert_eq!(ctx.agent_id.as_deref(), Some("agent-x"));
    assert_eq!(ctx.subject_type, "agent");
    assert_eq!(ctx.subject_id.as_deref(), Some("agent-x"));
    assert!(!ctx.is_system && !ctx.is_admin);
    assert_eq!(ctx.zone_id, "lookup-zone");
    assert_eq!(zone.as_deref(), Some("root"));
}

#[test]
fn typed_entry_creation_checks_write_before_mutating_metadata() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.create_dir_all("/ws").unwrap();
    let policy = install_file_policy(&kernel);
    for path in ["/ws/denied-dir", "/ws/denied-link", "/ws/denied-log"] {
        policy.deny(path);
    }
    let results = [
        fs.create_dir_all("denied-dir"),
        fs.link("denied-link", "/ws/target"),
        fs.create_append_log("denied-log", 0),
    ];
    for result in results {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
    for path in ["/ws/denied-dir", "/ws/denied-link", "/ws/denied-log"] {
        assert!(kernel.sys_stat(path, "root").is_none());
    }
}

#[test]
fn agents_with_one_owner_keep_separate_subjects() {
    let kernel = kernel_with_root_backend();
    let alice = vfs_backend(&kernel);
    let bob =
        KernelFsBackend::for_agent(Arc::clone(&kernel), "test-owner", "root", "agent-y", "/ws");
    alice.create_dir_all("/ws").unwrap();
    alice.write("shared-owner.txt", b"agent-x grant").unwrap();
    let policy = install_file_policy(&kernel);
    *policy.agent.lock().unwrap() = Some("agent-x".into());
    assert_eq!(alice.read("shared-owner.txt").unwrap(), b"agent-x grant");
    assert!(alice.exists("shared-owner.txt").unwrap());
    for result in [
        bob.read("shared-owner.txt").map(|_| ()),
        bob.stat("shared-owner.txt").map(|_| ()),
        bob.readdir("/ws").map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
    *policy.agent.lock().unwrap() = Some("agent-y".into());
    assert!(bob.exists("shared-owner.txt").unwrap());
    assert_eq!(
        alice.read("shared-owner.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[cfg(target_os = "linux")]
struct AgentAuth;

#[cfg(target_os = "linux")]
impl transport::auth::AuthProvider for AgentAuth {
    fn resolve(
        &self,
        credentials: &transport::auth::AuthCredentials<'_>,
    ) -> Result<OperationContext, tonic_host::Status> {
        if credentials.token != "agent-key" {
            return Err(match credentials.token {
                "denied-key" => tonic_host::Status::permission_denied("agent access denied"),
                "unavailable-key" => tonic_host::Status::internal("credential store unavailable"),
                _ => tonic_host::Status::unauthenticated("invalid agent key"),
            });
        }
        let mut ctx = OperationContext::new("test-owner", "root", false, Some("agent-x"), false);
        ctx.subject_type = "agent".into();
        ctx.subject_id = Some("agent-x".into());
        Ok(ctx)
    }
}

#[cfg(target_os = "linux")]
fn remote_backend(
    kernel: &Arc<Kernel>,
) -> (
    runtime::NexusVfsFsBackend,
    transport::grpc::VfsGrpcHandle,
    Arc<nexus_vfs_client::NexusVfsClient>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let host = transport::grpc::spawn(
        Arc::clone(kernel),
        transport::grpc::VfsGrpcConfig {
            bind_addr: addr,
            tls: None,
            max_message_bytes: 64 * 1024 * 1024,
            server_version: "test".into(),
        },
        Arc::new(AgentAuth),
    )
    .unwrap();
    let client =
        Arc::new(nexus_vfs_client::NexusVfsClient::connect(&format!("http://{addr}")).unwrap());
    let fs = runtime::NexusVfsFsBackend::from_arc(Arc::clone(&client), "agent-key".into());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match fs.stat("/ws/ready.txt") {
            Ok(_) => break,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(error) => panic!("VFS host did not become ready: {error}"),
        }
    }
    (fs, host, client)
}

#[test]
#[cfg(target_os = "linux")]
fn remote_existence_preserves_admission_and_policy_errors() {
    let kernel = kernel_with_root_backend();
    let local = vfs_backend(&kernel);
    local.create_dir_all("/ws").unwrap();
    local.write("ready.txt", b"ready").unwrap();
    install_file_policy(&kernel);
    let (remote, _host, client) = remote_backend(&kernel);
    assert!(remote.exists("/ws/ready.txt").unwrap());
    assert!(!remote.exists("/ws/missing.txt").unwrap());
    let unverified =
        runtime::NexusVfsFsBackend::from_arc(Arc::clone(&client), "invalid-key".into());
    assert_eq!(
        unverified.exists("/ws/ready.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let denied = runtime::NexusVfsFsBackend::from_arc(Arc::clone(&client), "denied-key".into());
    assert_eq!(
        denied.exists("/ws/ready.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let unavailable =
        runtime::NexusVfsFsBackend::from_arc(Arc::clone(&client), "unavailable-key".into());
    assert_eq!(
        unavailable.exists("/ws/ready.txt").unwrap_err().kind(),
        io::ErrorKind::Other
    );
    // A rejected request cannot poison the credential used by the shared channel.
    assert!(!remote.exists("/ws/missing.txt").unwrap());
}

#[test]
#[cfg(target_os = "linux")]
fn rebac_grants_to_agents_do_not_transfer_from_their_owner() {
    use lib::types::ReBACTuple;
    use nexus_rebac::{
        InMemoryReBACTupleStore, ReBACGraphCache, ReBACTupleStore, RebacPermissionProvider,
    };

    let kernel = kernel_with_root_backend();
    let alice = vfs_backend(&kernel);
    let bob =
        KernelFsBackend::for_agent(Arc::clone(&kernel), "test-owner", "root", "agent-y", "/ws");
    alice.create_dir_all("/ws").unwrap();
    alice.write("agent.txt", b"needle").unwrap();
    alice.write("owner.txt", b"owner secret").unwrap();
    let store = Arc::new(InMemoryReBACTupleStore::new());
    let grant = |path: &str, subject_type: &str, subject: &str| {
        let tuple = ReBACTuple {
            object_type: "file".into(),
            object_id: path.into(),
            relation: "reader".into(),
            subject_type: subject_type.into(),
            subject_id: subject.into(),
            subject_relation: None,
        };
        let key = nexus_rebac::tuple_key::encode("root", &tuple).unwrap();
        store.put(&key, b"").unwrap();
        key
    };
    grant("/ws", "agent", "agent-x");
    let agent_grant = grant("/ws/agent.txt", "agent", "agent-x");
    grant("/ws/owner.txt", "user", "test-owner");
    let provider: Box<dyn PermissionProvider> = Box::new(RebacPermissionProvider::new(Arc::new(
        ReBACGraphCache::new(store.clone()),
    )));
    kernel.set_permission_provider(Arc::new(provider));
    assert_eq!(alice.read("agent.txt").unwrap(), b"needle");
    assert_eq!(alice.stat("agent.txt").unwrap().len, 6);
    assert_eq!(
        alice
            .readdir("/ws")
            .unwrap()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["agent.txt"]
    );
    assert_eq!(glob_search(&alice, "**/*.txt", None).unwrap().num_files, 1);
    assert_eq!(
        grep_search(&alice, &grep_input("needle", "/ws"))
            .unwrap()
            .num_files,
        1
    );
    assert_eq!(
        alice.read("owner.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        bob.stat("agent.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        bob.readdir("/ws").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(store.delete(&agent_grant).unwrap());
    assert_eq!(
        alice.read("agent.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        alice.exists("agent.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(glob_search(&alice, "**/*.txt", None).unwrap().num_files, 0);
    assert_eq!(
        grep_search(&alice, &grep_input("needle", "/ws"))
            .unwrap()
            .num_files,
        0
    );
}

#[test]
#[cfg(target_os = "linux")]
fn remote_append_preserves_data_when_read_is_denied_or_unavailable() {
    let kernel = kernel_with_root_backend();
    let local = vfs_backend(&kernel);
    local.create_dir_all("/ws").unwrap();
    local.write("ready.txt", b"original").unwrap();
    let policy = install_file_policy(&kernel);
    let (remote, _host, _client) = remote_backend(&kernel);
    policy
        .read_denied
        .lock()
        .unwrap()
        .insert("/ws/ready.txt".into());
    assert_eq!(
        remote
            .append("/ws/ready.txt", b"replacement")
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    policy.read_denied.lock().unwrap().clear();
    assert_eq!(remote.read("/ws/ready.txt").unwrap(), b"original");
    *policy.broken.lock().unwrap() = Some("/ws/ready.txt".into());
    assert!(remote.append("/ws/ready.txt", b"replacement").is_err());
    *policy.broken.lock().unwrap() = None;
    assert_eq!(remote.read("/ws/ready.txt").unwrap(), b"original");
    remote.append("/ws/new.txt", b"new").unwrap();
    assert_eq!(remote.read("/ws/new.txt").unwrap(), b"new");
}

#[test]
#[cfg(target_os = "linux")]
fn remote_stat_preserves_modification_time() {
    let kernel = kernel_with_root_backend();
    let local = vfs_backend(&kernel);
    local.create_dir_all("/ws").unwrap();
    local.write("ready.txt", b"ready").unwrap();
    install_file_policy(&kernel);
    let expected = local.stat("ready.txt").unwrap().modified;
    assert!(expected.is_some());
    let (remote, _host, _client) = remote_backend(&kernel);
    assert_eq!(remote.stat("/ws/ready.txt").unwrap().modified, expected);
}

#[test]
fn glob_drops_a_path_revoked_after_directory_discovery() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.create_dir_all("/ws").unwrap();
    fs.write("revoked.txt", b"needle").unwrap();
    let policy = install_file_policy(&kernel);
    policy
        .revoke_after_read
        .lock()
        .unwrap()
        .insert("/ws/revoked.txt".into());
    assert!(glob_search(&fs, "**/*.txt", None)
        .unwrap()
        .filenames
        .is_empty());
}

#[test]
fn glob_stats_each_file_once_after_directory_discovery() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.create_dir_all("/ws").unwrap();
    for i in 0..20 {
        fs.write(&format!("file-{i}.txt"), b"needle").unwrap();
    }
    let policy = install_file_policy(&kernel);
    assert_eq!(glob_search(&fs, "**/*.txt", None).unwrap().num_files, 20);
    let calls = policy.calls.lock().unwrap();
    for i in 0..20 {
        let path = format!("/ws/file-{i}.txt");
        assert_eq!(
            calls
                .iter()
                .filter(|(checked, permission, _, _)| checked == &path
                    && *permission == Permission::Read)
                .count(),
            2,
            "one listing admission and one stat for {path}"
        );
    }
}
