//! Integration test: NexusVfsClient <-> kernel gRPC server roundtrip.
//!
//! Verifies that the nexus-vfs-client proto (vendored in sudocode) is
//! wire-compatible with the kernel's gRPC transport at runtime, not
//! just at compile time. This catches proto drift between the client
//! proto and the server implementation in nexus-vfs.
//!
//! Architecture:
//!   [NexusVfsClient] --gRPC--> [transport::grpc server] --> [Kernel]
//!   (sudocode crate)           (nexus-vfs crate)           (nexus-vfs)
//!
//! All three layers run in-process; the gRPC server binds localhost on
//! a random free port. No external binary or network dependency.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use kernel::abc::object_store::{ObjectStore, StorageError, WriteResult};
use kernel::abi::KernelAbi;
use kernel::kernel::convenience::{KernelConvenience, MountOptions};
use kernel::kernel::{Kernel, OperationContext};
use transport::auth::NoAuth;
use transport::grpc::{spawn, VfsGrpcConfig};

// ── In-memory ObjectStore backend (mirrors transport/src/grpc.rs tests) ──

#[derive(Default)]
struct MemBackend {
    blobs: StdMutex<HashMap<String, Vec<u8>>>,
}

impl ObjectStore for MemBackend {
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
        let mut map = self.blobs.lock().unwrap();
        let entry = map.entry(content_id.to_string()).or_default();
        let start = offset as usize;
        if start > entry.len() {
            entry.resize(start, 0);
        }
        let end = start + content.len();
        if end > entry.len() {
            entry.resize(end, 0);
        }
        entry[start..end].copy_from_slice(content);
        Ok(WriteResult {
            content_id: content_id.to_string(),
            version: content_id.to_string(),
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
            .unwrap()
            .get(content_id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(content_id.into()))
    }
}

// ── Test helpers ─────────────────────────────────────────────────────

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn kernel_with_mem_backend() -> Kernel {
    let k = Kernel::new();
    let backend: Arc<dyn ObjectStore> = Arc::new(MemBackend::default());
    k.mount(
        "/",
        MountOptions::new("mem")
            .with_backend(backend)
            .with_io_profile(""),
    )
    .expect("mount / with mem backend");
    k
}

fn admin_ctx() -> OperationContext {
    OperationContext::new("test", "root", true, None, true)
}

struct TestServer {
    port: u16,
    _handle: transport::grpc::VfsGrpcHandle,
}

impl TestServer {
    fn start(kernel: Arc<Kernel>) -> Self {
        let port = free_port();
        let handle = spawn(
            kernel,
            VfsGrpcConfig {
                bind_addr: ([127, 0, 0, 1], port).into(),
                tls: None,
                max_message_bytes: 4 * 1024 * 1024,
                server_version: "grpc-roundtrip-test".to_string(),
            },
            Arc::new(NoAuth),
        )
        .expect("spawn gRPC server");

        // Allow the tonic server to bind and start accepting connections.
        std::thread::sleep(std::time::Duration::from_millis(200));

        Self {
            port,
            _handle: handle,
        }
    }

    fn connect_client(&self) -> nexus_vfs_client::NexusVfsClient {
        nexus_vfs_client::NexusVfsClient::connect(&format!("http://127.0.0.1:{}", self.port))
            .expect("NexusVfsClient::connect")
    }
}

// ── Tests ────────────────────────────────────────────────────────────

/// Write via gRPC client, read back via gRPC client AND via in-process
/// kernel — verifies the full round-trip through the proto wire format.
#[test]
fn write_then_read_roundtrip() {
    let kernel = Arc::new(kernel_with_mem_backend());
    let server = TestServer::start(Arc::clone(&kernel));
    let client = server.connect_client();

    // Write through gRPC
    client
        .write("/hello.txt", b"hello gRPC".to_vec(), "test-token")
        .expect("gRPC write");

    // Read back through gRPC
    let data = client.read("/hello.txt", "test-token").expect("gRPC read");
    assert_eq!(
        data, b"hello gRPC",
        "gRPC read should return written content"
    );

    // Cross-verify: read through kernel directly
    let ctx = admin_ctx();
    let result = KernelAbi::sys_read(&*kernel, "/hello.txt", &ctx, 0, 0).expect("kernel read");
    assert_eq!(
        result.data.unwrap_or_default(),
        b"hello gRPC",
        "kernel read should match gRPC-written content"
    );
}

/// Write via kernel, read via gRPC client — verifies the server
/// correctly relays kernel data through the proto wire format.
#[test]
fn kernel_write_then_grpc_read() {
    let kernel = Arc::new(kernel_with_mem_backend());
    let server = TestServer::start(Arc::clone(&kernel));
    let client = server.connect_client();

    let ctx = admin_ctx();
    KernelAbi::sys_write(&*kernel, "/from-kernel.txt", &ctx, b"kernel data", 0)
        .expect("kernel write");

    let data = client
        .read("/from-kernel.txt", "test-token")
        .expect("gRPC read of kernel-written file");
    assert_eq!(data, b"kernel data");
}

/// Delete via gRPC client — verifies the Delete RPC wire format works.
/// Note: the in-memory MemBackend retains blob data after metastore
/// unlink, so we only assert the delete call itself succeeds (no
/// transport error). Full delete semantics are tested in kernel unit tests.
#[test]
fn delete_does_not_error() {
    let kernel = Arc::new(kernel_with_mem_backend());
    let server = TestServer::start(Arc::clone(&kernel));
    let client = server.connect_client();

    client
        .write("/ephemeral.txt", b"delete me".to_vec(), "test-token")
        .expect("gRPC write");

    // The Delete RPC should succeed without transport error.
    client
        .delete("/ephemeral.txt", "test-token")
        .expect("gRPC delete should not error");
}

/// Read a file that does not exist — verifies the error path through
/// the proto wire format.
#[test]
fn read_nonexistent_returns_error() {
    let kernel = Arc::new(kernel_with_mem_backend());
    let server = TestServer::start(Arc::clone(&kernel));
    let client = server.connect_client();

    let result = client.read("/does-not-exist.txt", "test-token");
    assert!(result.is_err(), "read of nonexistent file should error");
}

/// Multiple sequential writes to the same path — verifies last-write-wins.
#[test]
fn overwrite_roundtrip() {
    let kernel = Arc::new(kernel_with_mem_backend());
    let server = TestServer::start(Arc::clone(&kernel));
    let client = server.connect_client();

    client
        .write("/overwrite.txt", b"v1".to_vec(), "test-token")
        .expect("write v1");
    client
        .write("/overwrite.txt", b"v2-longer".to_vec(), "test-token")
        .expect("write v2");

    let data = client
        .read("/overwrite.txt", "test-token")
        .expect("read after overwrite");
    assert_eq!(data, b"v2-longer");
}

/// Write + read a non-trivial payload (64 KiB) to exercise chunking.
#[test]
fn large_payload_roundtrip() {
    let kernel = Arc::new(kernel_with_mem_backend());
    let server = TestServer::start(Arc::clone(&kernel));
    let client = server.connect_client();

    let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
    client
        .write("/large.bin", payload.clone(), "test-token")
        .expect("write 64 KiB");

    let data = client
        .read("/large.bin", "test-token")
        .expect("read 64 KiB");
    assert_eq!(data.len(), 64 * 1024);
    assert_eq!(data, payload);
}
