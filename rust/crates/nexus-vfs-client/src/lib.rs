pub mod proto {
    tonic::include_proto!("nexus.grpc.vfs");
}

use proto::nexus_vfs_service_client::NexusVfsServiceClient;
use proto::{
    CallRequest, DeleteRequest, ReadRequest, ReaddirRequest, SetattrRequest, StatRequest,
    StreamReadAtRequest, StreamWriteRequest, WriteRequest,
};
use std::io;
use std::sync::mpsc;
use std::time::Duration;

/// DT_STREAM entry-type code (mirrors the kernel `entry_type`), passed to
/// `Setattr` when provisioning a mailbox DT_STREAM.
const DT_STREAM: i32 = 4;

/// DT_DIR entry-type code, for reading `Readdir` results back.
const DT_DIR: u32 = 1;

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
    /// `Stat` — the TYPED RPC, not the generic Call surface.
    Stat {
        path: String,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<VfsStat>>,
    },
    /// `Readdir` — the TYPED RPC, not the generic Call surface.
    Readdir {
        path: String,
        auth_token: String,
        resp: mpsc::SyncSender<io::Result<Vec<VfsDirEntry>>>,
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
                    // `connect_timeout` before the posture split, so it covers
                    // plaintext and mTLS alike. Without it a lazily-connected
                    // channel to an address that blackholes packets never fails
                    // at connect — it yields a request future that never
                    // resolves, and the symptom lands on whichever op happened
                    // to be first rather than on the dial.
                    let builder = tonic::transport::Channel::from_shared(endpoint)
                        .expect("invalid vfs endpoint URI")
                        .connect_timeout(CONNECT_DEADLINE);
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
                        // Every arm below ends in exactly one `resp.send`, and
                        // that is the invariant callers depend on: `await_reply`
                        // turns a missing reply into an error, so an arm that
                        // returned without sending would surface as a timeout
                        // rather than as the failure it was. It holds by
                        // inspection today — no `?`, no early `return`, no
                        // panic path (`grpc_result` is a total match over
                        // `Result`) — and an eighth arm must keep it. A guard
                        // object was considered and rejected: `resp` is bound
                        // inside each variant's pattern, so enforcing it
                        // structurally means reshaping `VfsOp` to hoist the
                        // sender out, which is a bigger change to the op type
                        // than the unreachable branch it would protect.
                        // `let _ =` on each send is deliberate: a receiver that
                        // hung up is legitimate, and the caller already sees
                        // that as `Disconnected` -> `BrokenPipe`.
                        tokio::spawn(async move {
                            match op {
                                VfsOp::Read {
                                    path,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .read(deadlined(
                                            ReadRequest {
                                                path,
                                                auth_token,
                                                content_id: String::new(),
                                                timeout_ms: 0,
                                                offset: 0,
                                            },
                                            OP_DEADLINE,
                                        ))
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
                                        .write(deadlined(
                                            WriteRequest {
                                                path,
                                                content,
                                                auth_token,
                                            },
                                            OP_DEADLINE,
                                        ))
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
                                        .delete(deadlined(
                                            DeleteRequest {
                                                path,
                                                auth_token,
                                                recursive: false,
                                            },
                                            OP_DEADLINE,
                                        ))
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
                                        .call(deadlined(
                                            CallRequest {
                                                method,
                                                payload,
                                                auth_token,
                                            },
                                            OP_DEADLINE,
                                        ))
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
                                        .stream_write_nowait(deadlined(
                                            StreamWriteRequest {
                                                path,
                                                data,
                                                auth_token,
                                            },
                                            OP_DEADLINE,
                                        ))
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
                                    // The one op designed to wait: its deadline
                                    // is the wait it asked for plus a grace, so
                                    // the server's own `eof` at `timeout_ms` is
                                    // a normal return rather than a race.
                                    let deadline = tail_deadline(blocking, timeout_ms);
                                    let r = client
                                        .stream_read_at(deadlined(
                                            StreamReadAtRequest {
                                                path,
                                                offset,
                                                blocking,
                                                timeout_ms,
                                                auth_token,
                                            },
                                            deadline,
                                        ))
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
                                        .setattr(deadlined(
                                            SetattrRequest {
                                                path,
                                                auth_token,
                                                entry_type: DT_STREAM,
                                                io_profile,
                                                capacity,
                                                ..Default::default()
                                            },
                                            OP_DEADLINE,
                                        ))
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(r.created)
                                        }
                                    }));
                                }
                                VfsOp::Stat {
                                    path,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .stat(deadlined(
                                            StatRequest {
                                                path,
                                                auth_token,
                                                ..Default::default()
                                            },
                                            OP_DEADLINE,
                                        ))
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.found {
                                            Ok(VfsStat {
                                                size: u64::try_from(r.size).unwrap_or(0),
                                                is_directory: r.is_directory,
                                                modified_at_ms: None,
                                            })
                                        } else {
                                            Err(io::Error::new(
                                                io::ErrorKind::NotFound,
                                                format!("{}: not found", r.path),
                                            ))
                                        }
                                    }));
                                }
                                VfsOp::Readdir {
                                    path,
                                    auth_token,
                                    resp,
                                } => {
                                    let r = client
                                        .readdir(deadlined(
                                            ReaddirRequest {
                                                path,
                                                auth_token,
                                                ..Default::default()
                                            },
                                            OP_DEADLINE,
                                        ))
                                        .await;
                                    let _ = resp.send(grpc_result(r, |r| {
                                        if r.is_error {
                                            Err(vfs_err(&r.error_payload))
                                        } else {
                                            Ok(r.entries
                                                .into_iter()
                                                .map(|e| VfsDirEntry {
                                                    name: e.name,
                                                    is_directory: e.entry_type == DT_DIR,
                                                })
                                                .collect())
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
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
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
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
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
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
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
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
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
        // The one op that is *designed* to wait, so its ceiling is derived from the
        // wait it asked for rather than fixed: the server parks up to `timeout_ms`
        // and answers `eof`, and only a reply that never comes at all should trip
        // the bound.
        await_reply(
            &resp_rx,
            tail_deadline(blocking, timeout_ms) + HANDOFF_GRACE,
        )
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
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
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
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
    }

    /// Stat a path via the generic Call RPC.
    ///
    /// Returns `(size, is_directory)` on success.
    pub fn stat(&self, path: &str, auth_token: &str) -> io::Result<VfsStat> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::Stat {
                path: path.to_owned(),
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
    }

    /// List directory entries.
    ///
    /// Over the TYPED `Readdir` RPC, like every other file operation here.
    /// This and `stat` went through the generic `Call` surface, which the node
    /// answers with `unknown Call method: readdir — … call those instead`:
    /// Call carries registry and plugin dispatch, not file ops. Nothing noticed
    /// while no caller listed a directory; a receiver that finds its
    /// conversations by listing its chat list notices immediately, and what it
    /// reports is "no conversations" rather than "this call is not supported".
    pub fn readdir(&self, path: &str, auth_token: &str) -> io::Result<Vec<VfsDirEntry>> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .send(VfsOp::Readdir {
                path: path.to_owned(),
                auth_token: auth_token.to_owned(),
                resp: resp_tx,
            })
            .map_err(|_| broken_pipe())?;
        await_reply(&resp_rx, OP_DEADLINE + HANDOFF_GRACE)
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

/// Map a node error payload to an `io::Error`, preserving "not found".
///
/// Callers BRANCH on that one: a receiver listing a chat list that does not
/// exist yet has no conversations, which is not the same as a call that
/// failed. Flattened to `Other`, the two are indistinguishable and the
/// receiver reports an error where the honest answer is "none yet".
///
/// Every other code keeps its message and lands as `Other`, since nothing
/// downstream tells them apart today.
fn vfs_err(payload: &[u8]) -> io::Error {
    /// `FileNotFound` on the node's error enum (`transport/src/grpc.rs`).
    const FILE_NOT_FOUND: i64 = -32007;

    let text = String::from_utf8_lossy(payload).into_owned();
    let code = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("code").and_then(serde_json::Value::as_i64));
    if code == Some(FILE_NOT_FOUND) {
        return io::Error::new(io::ErrorKind::NotFound, text);
    }
    io::Error::other(text)
}

fn broken_pipe() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "vfs worker gone")
}

/// Deadline for an ordinary op, carried ON the request.
///
/// A deadline is a property of the request, so it is expressed once, in the
/// transport: [`tonic::Request::set_timeout`] writes the `grpc-timeout` header
/// and tonic's own client-side timeout layer enforces it (the layer is always
/// installed and reads the header even when the endpoint sets no timeout of its
/// own). One clock, and the error says the RPC exceeded its deadline rather than
/// blaming the worker that was waiting on it.
///
/// Generous on purpose: a stuck-detector, not a latency budget, so it must never
/// fire on a slow-but-working call.
const OP_DEADLINE: Duration = Duration::from_secs(60);

/// How much later than a blocking tail's own `timeout_ms` its deadline sits.
///
/// The tail has TWO deadlines by design and the application-level one must win:
/// the server parks up to `timeout_ms` and answers `eof`, which is a normal
/// return the caller acts on. The transport deadline exists only for a reply
/// that never comes at all, so it is placed after the server's — small, because
/// with one enforcing mechanism there is no second clock to out-run.
const TAIL_GRACE: Duration = Duration::from_secs(5);

/// Bound on establishing the connection.
///
/// The channel is lazy, so without this a dial to an address that blackholes
/// packets never fails — it produces a request future that never resolves, and
/// the symptom surfaces much later as "no reply" from whatever op happened to be
/// first. Naming the connect failure at the connect is what makes it
/// attributable.
const CONNECT_DEADLINE: Duration = Duration::from_secs(20);

/// How much later than the REQUEST's deadline the reply-channel backstop sits.
///
/// Two bounds cover one op and they must not be equal. The request carries the
/// primary deadline (see [`deadlined`]); [`await_reply`] is a backstop for the
/// handoff a deadline cannot reach. Setting both to the same value makes which
/// one fires a coin flip — and the backstop won in practice, so a daemon that
/// missed its deadline was reported as `vfs worker sent no reply`, blaming the
/// worker that was correctly waiting. Placing the backstop strictly later means
/// the deadline always wins for anything that reached a server, the error names
/// what actually happened, and if the backstop ever does fire its message is
/// true: the reply never came even though the RPC would have answered by then.
///
/// Sized for the handoff itself — an unbounded channel send, a task spawn, and a
/// `sync_channel` reply — not for the RPC, which the deadline already bounds.
const HANDOFF_GRACE: Duration = Duration::from_secs(2);

/// The request deadline for one `stream_read_at`.
///
/// Derived in ONE place because two callers need the same answer: the public
/// method (to size its backstop) and the worker arm (to stamp the request). They
/// computed it separately before, which is the same value with two definitions.
fn tail_deadline(blocking: bool, timeout_ms: u64) -> Duration {
    if blocking {
        Duration::from_millis(timeout_ms) + TAIL_GRACE
    } else {
        OP_DEADLINE
    }
}

/// Attach `deadline` to `msg` as a gRPC request deadline.
///
/// Every op goes through here rather than passing bare messages, so "a request
/// carries a deadline" is one call shape instead of a rule each of the seven
/// arms has to remember.
fn deadlined<T>(msg: T, deadline: Duration) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    req.set_timeout(deadline);
    req
}

/// Wait for the worker task's reply, bounded.
///
/// The guarantee: **a caller never parks without an error.** That is what this
/// exists for, and it is not hypothetical — a standing A2A receiver stopped
/// consuming its inbox and was indistinguishable from an idle one for four hours
/// (process alive, CPU flat, cursor frozen while the stream advanced, zero output
/// in 160 KB of log — sudocode #696). Its poller thread was parked exactly here.
/// Converting that silence into an `io::Error` is what lets
/// `runtime::mailbox::spawn_inbox_poller` log `inbox poll failed`, back off and
/// retry, so the receive loop is self-healing rather than permanently deaf.
///
/// What this is FOR, now that the request carries its own deadline (see
/// [`deadlined`]): the handoff, which a deadline cannot reach. The op crosses an
/// unbounded channel to the worker and the reply returns over a `sync_channel`,
/// so a worker that never polls its receiver, or a task dropped at runtime
/// teardown, still has to end as an error rather than as silence. A dropped
/// sender is already `Disconnected`; this covers the rest.
///
/// It is NOT the bound that fires for anything that reached a server — but that
/// is a consequence of ORDERING, not something to assume. While this budget
/// equalled the request deadline, which of the two fired was a coin flip, and in
/// practice this one won: a daemon that missed its deadline surfaced as `vfs
/// worker sent no reply`, naming the wrong layer. [`HANDOFF_GRACE`] is what puts
/// this budget strictly later, so the deadline wins where a deadline applies and
/// this message is only ever emitted when it is true.
///
/// Timing out is NOT the same as knowing the op failed: the task may still be in
/// flight and may still complete. The error says "no answer within `budget`", and
/// every caller here is idempotent-retry safe (a re-read at the same cursor, a
/// re-`ensure_stream`), so a retry is the correct response.
fn await_reply<T>(resp_rx: &mpsc::Receiver<io::Result<T>>, budget: Duration) -> io::Result<T> {
    match resp_rx.recv_timeout(budget) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("vfs worker sent no reply within {budget:?}"),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(broken_pipe()),
    }
}

/// The first tests in this crate, and deliberately only these four.
///
/// Coverage for this client otherwise lives in `runtime/tests/mailbox_nexus_live.rs`,
/// against a real daemon, which is the right place for anything about the wire. What
/// cannot be reached from there is [`await_reply`]: it is private, and the failure it
/// exists for — a worker task that is alive and simply never answers — has no
/// constructor on the far side of a real connection.
///
/// Four cases because the interesting part is not the timeout; it is that adding a
/// ceiling must not change the other three outcomes.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_out_rather_than_parking_forever() {
        // `_tx` must stay BOUND. A bare `_` drops the sender immediately and the
        // receiver reports Disconnected, which would test the wrong branch and pass
        // for the wrong reason.
        let (_tx, rx) = mpsc::sync_channel::<io::Result<u8>>(1);
        let budget = Duration::from_millis(50);

        let started = std::time::Instant::now();
        let err = await_reply(&rx, budget).expect_err("a silent worker must not park");

        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            err.to_string().contains("no reply within"),
            "the message has to say what happened: {err}"
        );
        assert!(
            started.elapsed() >= budget,
            "returned before the budget elapsed: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_dropped_worker_is_still_a_broken_pipe() {
        let (tx, rx) = mpsc::sync_channel::<io::Result<u8>>(1);
        drop(tx);

        let err = await_reply(&rx, Duration::from_secs(30))
            .expect_err("a dropped sender must report, not wait out the budget");

        // Distinguishable from the timeout, and unchanged from the behaviour
        // before the ceiling existed.
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_ready_reply_is_returned_without_waiting() {
        let (tx, rx) = mpsc::sync_channel::<io::Result<u8>>(1);
        tx.send(Ok(7)).expect("queue the reply");

        let started = std::time::Instant::now();
        let value = await_reply(&rx, Duration::from_secs(30)).expect("reply should pass through");

        assert_eq!(value, 7);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an already-queued reply must not wait on the budget: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_workers_own_error_is_not_masked() {
        let (tx, rx) = mpsc::sync_channel::<io::Result<u8>>(1);
        tx.send(Err(vfs_err(b"permission denied by the zone")))
            .expect("queue the error");

        let err = await_reply(&rx, Duration::from_secs(30)).expect_err("the error should surface");

        // The ceiling must not turn a real refusal into a timeout, or the caller
        // retries something that will never succeed.
        assert_ne!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            err.to_string().contains("permission denied by the zone"),
            "the worker's own message must survive: {err}"
        );
    }
}
