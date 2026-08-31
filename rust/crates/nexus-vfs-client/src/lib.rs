pub mod proto {
    tonic::include_proto!("nexus.grpc.vfs");
}

use proto::nexus_vfs_service_client::NexusVfsServiceClient;
use proto::{
    CallRequest, DeleteRequest, ReadRequest, SetattrRequest, StreamReadAtRequest,
    StreamWriteRequest, WriteRequest,
};
use std::io;
use std::sync::mpsc;

/// DT_STREAM entry-type code (mirrors the kernel `entry_type`), passed to
/// `Setattr` when provisioning a mailbox DT_STREAM.
const DT_STREAM: i32 = 4;

enum VfsOp {
    Read {
        path: String,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<Vec<u8>>>,
    },
    Write {
        path: String,
        content: Vec<u8>,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<()>>,
    },
    Delete {
        path: String,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<()>>,
    },
    /// Generic Call RPC — method name + JSON payload.
    Call {
        method: String,
        payload: Vec<u8>,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<Vec<u8>>>,
    },
    /// Append one frame to a DT_STREAM; returns the offset it landed at.
    StreamWrite {
        path: String,
        data: Vec<u8>,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<u64>>,
    },
    /// Read a DT_STREAM at `offset`; returns `(data, next_offset, eof)`.
    /// When `blocking`, the server parks up to `timeout_ms` waiting for the
    /// next frame (returning `eof` on timeout) — the event-driven mailbox tail.
    StreamReadAt {
        path: String,
        offset: u64,
        blocking: bool,
        timeout_ms: u64,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<(Vec<u8>, u64, bool)>>,
    },
    /// `sys_setattr(DT_STREAM)` — create (or no-op if present) a DT_STREAM
    /// container at `path`. Returns whether it was freshly created.
    EnsureStream {
        path: String,
        io_profile: String,
        capacity: u64,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<bool>>,
    },
}

/// Sync wrapper around the nexus VFS gRPC client.
///
/// Maintains a background tokio thread; all public methods are blocking
/// and can be called from any synchronous context, including outside of
/// an async runtime.
pub struct NexusVfsClient {
    // Unbounded so the sync public methods can enqueue an op from ANY context
    // — including from within a tokio runtime (scode's async tool executor).
    // `UnboundedSender::send` is synchronous and never blocks the caller, so it
    // cannot trigger tokio's "block the current thread from within a runtime"
    // panic the way the bounded channel's `blocking_send` did. The caller then
    // blocks on the per-op std channel until the background thread replies.
    tx: tokio::sync::mpsc::UnboundedSender<VfsOp>,
}

/// mTLS material for [`NexusVfsClient::connect_tls`].
struct TlsMaterial {
    ca_pem: Vec<u8>,
    client_cert_pem: Vec<u8>,
    client_key_pem: Vec<u8>,
    server_name: String,
}

impl NexusVfsClient {
    /// Connect to a nexus VFS gRPC server at `endpoint` over PLAINTEXT.
    ///
    /// The channel is lazy — the actual TCP/UDS connection is deferred
    /// until the first RPC. Returns an error only if the background
    /// thread cannot be spawned or the endpoint URI is invalid.
    pub fn connect(endpoint: &str) -> io::Result<Self> {
        Self::connect_inner(endpoint, None)
    }

    /// Connect over mTLS: pin `ca_pem`, present the client cert
    /// (`client_cert_pem` + `client_key_pem`), and validate the server
    /// against `server_name` (the cluster's fixed SAN, e.g. `nexus-node`).
    /// Required to reach an auth-on `nexusd-cluster` (which serves MUTUAL
    /// TLS — a plaintext client is rejected). Caller identity still rides
    /// the per-request `auth_token`, not the client cert.
    pub fn connect_tls(
        endpoint: &str,
        ca_pem: Vec<u8>,
        client_cert_pem: Vec<u8>,
        client_key_pem: Vec<u8>,
        server_name: &str,
    ) -> io::Result<Self> {
        Self::connect_inner(
            endpoint,
            Some(TlsMaterial {
                ca_pem,
                client_cert_pem,
                client_key_pem,
                server_name: server_name.to_owned(),
            }),
        )
    }

    fn connect_inner(endpoint: &str, tls: Option<TlsMaterial>) -> io::Result<Self> {
        // tonic's `Channel::from_shared` requires a URI scheme; accept a bare
        // `host:port` for ergonomics and supply the scheme the transport
        // implies (https under mTLS, http otherwise).
        let endpoint = if endpoint.contains("://") {
            endpoint.to_owned()
        } else if tls.is_some() {
            format!("https://{endpoint}")
        } else {
            format!("http://{endpoint}")
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<VfsOp>();

        std::thread::Builder::new()
            .name("nexus-vfs-client".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("nexus-vfs tokio runtime");
                rt.block_on(async move {
                    let builder = tonic::transport::Channel::from_shared(endpoint)
                        .expect("invalid vfs endpoint URI");
                    let ch = match tls {
                        None => builder.connect_lazy(),
                        Some(t) => {
                            let cfg = tonic::transport::ClientTlsConfig::new()
                                .ca_certificate(tonic::transport::Certificate::from_pem(t.ca_pem))
                                .identity(tonic::transport::Identity::from_pem(
                                    t.client_cert_pem,
                                    t.client_key_pem,
                                ))
                                .domain_name(t.server_name);
                            builder
                                .tls_config(cfg)
                                .expect("vfs client TLS config")
                                .connect_lazy()
                        }
                    };
                    let client = NexusVfsServiceClient::new(ch);
                    while let Some(op) = rx.recv().await {
                        // Each op runs on its own task: a long op (a blocking
                        // stream-tail read) must not block the others on this
                        // connection. The tonic client clones cheaply and
                        // multiplexes concurrent requests over the one HTTP/2
                        // channel, so a receiver parked on the tail can't
                        // starve the send half. Single-caller ordering is
                        // unchanged — every sync method blocks on its reply
                        // channel, so a caller can't issue its next op until
                        // this one returns; only cross-thread use concurs.
                        let mut client = client.clone();
                        tokio::spawn(async move {
                            match op {
                                VfsOp::Read {
                                    path,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .read(ReadRequest {
                                            path,
                                            auth_token,
                                            content_id: String::new(),
                                            timeout_ms: 0,
                                            offset: 0,
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(r.content)
                                        }
                                    }));
                                }
                                VfsOp::Write {
                                    path,
                                    content,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .write(WriteRequest {
                                            path,
                                            content,
                                            auth_token,
                                            content_id: String::new(),
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(())
                                        }
                                    }));
                                }
                                VfsOp::Delete {
                                    path,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .delete(DeleteRequest {
                                            path,
                                            auth_token,
                                            recursive: false,
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(())
                                        }
                                    }));
                                }
                                VfsOp::Call {
                                    method,
                                    payload,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .call(CallRequest {
                                            method,
                                            payload,
                                            auth_token,
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.payload))
                                        } else {
                                            Ok(r.payload)
                                        }
                                    }));
                                }
                                VfsOp::StreamWrite {
                                    path,
                                    data,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .stream_write_nowait(StreamWriteRequest {
                                            path,
                                            data,
                                            auth_token,
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(r.offset)
                                        }
                                    }));
                                }
                                VfsOp::StreamReadAt {
                                    path,
                                    offset,
                                    blocking,
                                    timeout_ms,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .stream_read_at(StreamReadAtRequest {
                                            path,
                                            offset,
                                            blocking,
                                            timeout_ms,
                                            auth_token,
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok((r.data, r.next_offset, r.eof))
                                        }
                                    }));
                                }
                                VfsOp::EnsureStream {
                                    path,
                                    io_profile,
                                    capacity,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .setattr(SetattrRequest {
                                            path,
                                            auth_token,
                                            entry_type: DT_STREAM,
                                            io_profile,
                                            capacity,
                                            ..Default::default()
                                        })
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(r.created)
                                        }
                                    }));
                                }
                            }
                        });
                    }
                });
            })
            .map_err(io::Error::other)?;

        Ok(Self { tx })
    }

    pub fn read(&self, path: &str, auth_token: &str) -> io::Result<Vec<u8>> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::Read {
                path: path.to_owned(),
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    pub fn write(&self, path: &str, content: Vec<u8>, auth_token: &str) -> io::Result<()> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::Write {
                path: path.to_owned(),
                content,
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    pub fn delete(&self, path: &str, auth_token: &str) -> io::Result<()> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::Delete {
                path: path.to_owned(),
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    /// Append one frame to a DT_STREAM at `path`; returns the byte offset
    /// the frame landed at. This is the A2A mailbox SEND path — one message
    /// is one framed append to `/agents/<recipient>/chat-with-me` (the node
    /// stamps an unforgeable `from` under auth-on).
    pub fn stream_write(&self, path: &str, data: Vec<u8>, auth_token: &str) -> io::Result<u64> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::StreamWrite {
                path: path.to_owned(),
                data,
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    /// Non-blocking read of a DT_STREAM at `offset`. Returns
    /// `(data, next_offset, eof)` — `eof == true` means no frame was
    /// available at `offset` yet. This is the A2A inbox POLL path (advance
    /// the caller's cursor to `next_offset` after each delivered frame).
    /// Read one DT_STREAM frame at `offset`, returning `(data, next_offset,
    /// eof)`. When `blocking`, the server parks up to `timeout_ms` for the next
    /// frame and returns `eof=true` (empty) on timeout — the event-driven
    /// mailbox tail (`read_at_blocking`), woken sub-millisecond by any write to
    /// `path` (node-local inline or a replicated peer write). Pass
    /// `blocking=false, timeout_ms=0` for a plain non-blocking drain.
    pub fn stream_read_at(
        &self,
        path: &str,
        offset: u64,
        blocking: bool,
        timeout_ms: u64,
        auth_token: &str,
    ) -> io::Result<(Vec<u8>, u64, bool)> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::StreamReadAt {
                path: path.to_owned(),
                offset,
                blocking,
                timeout_ms,
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    /// `sys_setattr(DT_STREAM)` on `path` — create the DT_STREAM container
    /// with the given `io_profile` (backend waterfall) and `capacity`
    /// (cold-storage retention budget). Idempotent: an existing stream is a
    /// no-op. Returns whether the stream was freshly created. This is how a
    /// standalone A2A participant provisions its own inbox, the gRPC analog
    /// of the co-host's in-process `a2a::ensure_mailbox_stream`.
    pub fn ensure_stream(
        &self,
        path: &str,
        io_profile: &str,
        capacity: u64,
        auth_token: &str,
    ) -> io::Result<bool> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::EnsureStream {
                path: path.to_owned(),
                io_profile: io_profile.to_owned(),
                capacity,
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    /// Generic Call RPC — sends `method` + JSON `payload` through the
    /// nexus VFS `Call` endpoint. Returns the response payload bytes.
    pub fn call(&self, method: &str, payload: &[u8], auth_token: &str) -> io::Result<Vec<u8>> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::Call {
                method: method.to_owned(),
                payload: payload.to_vec(),
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        resp_rx.recv().map_err(|_| broken_pipe())?
    }

    /// Stat a path via the generic Call RPC.
    ///
    /// Returns `(size, is_directory)` on success.
    pub fn stat(&self, path: &str, auth_token: &str) -> io::Result<VfsStat> {
        let payload = serde_json::json!({ "path": path });
        let resp = self.call("stat", payload.to_string().as_bytes(), auth_token)?;
        let value: serde_json::Value = serde_json::from_slice(&resp)
            .map_err(|e| io::Error::other(format!("stat response parse: {e}")))?;
        Ok(VfsStat {
            size: value["size"].as_u64().unwrap_or(0),
            is_directory: value["is_directory"].as_bool().unwrap_or(false),
            modified_at_ms: value["modified_at_ms"].as_i64(),
        })
    }

    /// List directory entries via the generic Call RPC.
    pub fn readdir(&self, path: &str, auth_token: &str) -> io::Result<Vec<VfsDirEntry>> {
        let payload = serde_json::json!({ "path": path });
        let resp = self.call("readdir", payload.to_string().as_bytes(), auth_token)?;
        let value: serde_json::Value = serde_json::from_slice(&resp)
            .map_err(|e| io::Error::other(format!("readdir response parse: {e}")))?;
        let entries = value
            .as_array()
            .ok_or_else(|| io::Error::other("readdir: expected array"))?;
        Ok(entries
            .iter()
            .filter_map(|entry| {
                Some(VfsDirEntry {
                    name: entry["name"].as_str()?.to_string(),
                    is_directory: entry["is_directory"].as_bool().unwrap_or(false),
                })
            })
            .collect())
    }
}

/// Stat result returned by [`NexusVfsClient::stat`].
#[derive(Debug, Clone)]
pub struct VfsStat {
    pub size: u64,
    pub is_directory: bool,
    pub modified_at_ms: Option<i64>,
}

/// Directory entry returned by [`NexusVfsClient::readdir`].
#[derive(Debug, Clone)]
pub struct VfsDirEntry {
    pub name: String,
    pub is_directory: bool,
}

fn grpc_result<T, R, F>(result: Result<tonic::Response<T>, tonic::Status>, f: F) -> io::Result<R>
where
    F: FnOnce(T) -> io::Result<R>,
{
    match result {
        Ok(resp) => f(resp.into_inner()),
        Err(status) => Err(io::Error::other(status.to_string())),
    }
}

fn vfs_err(payload: &[u8]) -> io::Error {
    io::Error::other(String::from_utf8_lossy(payload).into_owned())
}

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "vfs worker gone")
}
