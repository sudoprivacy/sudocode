//! A process-local read-only directory. Disk contains routing, never engine state.

use futures::{stream, StreamExt};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use kernel::core::agents::registry::{AgentInstance, AgentRegistry, RuntimeRegistryHost};
use runtime::runtime_status::{
    RuntimeDirectory, RuntimeInstanceId, RuntimeObservationHost, RuntimeStatusSource,
    RuntimeUnavailableScope,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_INDEX: usize = 1024;
const OWNER: &str = "local";
const ZONE: &str = "root";
type WeakStatusSource =
    Weak<dyn Fn() -> Option<runtime::runtime_status::RuntimeFacts> + Send + Sync>;
type Registries = Arc<Mutex<HashMap<String, (Weak<AgentRegistry>, WeakStatusSource)>>>;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IndexEntry {
    version: u32,
    directory_id: String,
    endpoint: SocketAddr,
    token: String,
}

struct LocalDirectory {
    // A process directory can outlive its first engine session. Keep that
    // existing host executor alive without retaining the engine or its store.
    _host: Arc<kernel::kernel::Kernel>,
    path: PathBuf,
    _ownership: File,
    registries: Registries,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for LocalDirectory {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("owner.lock"));
    }
}

static DIRECTORIES: OnceLock<Mutex<HashMap<PathBuf, Weak<LocalDirectory>>>> = OnceLock::new();

impl LocalDirectory {
    fn acquire(root: &Path, host: &Arc<kernel::kernel::Kernel>) -> io::Result<Arc<Self>> {
        let mut directories = DIRECTORIES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| io::Error::other("local runtime directory is unavailable"))?;
        if let Some(directory) = directories.get(root).and_then(Weak::upgrade) {
            return Ok(directory);
        }
        let runtime = host.runtime().handle().clone();
        prepare_routing_root(root)?;
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let entry = IndexEntry {
            version: 1,
            directory_id: uuid::Uuid::new_v4().to_string(),
            endpoint: listener.local_addr()?,
            token: uuid::Uuid::new_v4().to_string(),
        };
        let path = root.join(format!("{}.json", entry.directory_id));
        let lock_path = path.with_extension("owner.lock");
        lib::transport_primitives::create_secret(&lock_path, b"")?;
        let ownership = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)?;
        ownership
            .try_lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        // Windows' mandatory byte lock must live on a separate inode, so a
        // reader can inspect metadata while its process owns the registration.
        lib::transport_primitives::create_secret(&path, &serde_json::to_vec(&entry)?)?;
        let registries: Registries = Arc::default();
        let sources = Arc::clone(&registries);
        let _entered = runtime.enter();
        let listener = TcpListener::from_std(listener)?;
        let server = runtime.spawn(async move {
            let slots = Arc::new(tokio::sync::Semaphore::new(4));
            while let Ok((mut stream, address)) = listener.accept().await {
                if !address.ip().is_loopback() {
                    continue;
                }
                let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
                    continue;
                };
                let sources = Arc::clone(&sources);
                let expected = entry.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let _ = tokio::time::timeout(Duration::from_secs(1), async {
                        let request: IndexEntry = read_frame(&mut stream).await?;
                        if request.version != 1
                            || request.directory_id != expected.directory_id
                            || request.token != expected.token
                            || request.endpoint != expected.endpoint
                        {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "invalid runtime reader",
                            ));
                        }
                        let registries: Vec<_> = sources
                            .lock()
                            .map_err(|_| io::Error::other("directory unavailable"))?
                            .values()
                            .filter_map(|(registry, _)| registry.upgrade())
                            .collect();
                        let directory = RuntimeDirectory {
                            version: 1,
                            inventories: registries
                                .iter()
                                .map(|registry| registry.runtime_inventory(Some(OWNER), ZONE))
                                .collect(),
                            unavailable: Vec::new(),
                        };
                        write_frame(&mut stream, &directory).await
                    })
                    .await;
                });
            }
        });
        let directory = Arc::new(Self {
            _host: Arc::clone(host),
            path,
            _ownership: ownership,
            registries,
            server,
        });
        directories.retain(|_, directory| directory.strong_count() > 0);
        directories.insert(root.into(), Arc::downgrade(&directory));
        Ok(directory)
    }
}

fn prepare_routing_root(root: &Path) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    if !std::fs::symlink_metadata(root)?.file_type().is_dir() {
        return Err(io::Error::other(
            "runtime routing root must be a native directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(root)?.permissions().mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "runtime routing root must be owner-controlled",
            ));
        }
    }
    Ok(())
}

async fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> io::Result<T> {
    let size = stream.read_u32().await? as usize;
    if size > MAX_FRAME {
        return Err(io::Error::other("runtime response exceeds limit"));
    }
    let mut frame = vec![0; size];
    stream.read_exact(&mut frame).await?;
    serde_json::from_slice(&frame).map_err(io::Error::other)
}

async fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> io::Result<()> {
    let frame = serde_json::to_vec(value)?;
    if frame.len() > MAX_FRAME {
        return Err(io::Error::other("runtime response exceeds limit"));
    }
    stream
        .write_u32(u32::try_from(frame.len()).map_err(io::Error::other)?)
        .await?;
    stream.write_all(&frame).await
}

pub(crate) struct LocalRuntimeHost {
    kernel: Arc<kernel::kernel::Kernel>,
    registry: Arc<AgentRegistry>,
    instance: AgentInstance,
    host: RuntimeRegistryHost,
    root: PathBuf,
}

impl std::fmt::Debug for LocalRuntimeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRuntimeHost")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl LocalRuntimeHost {
    pub(crate) fn new(kernel: Arc<kernel::kernel::Kernel>, agent: &str) -> io::Result<Self> {
        let registry = Arc::clone(kernel.agent_registry());
        let instance = registry
            .register_instance(kernel::core::agents::registry::AgentDescriptor {
                pid: std::process::id().to_string(),
                name: agent.into(),
                owner_id: OWNER.into(),
                zone_id: ZONE.into(),
                ..Default::default()
            })
            .ok_or_else(|| io::Error::other("local runtime is already registered"))?;
        let host = RuntimeRegistryHost::new(Arc::clone(&registry), instance.clone(), None)
            .ok_or_else(|| io::Error::other("local runtime registration retired"))?;
        Ok(Self {
            kernel,
            registry,
            instance,
            host,
            root: runtime::mailbox::local_pair_root().join("runtime-instances"),
        })
    }
}

impl Drop for LocalRuntimeHost {
    fn drop(&mut self) {
        let _ = self.registry.unregister_instance(&self.instance);
    }
}

struct LocalBinding {
    directory: Arc<LocalDirectory>,
    registry_id: String,
    source: RuntimeStatusSource,
    _engine: Box<dyn Send>,
}

impl Drop for LocalBinding {
    fn drop(&mut self) {
        let mut sources = self
            .directory
            .registries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sources
            .get(&self.registry_id)
            .is_some_and(|(_, source)| source.ptr_eq(&Arc::downgrade(&self.source)))
        {
            sources.remove(&self.registry_id);
        }
    }
}

impl RuntimeObservationHost for LocalRuntimeHost {
    fn bind(&self, source: RuntimeStatusSource) -> Option<Box<dyn Send>> {
        let engine = self.host.bind(Arc::clone(&source))?;
        let directory = match LocalDirectory::acquire(&self.root, &self.kernel) {
            Ok(directory) => directory,
            Err(error) => {
                tracing::warn!(%error, "local runtime observation is unavailable");
                return None;
            }
        };
        let registry_id = self.registry.runtime_scope_id().to_string();
        directory.registries.lock().ok()?.insert(
            registry_id.clone(),
            (Arc::downgrade(&self.registry), Arc::downgrade(&source)),
        );
        Some(Box::new(LocalBinding {
            directory,
            registry_id,
            source,
            _engine: engine,
        }))
    }
    fn instance(&self) -> Option<RuntimeInstanceId> {
        self.host.instance()
    }
    fn discover(&self) -> Result<RuntimeDirectory, String> {
        self.instance().ok_or("local runtime retired")?;
        discover(&self.root, self.kernel.runtime().handle()).map_err(|error| error.to_string())
    }
}

fn ownership_is_live(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn read_entry(path: &Path) -> io::Result<Option<IndexEntry>> {
    let ownership = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.with_extension("owner.lock"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // OS-released ownership retires this route, never all instances of an Agent/SID.
    if !ownership_is_live(&ownership)? {
        return Ok(None);
    }
    let entry = read_routing_payload(path);
    // A natural exit can remove its routing bytes between the lock and read.
    // Missing bytes under a still-held owner remain an explicit coverage gap.
    if entry
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        && !ownership_is_live(&ownership)?
    {
        return Ok(None);
    }
    entry.map(Some)
}

fn read_routing_payload(path: &Path) -> io::Result<IndexEntry> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid runtime routing entry",
        ));
    }
    let metadata = std::io::Read::take(File::open(path)?, 4097);
    let entry: IndexEntry = serde_json::from_reader(metadata)?;
    if entry.version != 1
        || !entry.endpoint.ip().is_loopback()
        || path.file_stem().and_then(|name| name.to_str()) != Some(&entry.directory_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid runtime routing entry",
        ));
    }
    Ok(entry)
}

fn discover(root: &Path, runtime: &tokio::runtime::Handle) -> io::Result<RuntimeDirectory> {
    let mut result = RuntimeDirectory {
        version: 1,
        inventories: Vec::new(),
        unavailable: Vec::new(),
    };
    let mut entries = Vec::new();
    let files = std::fs::read_dir(root)?;
    let mut routes = BTreeSet::new();
    for (index, path) in files.enumerate() {
        if index >= MAX_INDEX {
            result.unavailable.push(RuntimeUnavailableScope {
                endpoint: "local-index".into(),
                reason: "index_limit".into(),
            });
            break;
        }
        let path = match path {
            Ok(path) => path.path(),
            Err(error) => {
                result.unavailable.push(RuntimeUnavailableScope {
                    endpoint: "local-index".into(),
                    reason: error.kind().to_string(),
                });
                continue;
            }
        };
        if path.extension().and_then(|extension| extension.to_str()) == Some("json") {
            routes.insert(path);
        } else if let Some(identity) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".owner.lock"))
        {
            routes.insert(root.join(format!("{identity}.json")));
        }
    }
    for path in routes {
        match read_entry(&path) {
            Ok(Some(entry)) => entries.push(entry),
            Ok(None) => {}
            Err(error) => result.unavailable.push(RuntimeUnavailableScope {
                endpoint: path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                reason: error.kind().to_string(),
            }),
        }
    }
    lib::rt::block_on_via(runtime, async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        // One deadline bounds the entire directory, including peers not yet polled.
        let mut responses = stream::iter(entries)
            .map(|entry| async move {
                let response = tokio::time::timeout_at(deadline, async {
                    let mut stream = TcpStream::connect(entry.endpoint).await?;
                    write_frame(&mut stream, &entry).await?;
                    read_frame::<RuntimeDirectory>(&mut stream).await
                })
                .await;
                (entry, response)
            })
            .buffer_unordered(4);
        while let Some((entry, response)) = responses.next().await {
            match response {
                Ok(Ok(directory))
                    if directory.version == 1
                        && directory.inventories.iter().all(|inventory| {
                            inventory.scope.owner_id.as_deref() == Some(OWNER)
                                && inventory.scope.zone_id == ZONE
                                && inventory.version == 1
                                && inventory.instances.iter().all(|observation| {
                                    observation.instance.registry_id == inventory.scope.registry_id
                                })
                        }) =>
                {
                    result.inventories.extend(directory.inventories);
                    result.unavailable.extend(directory.unavailable);
                }
                _ => result.unavailable.push(RuntimeUnavailableScope {
                    endpoint: entry.directory_id,
                    reason: "unavailable".into(),
                }),
            }
        }
        Ok(result)
    })
}
