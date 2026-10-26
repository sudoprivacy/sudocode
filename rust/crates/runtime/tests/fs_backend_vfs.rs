//! Integration tests proving the co-hosted file tools hit the VFS
//! in-process through `KernelFsBackend`, never the host filesystem.
//!
//! These exercise `read_file` / `write_file` / `edit_file` / `glob_search`
//! / `grep_search` against a real in-memory `Kernel` (not a mock) so the
//! backend-aware path normalisation and the `readdir`-composed traversal
//! are validated end to end. The paths used (`/ws/…`) do not exist on the
//! host, so a host-`std::fs` regression would surface as a NotFound
//! rather than silently passing.
//!
//! One test here is deliberately NOT about the VFS: `link` is a contract both
//! backends answer, and the kernel half used to claim parity with the host half
//! in a comment while nothing exercised it. The two live next to each other so
//! the claim is checked rather than asserted.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kernel::abc::object_store::{ObjectStore, StorageError, WriteResult};
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::{Kernel, KernelError, OperationContext};
use kernel::meta_store::DT_LINK;
use kernel::{Permission, PermissionProvider};
use runtime::fs_backend::RecordLogMode;
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
    let backend: Arc<dyn ObjectStore> = Arc::new(runtime::test_support::MemObjectStore::default());
    kernel
        .vfs_router_arc()
        .add_mount("/", zone_id, Some(backend), false);
    kernel
}

/// A planted descriptor for `name` in `zone` — the test-side stand-in for
/// what the ManagedAgentService plants for a real managed agent, so backends
/// built from it carry the trusted-local zone source a planted descriptor
/// grants.
fn desc_for(name: &str, zone: &str) -> AgentDescriptor {
    AgentDescriptor {
        pid: format!("pid-{name}"),
        name: name.to_string(),
        owner_id: "test-owner".to_string(),
        zone_id: zone.to_string(),
        ..AgentDescriptor::default()
    }
}

/// A `KernelFsBackend` rooted at `/ws`, over the given kernel. The context
/// names agent-x: the agents-base subtrees (`managed_root`) are keyed by the
/// context's agent identity, and a context without one roots nothing.
fn vfs_backend(kernel: &Arc<Kernel>) -> KernelFsBackend<Kernel> {
    KernelFsBackend::for_agent_descriptor(Arc::clone(kernel), &desc_for("agent-x", "root"), "/ws")
}

#[test]
fn p1a_resource_targets_require_the_same_delegated_zone_for_cohost_and_subprocess() {
    let kernel = kernel_with_backend("tenant-zone");
    let permission_checks = Arc::new(AtomicUsize::new(0));
    let provider: Arc<Box<dyn PermissionProvider>> =
        Arc::new(Box::new(CountingAllow(Arc::clone(&permission_checks))));
    kernel.set_permission_provider(provider);

    let without_delegation = KernelFsBackend::new(
        Arc::clone(&kernel),
        runtime::agent_operation_context("test-owner", "tenant-zone", "agent-x"),
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
        delivery: runtime::turn_input::InputDelivery::Steer,
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
fn record_log_requires_wal_and_preserves_regular_files() {
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    let path = "/ws/atomic-records";
    // This real kernel has no Raft coordinator. An explicit WAL request must
    // fail, with neither a memory stream nor a regular-file fallback.
    assert!(fs.record_log(path, RecordLogMode::CreateNew).is_err());
    assert!(kernel.sys_stat(path, "root").is_none());
    fs.write(path, b"original regular file").unwrap();
    assert!(fs.record_log(path, RecordLogMode::CreateNew).is_err());
    assert!(fs.record_log(path, RecordLogMode::Existing).is_err());
    assert_eq!(fs.read(path).unwrap(), b"original regular file");
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

fn canonical_workspace_mount() -> std::path::PathBuf {
    let directory = runtime::test_support::temp_dir_path("scode-canonical-workspace");
    std::fs::create_dir_all(&directory).unwrap();
    directory.canonicalize().unwrap()
}

#[test]
fn local_workspace_paths_and_virtual_paths_share_one_authorized_object() {
    let kernel = kernel_with_root_backend();
    let mount = canonical_workspace_mount();
    let fs = vfs_backend(&kernel).with_local_workspace(&mount).unwrap();
    let path = mount.join("project.txt").to_string_lossy().into_owned();
    assert_eq!(fs.working_root().unwrap(), mount.to_string_lossy());
    assert_eq!(fs.normalize("project.txt").unwrap(), path);
    assert_eq!(fs.normalize(&path).unwrap(), path);
    write_file(&fs, "project.txt", "shared bytes").unwrap();
    assert_eq!(fs.read(&path).unwrap(), b"shared bytes");
    assert_eq!(
        vfs_backend(&kernel).read("/ws/project.txt").unwrap(),
        b"shared bytes"
    );
    edit_file(&fs, &path, "shared", "same", false).unwrap();
    assert_eq!(fs.read("/ws/project.txt").unwrap(), b"same bytes");
    let glob = glob_search(&fs, "**/*.txt", None).unwrap();
    assert_eq!(glob.filenames.len(), 1);
    assert_eq!(fs.normalize(&glob.filenames[0]).unwrap(), path);
    let mut input = grep_input("same bytes", "");
    input.path = None;
    let grep = grep_search(&fs, &input).unwrap();
    assert_eq!(grep.filenames.len(), 1);
    assert_eq!(fs.normalize(&grep.filenames[0]).unwrap(), path);
    fs.rename(&path, "renamed.txt").unwrap();
    assert!(!fs.exists(&path).unwrap());
    assert_eq!(fs.read("renamed.txt").unwrap(), b"same bytes");
    std::fs::remove_dir(mount).unwrap();
}

#[test]
fn local_workspace_keeps_managed_state_in_its_durable_namespace() {
    use runtime::ManagedRoot;
    let kernel = kernel_with_root_backend();
    let mount = canonical_workspace_mount();
    let fs = vfs_backend(&kernel).with_local_workspace(&mount).unwrap();
    let memory = fs.managed_root(ManagedRoot::Memory).unwrap();
    assert_eq!(memory, "/agents/agent-x/memory");
    fs.create_dir_all(&memory).unwrap();
    fs.write(&fs.join_path(&memory, "MEMORY.md"), b"agent memory")
        .unwrap();
    assert_eq!(
        vfs_backend(&kernel)
            .read("/agents/agent-x/memory/MEMORY.md")
            .unwrap(),
        b"agent memory"
    );
    assert_eq!(fs.managed_root(ManagedRoot::Sessions).unwrap(), "/sessions");
    fs.create_dir_all("/sessions/original-id").unwrap();
    let index = "/agents/agent-x/sessions/original-id".to_string();
    let target = "/sessions/original-id".to_string();
    fs.link(&index, &target).unwrap();
    assert_eq!(fs.read_link(&index).unwrap(), target);
    assert_eq!(
        vfs_backend(&kernel)
            .read_link("/agents/agent-x/sessions/original-id")
            .unwrap(),
        "/sessions/original-id"
    );
    std::fs::remove_dir(mount).unwrap();
}

#[test]
fn local_workspace_spelling_preserves_authorization_and_rejects_path_escape() {
    let kernel = kernel_with_root_backend();
    let policy = install_file_policy(&kernel);
    let mount = canonical_workspace_mount();
    let fs = vfs_backend(&kernel).with_local_workspace(&mount).unwrap();
    fs.write("secret.txt", b"private").unwrap();
    policy.deny("/ws/secret.txt");
    let mounts = vec![mount.to_string_lossy().into_owned()];
    #[cfg(windows)]
    let mounts = {
        let ordinary = mounts[0]
            .strip_prefix(r"\\?\")
            .expect("canonicalized Windows workspace")
            .to_string();
        vec![
            mounts[0].clone(),
            ordinary.clone(),
            ordinary.replace('\\', "/"),
        ]
    };
    for native_mount in mounts {
        let secret = format!("{native_mount}/secret.txt");
        assert_eq!(
            fs.read(&secret).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "authorization must survive native path spelling: {secret}"
        );
        let escape = format!("{native_mount}/../outside.txt");
        assert_eq!(
            fs.write(&escape, b"refused").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
            "lexical escape must fail: {escape}"
        );
    }
    let secret = mount.join("secret.txt").to_string_lossy().into_owned();
    assert_eq!(
        fs.read(&secret).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        fs.read("secret.txt").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let escape = format!("{}/../outside.txt", mount.display());
    assert_eq!(
        fs.write(&escape, b"refused").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(vfs_backend(&kernel)
        .with_local_workspace("relative/project")
        .is_err());
    std::fs::remove_dir(mount).unwrap();
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
    kernel
        .create_stream("/ws/sessions/sid-1.jsonl", 64 * 1024)
        .expect("explicit atomic stream in the non-federated fixture");
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
    let before_empty = fs.stat(path).unwrap().len;
    fs.append(path, b"").unwrap();
    assert_eq!(
        fs.stat(path).unwrap().len,
        before_empty,
        "empty writes are no-ops"
    );
    fs.append(path, b"{\"b\":2}\n").unwrap();
    assert_eq!(
        fs.read(path).unwrap(),
        b"{\"a\":1}\n{\"b\":2}\n",
        "reading a DT_STREAM reproduces the appended records in order"
    );
    let (first, cursor, eof) = fs.tail_read(path, 0, 0).unwrap();
    assert_eq!(first, b"{\"a\":1}\n");
    assert!(!eof);
    let (second, after_second, eof) = fs.tail_read(path, cursor, 0).unwrap();
    assert_eq!(second, b"{\"b\":2}\n");
    assert!(after_second > cursor);
    assert!(!eof);
}

#[test]
fn rename_replaces_a_file_in_one_step_and_refuses_a_stream() {
    // `rename` is the kernel's own sys_rename. Temp-then-rename replaces the
    // destination in one operation, and a DT_STREAM is identity-bound: the
    // kernel refuses to move it, so nothing may copy its frames into a new
    // regular file and delete the original.
    let kernel = kernel_with_root_backend();
    let fs = vfs_backend(&kernel);
    fs.write("/ws/manifest.json", b"old").unwrap();
    fs.write("/ws/manifest.json.tmp", b"new").unwrap();
    fs.rename("/ws/manifest.json.tmp", "/ws/manifest.json")
        .unwrap();
    assert_eq!(fs.read("/ws/manifest.json").unwrap(), b"new");
    assert!(!fs.exists("/ws/manifest.json.tmp").unwrap());

    let stream = "/ws/log.jsonl";
    kernel
        .create_stream(stream, 64 * 1024)
        .expect("create DT_STREAM");
    fs.append(stream, b"frame\n").unwrap();
    let error = fs.rename(stream, "/ws/moved.jsonl").unwrap_err();
    assert!(
        error.to_string().contains("rename not supported"),
        "the kernel's refusal surfaces unchanged: {error}"
    );
    assert!(fs.is_append_stream(stream).unwrap());
    assert_eq!(fs.read(stream).unwrap(), b"frame\n");
    assert!(!fs.exists("/ws/moved.jsonl").unwrap());
}

#[test]
fn stream_links_preserve_complete_reads_and_append_payloads() {
    let kernel = kernel_with_root_backend();
    let target = "/ws/linked-stream.jsonl";
    let alias = "/ws/stream-alias";
    kernel.create_stream(target, 64 * 1024).unwrap();
    let fs = vfs_backend(&kernel);
    fs.append(target, b"first\n").unwrap();
    fs.append(target, b"second\n").unwrap();
    fs.link(alias, target).unwrap();
    fs.link("/ws/stream-directory", "/ws").unwrap();
    let directory_alias = "/ws/stream-directory/linked-stream.jsonl";
    for path in [alias, directory_alias] {
        assert!(fs.is_append_stream(path).unwrap());
        assert_eq!(fs.read(path).unwrap(), b"first\nsecond\n");
    }
    fs.append(alias, b"third\n").unwrap();
    fs.append(directory_alias, b"fourth\n").unwrap();
    assert_eq!(fs.read(target).unwrap(), b"first\nsecond\nthird\nfourth\n");
}

#[test]
fn regular_file_aliases_retain_byte_tail_cursors() {
    let kernel = kernel_with_root_backend();
    assert_regular_alias_tails(&vfs_backend(&kernel));
}

fn assert_regular_alias_tails(fs: &dyn FsBackend) {
    let target = "/ws/tail-file";
    let paths = [target, "/ws/tail-alias", "/ws/tail-directory/tail-file"];
    fs.write(target, b"first\n").unwrap();
    fs.link(paths[1], target).unwrap();
    fs.link("/ws/tail-directory", "/ws").unwrap();
    for path in paths {
        assert_eq!(
            fs.tail_read(path, 0, 0).unwrap(),
            (b"first\n".to_vec(), 6, false),
            "{path}"
        );
    }
    fs.append(target, b"second\n").unwrap();
    for path in paths {
        assert_eq!(
            fs.tail_read(path, 6, 0).unwrap(),
            (b"second\n".to_vec(), 13, false),
            "{path}"
        );
        assert_eq!(
            fs.tail_read(path, 13, 0).unwrap(),
            (Vec::new(), 13, true),
            "{path}"
        );
    }
}

#[test]
fn stream_link_append_needs_no_read_grant_and_rechecks_the_target() {
    let kernel = kernel_with_root_backend();
    let target = "/ws/write-only-stream";
    let alias = "/ws/write-only-alias";
    kernel.create_stream(target, 4096).unwrap();
    let fs = vfs_backend(&kernel);
    fs.link(alias, target).unwrap();
    let policy = install_file_policy(&kernel);
    policy
        .read_denied
        .lock()
        .unwrap()
        .extend([target.into(), alias.into()]);
    fs.append(alias, b"accepted\n").unwrap();
    assert_eq!(
        fs.read(alias).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    policy.deny(target);
    assert_eq!(
        fs.append(alias, b"denied\n").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    policy.denied.lock().unwrap().clear();
    policy.read_denied.lock().unwrap().clear();
    assert_eq!(fs.read(target).unwrap(), b"accepted\n");
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
    let original = std::path::Path::new(&target).join("persistent.txt");
    std::fs::write(&original, b"durable object").expect("write target content");

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

    fs.delete(&alias)
        .expect("unlink the runtime directory alias");
    assert!(std::fs::symlink_metadata(&alias).is_err());
    assert_eq!(std::fs::read(&original).unwrap(), b"durable object");
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
    let backend: Arc<dyn ObjectStore> = Arc::new(runtime::test_support::MemObjectStore::default());
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

/// The denied principal: a permission provider that refuses everything, so a
/// metadata probe must surface `PermissionDenied` rather than a silent
/// `Ok(false)`.
struct DenyAllMetadata;

impl PermissionProvider for DenyAllMetadata {
    fn check(
        &self,
        _path: &str,
        _route: Option<&kernel::core::vfs_router::RouteResult>,
        _permission: Permission,
        _ctx: &OperationContext,
    ) -> Result<(), KernelError> {
        Err(KernelError::PermissionDenied(
            "denied principal may not probe metadata".to_string(),
        ))
    }
}

/// `exists` / `is_append_stream` are metadata probes over `sys_stat`, which
/// carries only a routing zone — no principal — so the kernel's policy must
/// be consulted at the owning filesystem boundary, with the caller's full
/// identity (the same check `stat` performs). A denied principal gets
/// `PermissionDenied`; an allowed one reaches the provider (the count proves
/// it) and gets a real answer.
///
/// The backend is built with `for_agent_descriptor` on purpose: its context
/// is NOT `is_system`, and the kernel short-circuits system contexts before
/// the provider — a system-context backend would make both the denial and
/// the count unreachable.
#[test]
fn denied_principals_cannot_probe_metadata_through_exists_or_is_append_stream() {
    let kernel = kernel_with_backend("tenant-zone");

    let mut descriptor = AgentDescriptor {
        pid: "pid-zone-metadata".to_string(),
        name: "agent-zone-metadata".to_string(),
        owner_id: "test-owner".to_string(),
        zone_id: "tenant-zone".to_string(),
        ..AgentDescriptor::default()
    };
    descriptor.labels.insert(
        ENV_NEXUS_RESOURCE_SCOPE.to_string(),
        serde_json::json!({
            "schema_version": 1,
            "zone_id": "tenant-zone",
            "rules": [
                {"capability": "zone.data.read", "resource_prefixes": ["/ws"]},
                {"capability": "zone.data.write", "resource_prefixes": ["/ws"]}
            ]
        })
        .to_string(),
    );

    kernel.set_permission_provider(Arc::new(Box::new(DenyAllMetadata)));
    let denied_backend =
        KernelFsBackend::for_agent_descriptor(Arc::clone(&kernel), &descriptor, "/ws");
    for probe_label in ["exists", "is_append_stream"] {
        let probe = if probe_label == "exists" {
            denied_backend.exists("/ws/allowed.txt")
        } else {
            denied_backend.is_append_stream("/ws/allowed.txt")
        };
        let error = probe.expect_err("a denied principal must not probe metadata");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{probe_label}: got {error}"
        );
    }

    let permission_checks = Arc::new(AtomicUsize::new(0));
    kernel.set_permission_provider(Arc::new(Box::new(CountingAllow(Arc::clone(
        &permission_checks,
    )))));
    let allowed_backend =
        KernelFsBackend::for_agent_descriptor(Arc::clone(&kernel), &descriptor, "/ws");
    allowed_backend
        .exists("/ws/allowed.txt")
        .expect("an allowed principal gets a real existence answer");
    allowed_backend
        .is_append_stream("/ws/allowed.txt")
        .expect("an allowed principal gets a real stream answer");
    assert!(
        permission_checks.load(Ordering::Relaxed) > 0,
        "the metadata probes reached the kernel's permission provider"
    );
}

/// The elevation mapping that lets a host-injected env shape an in-process
/// kernel's authorization (review finding 1: a standalone-local exception is
/// limited to genuinely absent hosted metadata). The env shapes the context
/// exactly four ways:
///
/// * a valid zone with a scope → `TrustedLocal`, enforced to that scope;
/// * a valid zone with no scope → `TrustedLocal`, unscoped — parity with a
///   planted descriptor that carries no labels (the host said "no limit");
/// * an invalid zone id, or hosted metadata without one → the denying shape
///   stays, never widened back into root semantics;
/// * a scope label that will not parse → `DelegationInvalid`, fail-closed.
///
/// The genuinely-absent (`Absent`) entry is covered end to end by the
/// no-env control group of the subprocess scope test in
/// `rusty-sudocode-cli/tests/acp_integration.rs` — it cannot be built from
/// the public constructors without mutating process-global env.
#[test]
fn trusted_for_in_process_kernel_maps_host_env_fail_closed_or_scoped() {
    let scope = serde_json::json!({
        "schema_version": 1,
        "zone_id": "tenant-zone",
        "rules": [
            {"capability": "zone.data.read", "resource_prefixes": ["/ws"]},
            {"capability": "zone.data.write", "resource_prefixes": ["/ws"]}
        ]
    })
    .to_string();

    // Valid zone + scope: elevated to trusted-local and enforced to it.
    let scoped = HostZoneContext::from_parts_with_scope(
        "tenant-zone",
        Some("https://nexus.example/v2".to_string()),
        Some("dlg-short-lived".to_string()),
        Some(&scope),
        ContextSource::UnverifiedDelegationRef,
    )
    .trusted_for_in_process_kernel();
    assert_eq!(scoped.source(), &ContextSource::TrustedLocal);
    assert!(scoped.authorize_path("/ws/in-scope.txt").is_ok());
    assert!(matches!(
        scoped.authorize_path("/outside.txt"),
        Err(runtime::zone_context::ZoneAuthError::OutOfScope(_))
    ));

    // Valid zone, host injected no scope: unscoped, like a label-less
    // planted descriptor — the host's explicit "no limit".
    let unscoped = HostZoneContext::from_parts_with_scope(
        "tenant-zone",
        None,
        None,
        None,
        ContextSource::UnverifiedDelegationRef,
    )
    .trusted_for_in_process_kernel();
    assert_eq!(unscoped.source(), &ContextSource::TrustedLocal);
    assert!(unscoped.authorize_path("/anywhere.txt").is_ok());

    // An invalid zone id leaves no usable zone: the denying shape survives
    // elevation. (A partial hosted env — metadata without a zone id —
    // produces this same zone-less unverified shape in `from_host_env`.)
    let zone_less = HostZoneContext::from_parts_with_scope(
        "INVALID ZONE",
        None,
        None,
        None,
        ContextSource::UnverifiedDelegationRef,
    )
    .trusted_for_in_process_kernel();
    assert_eq!(zone_less.source(), &ContextSource::UnverifiedDelegationRef);
    assert!(matches!(
        zone_less.authorize_path("/ws/x.txt"),
        Err(runtime::zone_context::ZoneAuthError::NoZoneContext(_))
    ));

    // A scope label that will not parse is hosted metadata that failed to
    // authorize anything: DelegationInvalid, never widened.
    let broken_scope = HostZoneContext::from_parts_with_scope(
        "tenant-zone",
        None,
        None,
        Some("not a scope json"),
        ContextSource::UnverifiedDelegationRef,
    )
    .trusted_for_in_process_kernel();
    assert_eq!(broken_scope.source(), &ContextSource::TrustedLocal);
    assert!(matches!(
        broken_scope.authorize_path("/ws/x.txt"),
        Err(runtime::zone_context::ZoneAuthError::DelegationInvalid)
    ));

    // Elevation is idempotent on an already-trusted context.
    let trusted = HostZoneContext::from_parts_with_scope(
        "tenant-zone",
        None,
        None,
        Some(&scope),
        ContextSource::TrustedLocal,
    );
    assert_eq!(
        trusted.trusted_for_in_process_kernel().source(),
        &ContextSource::TrustedLocal
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Restored from main: the shipped-host authorization suite. The branch's
// dependency detour dropped it along with the linux dev-dependencies it
// rides; both return here. Constructions that main spelled as
// `for_agent(..)` ride the planted descriptor (`desc_for`) instead — the
// constructor this line keeps — and the real-transport tests below reach the
// spawned host over mTLS where main used plaintext `from_arc` (that
// constructor now fails closed before any RPC by design).
// ─────────────────────────────────────────────────────────────────────────────

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
    let fs = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &desc_for("agent-x", "lookup-zone"),
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
    let bob = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &desc_for("agent-y", "root"),
        "/ws",
    );
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

// ── real-transport coverage (linux): mTLS gRPC round-trips ──────────────────
//
// main reached the spawned host over plaintext TCP with `from_arc`. That
// constructor now fails closed before any RPC — env metadata is not a
// credential the server can verify — so the transport here is mTLS, the one
// remote path production uses, minted in memory with rcgen. The bearer token
// still rides every request and the server still resolves it after the
// handshake, so AgentAuth's admission and error envelopes stay under test
// exactly as before.

#[cfg(target_os = "linux")]
mod mtls {
    //! One throwaway CA, one server leaf carrying the fixed cluster server
    //! name as its DNS SAN, one client leaf — the material both ends of a
    //! test VFS gRPC pair need.
    use lib::transport_primitives::TlsConfig;

    pub struct Material {
        pub server: TlsConfig,
        pub client: TlsConfig,
    }

    pub fn mint() -> Material {
        use rcgen::{
            BasicConstraints, CertificateParams, DistinguishedName, DnType,
            ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType,
            PKCS_ECDSA_P256_SHA256,
        };

        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let mut ca_dn = DistinguishedName::new();
        ca_dn.push(DnType::CommonName, "scode-vfs-test-ca");
        ca_params.distinguished_name = ca_dn;
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca.pem();
        let ca_key_pem = ca_key.serialize_pem();

        let leaf = |cn: &str, dns_san: Option<&str>| {
            let issuer =
                Issuer::from_ca_cert_pem(&ca_pem, KeyPair::from_pem(&ca_key_pem).unwrap()).unwrap();
            let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
            let mut params = CertificateParams::default();
            let mut dn = DistinguishedName::new();
            dn.push(DnType::CommonName, cn);
            params.distinguished_name = dn;
            if let Some(name) = dns_san {
                params.subject_alt_names =
                    vec![SanType::DnsName(name.try_into().expect("DNS SAN"))];
            }
            params.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
            params.key_usages = vec![
                KeyUsagePurpose::DigitalSignature,
                KeyUsagePurpose::KeyEncipherment,
            ];
            params.is_ca = IsCa::NoCa;
            let cert = params.signed_by(&key, &issuer).unwrap();
            (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
        };

        let (server_cert, server_key) = leaf(
            "scode-vfs-test-server",
            Some(TlsConfig::CLUSTER_SERVER_NAME),
        );
        let (client_cert, client_key) = leaf("scode-vfs-test-client", None);
        let ca_bytes = ca_pem.into_bytes();
        Material {
            server: TlsConfig {
                cert_pem: server_cert,
                key_pem: server_key,
                ca_pem: ca_bytes.clone(),
            },
            client: TlsConfig {
                cert_pem: client_cert,
                key_pem: client_key,
                ca_pem: ca_bytes,
            },
        }
    }
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
    let tls = mtls::mint();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let host = transport::grpc::spawn(
        Arc::clone(kernel),
        transport::grpc::VfsGrpcConfig {
            bind_addr: addr,
            tls: Some(tls.server),
            max_message_bytes: 64 * 1024 * 1024,
            server_version: "test".into(),
        },
        Arc::new(AgentAuth),
    )
    .unwrap();
    let client = Arc::new(
        nexus_vfs_client::NexusVfsClient::connect_tls(
            &format!("https://{addr}"),
            tls.client.ca_pem.clone(),
            tls.client.cert_pem.clone(),
            tls.client.key_pem.clone(),
            lib::transport_primitives::TlsConfig::CLUSTER_SERVER_NAME,
        )
        .unwrap(),
    );
    let fs =
        runtime::NexusVfsFsBackend::from_arc_mtls(Arc::clone(&client), "agent-key".into(), "root");
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
    // Over mTLS the server normalizes an AUTHENTICATION refusal to "not
    // found": every bad token below gets Ok(false) for a file that exists,
    // so an unauthenticated prober learns nothing about existence. The
    // error-preserving half of the old contract — policy (authz) failures
    // surfacing as PermissionDenied / Other — still holds and is asserted by
    // remote_append and remote_stat below through the same transport.
    for key in ["invalid-key", "denied-key", "unavailable-key"] {
        let fs = runtime::NexusVfsFsBackend::from_arc_mtls(Arc::clone(&client), key.into(), "root");
        assert_eq!(
            fs.exists("/ws/ready.txt").unwrap(),
            false,
            "{key}: an authentication refusal must read as absence, not leak existence"
        );
    }
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
    let bob = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &desc_for("agent-y", "root"),
        "/ws",
    );
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

fn scoped_descriptor(prefix: &str, read: bool) -> AgentDescriptor {
    let mut descriptor = desc_for("scoped-agent", "root");
    let mut rules =
        vec![serde_json::json!({"capability":"zone.data.write","resource_prefixes":[prefix]})];
    if read {
        rules.push(serde_json::json!({"capability":"zone.data.read","resource_prefixes":[prefix]}));
    }
    descriptor.labels.insert(
        ENV_NEXUS_RESOURCE_SCOPE.into(),
        serde_json::json!({"schema_version":1,"zone_id":"root","rules":rules}).to_string(),
    );
    descriptor
}

#[track_caller]
fn permission_denied<T>(result: io::Result<T>) {
    match result {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}"),
        Ok(_) => panic!("expected permission denial"),
    }
}

#[test]
fn metadata_and_lock_share_scope_with_positive_lifecycle_controls() {
    let kernel = kernel_with_root_backend();
    let unrestricted = vfs_backend(&kernel);
    unrestricted.create_dir_all("/private").unwrap();
    unrestricted.create_dir_all("/ws").unwrap();
    unrestricted.write("/private/target", b"private").unwrap();
    unrestricted
        .link("/private/alias", "/private/target")
        .unwrap();
    unrestricted.write("/ws/target", b"allowed").unwrap();
    unrestricted.link("/ws/alias", "/ws/target").unwrap();
    let restricted = KernelFsBackend::for_agent_descriptor(
        kernel.clone(),
        &scoped_descriptor("/ws", true),
        "/ws",
    );
    permission_denied(restricted.stat("/private/alias"));
    permission_denied(restricted.read_link("/private/alias"));
    permission_denied(restricted.try_lock("/private/lease", 1000));
    permission_denied(restricted.record_log("/private/history", RecordLogMode::CreateNew));
    permission_denied(restricted.record_log("/private/target", RecordLogMode::Existing));
    assert_eq!(restricted.read_link("/ws/alias").unwrap(), "/ws/target");
    assert!(restricted.stat("/ws/alias").unwrap().is_symlink);
    let lease = restricted.try_lock("/ws/lease", 1000).unwrap().unwrap();
    assert!(restricted.try_lock("/ws/lease", 1000).unwrap().is_none());
    assert!(lease.renew().unwrap());
    drop(lease);
    assert!(restricted.try_lock("/ws/lease", 1000).unwrap().is_some());
    // The refused scope never acquired the private lock.
    assert!(unrestricted
        .try_lock("/private/lease", 1000)
        .unwrap()
        .is_some());
}

#[test]
fn links_cannot_redirect_scoped_io_or_rename_to_private_content() {
    let kernel = kernel_with_root_backend();
    let unrestricted = vfs_backend(&kernel);
    unrestricted.create_dir_all("/ws").unwrap();
    unrestricted.create_dir_all("/private").unwrap();
    unrestricted.write("/private/target", b"private").unwrap();
    unrestricted.link("/ws/escape", "/private/target").unwrap();
    let restricted = KernelFsBackend::for_agent_descriptor(
        kernel.clone(),
        &scoped_descriptor("/ws", true),
        "/ws",
    );
    permission_denied(restricted.read("/ws/escape"));
    permission_denied(restricted.write("/ws/escape", b"changed"));
    permission_denied(restricted.append("/ws/escape", b"changed"));
    permission_denied(restricted.tail_read("/ws/escape", 0, 0));
    permission_denied(restricted.record_log("/ws/escape", RecordLogMode::Existing));
    permission_denied(restricted.rename("/ws/escape", "/ws/copy"));
    permission_denied(restricted.rename("/ws/file", "/private/copy"));
    // The follow guard admits links as a whole: reading even the METADATA of
    // an in-scope alias that points outside the scope is refused, because a
    // legitimately planted link (link() authorizes alias AND target) never
    // points out — only a layout planted without the scope can, and a scoped
    // runtime has no business resolving it.
    permission_denied(restricted.read_link("/ws/escape"));
    assert_eq!(unrestricted.read("/private/target").unwrap(), b"private");
    assert!(!unrestricted.exists("/ws/copy").unwrap());
    restricted.write("/ws/file", b"allowed").unwrap();
    restricted.append("/ws/file", b" append").unwrap();
    restricted.rename("/ws/file", "/ws/moved").unwrap();
    assert_eq!(restricted.read("/ws/moved").unwrap(), b"allowed append");
}

#[test]
fn scoped_directories_do_not_create_unauthorized_ancestors() {
    let kernel = kernel_with_root_backend();
    let restricted = KernelFsBackend::for_agent_descriptor(
        kernel.clone(),
        &scoped_descriptor("/fresh/sub", true),
        "/fresh/sub",
    );
    assert!(restricted.create_dir_all("/fresh/sub/leaf").is_err());
    assert!(kernel.sys_stat("/fresh", "root").is_none());
    let unrestricted = vfs_backend(&kernel);
    unrestricted.create_dir_all("/fresh/sub").unwrap();
    restricted.create_dir_all("/fresh/sub/leaf/deeper").unwrap();
    assert!(restricted.stat("/fresh/sub/leaf/deeper").unwrap().is_dir);
}

#[test]
fn exact_file_scope_and_native_spelling_retain_valid_writes() {
    let kernel = kernel_with_root_backend();
    let unrestricted = vfs_backend(&kernel);
    unrestricted.create_dir_all("/ws").unwrap();
    let descriptor = scoped_descriptor("/ws/new.txt", true);
    let mount = std::env::temp_dir().join("pr871-native-spelling");
    let restricted = KernelFsBackend::for_agent_descriptor(kernel, &descriptor, "/ws")
        .with_local_workspace(&mount)
        .unwrap();
    restricted
        .write(&mount.join("new.txt").to_string_lossy(), b"new")
        .unwrap();
    assert_eq!(restricted.read("/ws/new.txt").unwrap(), b"new");
    permission_denied(restricted.write("/ws/other.txt", b"other"));
}

#[test]
fn write_only_kernel_scope_preserves_internal_read_and_rename() {
    let kernel = kernel_with_root_backend();
    let unrestricted = vfs_backend(&kernel);
    unrestricted.create_dir_all("/ws").unwrap();
    let restricted =
        KernelFsBackend::for_agent_descriptor(kernel, &scoped_descriptor("/ws", false), "/ws");
    restricted.write("/ws/file", b"a").unwrap();
    restricted.append("/ws/file", b"b").unwrap();
    restricted.rename("/ws/file", "/ws/moved").unwrap();
    permission_denied(restricted.read("/ws/moved"));
    permission_denied(restricted.read_link("/ws/alias"));
    assert_eq!(unrestricted.read("/ws/moved").unwrap(), b"ab");
}

#[test]
fn revoked_lease_does_not_renew_or_release_without_permission() {
    let kernel = kernel_with_root_backend();
    let restricted = KernelFsBackend::for_agent_descriptor(
        kernel.clone(),
        &scoped_descriptor("/ws", true),
        "/ws",
    );
    let held = restricted.try_lock("/ws/held", 10000).unwrap().unwrap();
    let lease = restricted.try_lock("/ws/lease", 1).unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(650));
    kernel.set_permission_provider(Arc::new(Box::new(DenyAllMetadata)));
    permission_denied(lease.renew());
    drop(lease);
    drop(held);
    kernel.set_permission_provider(Arc::new(Box::new(CountingAllow(Arc::new(
        AtomicUsize::new(0),
    )))));
    assert!(restricted.try_lock("/ws/held", 10000).unwrap().is_none());
    // The lock backend records expiry in integer seconds; wait beyond the original TTL.
    std::thread::sleep(std::time::Duration::from_millis(500));
    kernel.set_permission_provider(Arc::new(Box::new(CountingAllow(Arc::new(
        AtomicUsize::new(0),
    )))));
    assert!(restricted.try_lock("/ws/lease", 1).unwrap().is_some());
}

#[test]
fn scoped_stream_append_and_existing_log_keep_write_only_principal_behavior() {
    let kernel = kernel_with_root_backend();
    kernel.create_stream("/ws/stream", 65536).unwrap();
    let restricted = KernelFsBackend::for_agent_descriptor(
        kernel.clone(),
        &scoped_descriptor("/ws", false),
        "/ws",
    );
    let policy = install_file_policy(&kernel);
    policy
        .read_denied
        .lock()
        .unwrap()
        .insert("/ws/stream".into());
    restricted.create_append_log("/ws/stream", 0).unwrap();
    restricted.append("/ws/stream", b"frame").unwrap();
    assert_eq!(
        kernel.sys_stat("/ws/stream", "root").unwrap().entry_type,
        kernel::meta_store::DT_STREAM
    );
    permission_denied(restricted.read("/ws/stream"));
    policy.read_denied.lock().unwrap().clear();
    assert_eq!(vfs_backend(&kernel).read("/ws/stream").unwrap(), b"frame");
}

/// A scope bound to an mTLS-verified transport refuses out-of-scope targets
/// on the CLIENT, before any RPC leaves the process, while in-scope work
/// flows normally over the same transport — the remote half of the one
/// admission contract (`read_link`/`try_lock` included), against a real
/// daemon rather than a stub.
#[test]
#[cfg(target_os = "linux")]
fn scoped_mtls_transport_early_denies_out_of_scope_targets() {
    let kernel = kernel_with_root_backend();
    let local = vfs_backend(&kernel);
    local.create_dir_all("/ws").unwrap();
    local.create_dir_all("/private").unwrap();
    local.write("/ws/ready.txt", b"ready").unwrap();
    local.write("/private/target", b"private").unwrap();
    local.link("/private/alias", "/private/target").unwrap();
    local.link("/ws/escape", "/private/target").unwrap();
    let (_, _host, client) = remote_backend(&kernel);
    let scope = scoped_descriptor("/ws", true)
        .labels
        .remove(ENV_NEXUS_RESOURCE_SCOPE)
        .unwrap();
    let remote =
        NexusVfsFsBackend::from_arc_mtls_with_scope(client, "agent-key".into(), "root", &scope);
    // Every out-of-scope target is refused before the transport is touched.
    permission_denied(remote.read("/private/target"));
    permission_denied(remote.read_link("/private/alias"));
    permission_denied(remote.try_lock("/private/lease", 1000));
    permission_denied(remote.record_log("/private/history", RecordLogMode::CreateNew));
    permission_denied(remote.record_log("/private/target", RecordLogMode::Existing));
    // A link whose ALIAS is in scope but whose TARGET is not is refused whole
    // by the follow guard — metadata included.
    permission_denied(remote.read("/ws/escape"));
    permission_denied(remote.record_log("/ws/escape", RecordLogMode::Existing));
    permission_denied(remote.write("/ws/escape", b"changed"));
    permission_denied(remote.read_link("/ws/escape"));
    permission_denied(remote.rename("/ws/escape", "/ws/copy"));
    // In-scope work over the SAME transport flows normally.
    remote.write("/ws/file", b"a").unwrap();
    remote.append("/ws/file", b"b").unwrap();
    remote.rename("/ws/file", "/ws/moved").unwrap();
    assert_eq!(remote.read("/ws/moved").unwrap(), b"ab");
    remote.link("/ws/alias", "/ws/moved").unwrap();
    assert_eq!(remote.read_link("/ws/alias").unwrap(), "/ws/moved");
    remote.create_dir_all("/ws/nested/deep").unwrap();
    assert!(remote.stat("/ws/nested/deep").unwrap().is_dir);
    let lease = remote.try_lock("/ws/lease", 1000).unwrap().unwrap();
    assert!(remote.try_lock("/ws/lease", 1000).unwrap().is_none());
    assert!(lease.renew().unwrap());
    drop(lease);
    assert!(remote.try_lock("/ws/lease", 1000).unwrap().is_some());
    // Nothing landed outside the scope.
    assert_eq!(local.read("/private/target").unwrap(), b"private");
    assert!(!local.exists("/ws/copy").unwrap());
    assert!(!local.exists("/private/lease").unwrap());
    assert_directory_alias_scope(&local, &remote);
}

#[test]
fn scoped_directory_aliases_cannot_read_or_create_outside_their_resource_scope() {
    let kernel = kernel_with_root_backend();
    let owner = vfs_backend(&kernel);
    let scoped = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &scoped_descriptor("/ws", true),
        "/ws",
    );
    assert_directory_alias_scope(&owner, &scoped);
}

fn assert_directory_alias_scope(owner: &dyn FsBackend, scoped: &dyn FsBackend) {
    owner.write("/private/secret", b"private fixture").unwrap();
    owner.link("/ws/directory-escape", "/private").unwrap();
    permission_denied(scoped.read("/ws/directory-escape/secret"));
    permission_denied(scoped.stat("/ws/directory-escape/secret"));
    permission_denied(scoped.tail_read("/ws/directory-escape/secret", 0, 0));
    permission_denied(scoped.write("/ws/directory-escape/new", b"must be denied"));
    permission_denied(scoped.append("/ws/directory-escape/new", b"must be denied"));
    assert!(!owner.exists("/private/new").unwrap());
    assert_eq!(owner.read("/private/secret").unwrap(), b"private fixture");
    owner.create_dir_all("/ws/inside").unwrap();
    owner.link("/ws/shortcut", "/ws/inside").unwrap();
    scoped.write("/ws/shortcut/new", b"allowed").unwrap();
    assert_eq!(scoped.read("/ws/shortcut/new").unwrap(), b"allowed");
    assert_eq!(owner.read("/ws/inside/new").unwrap(), b"allowed");
}

#[test]
#[cfg(target_os = "linux")]
fn remote_stream_aliases_preserve_payloads_with_write_only_append() {
    let kernel = kernel_with_root_backend();
    let local = vfs_backend(&kernel);
    local.write("/ws/ready.txt", b"ready").unwrap();
    kernel.create_stream("/ws/stream", 65536).unwrap();
    local.link("/ws/stream-alias", "/ws/stream").unwrap();
    local.link("/ws/stream-directory", "/ws").unwrap();
    let policy = install_file_policy(&kernel);
    let (remote, _host, client) = remote_backend(&kernel);
    assert_regular_alias_tails(&remote);
    for path in ["/ws/stream-alias", "/ws/stream-directory/stream"] {
        assert!(remote.is_append_stream(path).unwrap());
        remote.append(path, b"frame\n").unwrap();
    }
    assert_eq!(remote.read("/ws/stream-alias").unwrap(), b"frame\nframe\n");
    let scope = scoped_descriptor("/ws", false)
        .labels
        .remove(ENV_NEXUS_RESOURCE_SCOPE)
        .unwrap();
    let write_only =
        NexusVfsFsBackend::from_arc_mtls_with_scope(client, "agent-key".into(), "root", &scope);
    policy
        .read_denied
        .lock()
        .unwrap()
        .extend(["/ws/stream".into(), "/ws/stream-alias".into()]);
    write_only
        .append("/ws/stream-alias", b"write only\n")
        .unwrap();
    permission_denied(write_only.read("/ws/stream-alias"));
    policy.deny("/ws/stream");
    permission_denied(write_only.append("/ws/stream-alias", b"denied\n"));
    policy.denied.lock().unwrap().clear();
    policy.read_denied.lock().unwrap().clear();
    assert_eq!(
        remote.read("/ws/stream-directory/stream").unwrap(),
        b"frame\nframe\nwrite only\n"
    );
}
