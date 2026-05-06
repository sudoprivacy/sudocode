pub mod proto {
    tonic::include_proto!("nexus.grpc.vfs");
}

use proto::nexus_vfs_service_client::NexusVfsServiceClient;
use proto::{CallRequest, DeleteRequest, ReadRequest, WriteRequest};
use std::io;
use std::sync::mpsc;

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
}

/// Sync wrapper around the nexus VFS gRPC client.
///
/// Maintains a background tokio thread; all public methods are blocking
/// and can be called from any synchronous context, including outside of
/// an async runtime.
pub struct NexusVfsClient {
    tx: tokio::sync::mpsc::Sender<VfsOp>,
}

impl NexusVfsClient {
    /// Connect to a nexus VFS gRPC server at `endpoint`.
    ///
    /// The channel is lazy — the actual TCP/UDS connection is deferred
    /// until the first RPC. Returns an error only if the background
    /// thread cannot be spawned or the endpoint URI is invalid.
    pub fn connect(endpoint: &str) -> io::Result<Self> {
        let endpoint = endpoint.to_owned();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<VfsOp>(64);

        std::thread::Builder::new()
            .name("nexus-vfs-client".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("nexus-vfs tokio runtime");
                rt.block_on(async move {
                    let ch = tonic::transport::Channel::from_shared(endpoint)
                        .expect("invalid vfs endpoint URI")
                        .connect_lazy();
                    let mut client = NexusVfsServiceClient::new(ch);
                    while let Some(op) = rx.recv().await {
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
                                        etag: String::new(),
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
                        }
                    }
                });
            })
            .map_err(io::Error::other)?;

        Ok(Self { tx })
    }

    pub fn read(&self, path: &str, auth_token: &str) -> io::Result<Vec<u8>> {
        let (resp_tx, resp_rx) = mpsc::sync_channel(1);
        self.tx
            .blocking_send(VfsOp::Read {
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
            .blocking_send(VfsOp::Write {
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
            .blocking_send(VfsOp::Delete {
                path: path.to_owned(),
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
            .blocking_send(VfsOp::Call {
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
