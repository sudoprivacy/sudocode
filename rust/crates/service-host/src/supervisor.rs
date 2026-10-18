use std::fs::{self, File, OpenOptions};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use fs2::FileExt;
use nexus_vfs_client::NexusVfsClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    artifacts,
    capabilities::{self, Request, Response},
    Result,
};

const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
struct Discovery {
    endpoint: String,
    token: String,
    instance: String,
    supervisor_pid: u32,
    nexus_pid: u32,
    version: String,
}

struct Runtime {
    child: Arc<Mutex<Child>>,
    root: PathBuf,
    discovery: Discovery,
    diagnostics: Vec<std::thread::JoinHandle<std::io::Result<()>>>,
    _lock: File,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for reader in self.diagnostics.drain(..) {
            let _ = reader.join();
        }
        if read_discovery(&self.root).is_some_and(|value| value.instance == self.discovery.instance)
        {
            let _ = fs::remove_file(self.root.join("run").join("service.json"));
        }
    }
}

#[derive(Clone)]
struct ServiceState {
    client: Arc<NexusVfsClient>,
    discovery: Discovery,
    shutdown: tokio::sync::watch::Sender<bool>,
    admission: Arc<tokio::sync::RwLock<bool>>,
}

fn lock_file(root: &Path, name: &str) -> Result<File> {
    let directory = root.join("run");
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(name))?)
}

fn is_lock_contention(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn install(root: &Path) -> Result<()> {
    let lock = lock_file(root, "install.lock")?;
    lock.lock_exclusive()?;
    artifacts::install(root)
}

fn read_discovery(root: &Path) -> Option<Discovery> {
    serde_json::from_slice(&fs::read(root.join("run").join("service.json")).ok()?).ok()
}

fn validate_endpoint(endpoint: &reqwest::Url) -> Result<()> {
    if endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.path() != "/rpc"
    {
        return Err("invalid local service discovery endpoint".into());
    }
    Ok(())
}

fn call(
    http: &reqwest::blocking::Client,
    discovery: &Discovery,
    request: &Request,
) -> Result<Response> {
    let endpoint: reqwest::Url = discovery.endpoint.parse()?;
    validate_endpoint(&endpoint)?;
    let response = http
        .post(endpoint)
        .bearer_auth(&discovery.token)
        .json(&json!({"id": request.id, "method": request.method, "params": request.params}))
        .send()?
        .error_for_status()?
        .json::<Value>()?;
    if response["id"] != request.id {
        return Err("service response does not match the request".into());
    }
    if let Some(error) = response.get("error") {
        return Err(error["message"]
            .as_str()
            .unwrap_or("service request failed")
            .to_owned()
            .into());
    }
    let result = response
        .get("result")
        .ok_or("service response has no result")?
        .clone();
    Ok(Response::from_result(request.id.clone(), Ok(result)))
}

fn is_serving(http: &reqwest::blocking::Client, discovery: &Discovery) -> bool {
    call(
        http,
        discovery,
        &Request {
            id: json!(0),
            method: "service.status".into(),
            params: Value::Null,
        },
    )
    .is_ok_and(|response| {
        response
            .result
            .is_some_and(|result| result["instance"] == discovery.instance)
    })
}

fn ensure_runtime(root: &Path, http: &reqwest::blocking::Client) -> Result<Discovery> {
    if let Some(discovery) = read_discovery(root).filter(|discovery| is_serving(http, discovery)) {
        return Ok(discovery);
    }
    install(root)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["service", "daemon"])
        .env("SCODE_SERVICE_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(discovery) =
            read_discovery(root).filter(|discovery| is_serving(http, discovery))
        {
            return Ok(discovery);
        }
        if Instant::now() >= deadline {
            return Err(
                "Sudocode service did not become ready; inspect the service runtime log".into(),
            );
        }
        // A concurrent launcher may exit after losing the singleton lock.
        // Readiness remains owned by the authenticated discovery response.
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(format!(
                    "Sudocode service startup failed ({status}); inspect the service runtime log"
                )
                .into());
            }
            // The winning supervisor may be retiring or may have failed before
            // publishing discovery. Once its lifetime lock is free, launch a
            // replacement instead of waiting for readiness from an exited owner.
            let lock = lock_file(root, "service.lock")?;
            match lock.try_lock_exclusive() {
                Ok(()) => {
                    drop(lock);
                    child = command.spawn()?;
                }
                Err(error) if is_lock_contention(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Frontends own this connection. The OS-user supervisor owns the shared daemon.
pub fn run_stdio() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(serve_stdio())
}

async fn serve_stdio() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let root = Arc::new(artifacts::root()?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(35))
        .no_proxy()
        .build()?;
    let connected = Arc::new(tokio::sync::Mutex::new(None::<Discovery>));
    let output = Arc::new(tokio::sync::Mutex::new(tokio::io::stdout()));
    let capacity = Arc::new(tokio::sync::Semaphore::new(64));
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        while let Some(result) = tasks.try_join_next() {
            result??;
        }
        let permit = capacity.clone().acquire_owned().await?;
        let mut bytes = Vec::new();
        let count = (&mut input)
            .take(MAX_REQUEST_BYTES + 1)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_REQUEST_BYTES {
            return Err("service request exceeds 8 MiB".into());
        }
        let root = root.clone();
        let http = http.clone();
        let connected = connected.clone();
        let output = output.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let response = match serde_json::from_slice::<Request>(&bytes) {
                Ok(request) => dispatch_stdio(root, &http, &connected, request).await,
                Err(_) => Response::from_result(Value::Null, Err("invalid service request".into())),
            };
            let mut bytes = serde_json::to_vec(&response)?;
            bytes.push(b'\n');
            let mut output = output.lock().await;
            output.write_all(&bytes).await?;
            output.flush().await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}

fn stop_existing(root: &Path, http: &reqwest::blocking::Client) -> Result<()> {
    if let Some(discovery) = read_discovery(root).filter(|value| is_serving(http, value)) {
        call(
            http,
            &discovery,
            &Request {
                id: json!(0),
                method: "service.shutdown".into(),
                params: Value::Null,
            },
        )?;
        let lock = lock_file(root, "service.lock")?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error) if is_lock_contention(&error) => {}
                Err(error) => return Err(error.into()),
            }
            if Instant::now() >= deadline {
                return Err("Sudocode service is still draining requests".into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

fn control_operation(root: &Path, method: &str) -> Result<Value> {
    match method {
        "service.install" => {
            install(root)?;
            Ok(json!({"installed": true, "version": artifacts::version()}))
        }
        "service.checkInstalled" => Ok(
            json!({"installed": artifacts::installed(root)?, "version": artifacts::version(), "plugins": artifacts::expected_plugins()?}),
        ),
        "service.restart" | "service.removeInstallation" => {
            let http = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(35))
                .no_proxy()
                .build()?;
            stop_existing(root, &http)?;
            if method == "service.restart" {
                let discovery = ensure_runtime(root, &http)?;
                return call(
                    &http,
                    &discovery,
                    &Request {
                        id: json!(0),
                        method: "service.status".into(),
                        params: Value::Null,
                    },
                )?
                .result
                .ok_or_else(|| "missing service status".into());
            }
            let lock = lock_file(root, "service.lock")?;
            if let Err(error) = lock.try_lock_exclusive() {
                if is_lock_contention(&error) {
                    return Err("Sudocode services are in use".into());
                }
                return Err(error.into());
            }
            let install_lock = lock_file(root, "install.lock")?;
            install_lock.lock_exclusive()?;
            let assembly = artifacts::assembly(root);
            if assembly.exists() {
                let expected_parent = fs::canonicalize(root)?.join("runtimes");
                if fs::canonicalize(assembly.parent().ok_or("missing assembly parent")?)?
                    != expected_parent
                    || fs::symlink_metadata(&assembly)?.file_type().is_symlink()
                    || fs::canonicalize(&assembly)?
                        != expected_parent
                            .join(assembly.file_name().ok_or("missing assembly name")?)
                {
                    return Err("service assembly is outside its runtime directory".into());
                }
                fs::remove_dir_all(assembly)?;
            }
            Ok(Value::Null)
        }
        _ => Err("unknown service lifecycle operation".into()),
    }
}

async fn dispatch_stdio(
    root: Arc<PathBuf>,
    http: &reqwest::Client,
    connected: &tokio::sync::Mutex<Option<Discovery>>,
    request: Request,
) -> Response {
    if matches!(
        request.method.as_str(),
        "service.install"
            | "service.removeInstallation"
            | "service.checkInstalled"
            | "service.restart"
    ) {
        let method = request.method.clone();
        if method == "service.restart" || method == "service.removeInstallation" {
            *connected.lock().await = None;
        }
        let result = tokio::task::spawn_blocking(move || control_operation(&root, &method))
            .await
            .unwrap_or_else(|error| Err(error.to_string().into()));
        return Response::from_result(request.id, result);
    }
    let discovery = {
        let mut current = connected.lock().await;
        if request.method == "service.shutdown" {
            *current = read_discovery(&root);
        } else if current.is_none() {
            if request.method == "service.status" {
                *current = read_discovery(&root);
            } else {
                let service_root = root.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let probe = reqwest::blocking::Client::builder()
                        .timeout(Duration::from_secs(2))
                        .no_proxy()
                        .build()?;
                    ensure_runtime(&service_root, &probe)
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string().into()));
                match result {
                    Ok(discovery) => *current = Some(discovery),
                    Err(error) => return Response::from_result(request.id, Err(error)),
                }
            }
        }
        current.clone()
    };
    let Some(discovery) = discovery else {
        if request.method == "service.shutdown" {
            return Response::from_result(request.id, Ok(json!(true)));
        }
        return Response::from_result(
            request.id,
            Ok(json!({"running": false, "version": artifacts::version()})),
        );
    };
    let result = async {
        let endpoint: reqwest::Url = discovery.endpoint.parse()?;
        validate_endpoint(&endpoint)?;
        let response = http
            .post(endpoint)
            .bearer_auth(&discovery.token)
            .json(&json!({"id": request.id, "method": request.method, "params": request.params}))
            .send()
            .await?
            .error_for_status()?
            .json::<Response>()
            .await?;
        if response.id != request.id {
            return Err("service response does not match the request".into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(response)
    }
    .await;
    if result.is_err() || request.method == "service.shutdown" {
        let mut current = connected.lock().await;
        if current
            .as_ref()
            .is_some_and(|value| value.instance == discovery.instance)
        {
            *current = None;
        }
    }
    match result {
        Ok(response) => response,
        Err(_)
            if request.method == "service.shutdown"
                && !read_discovery(&root)
                    .is_some_and(|value| value.instance == discovery.instance) =>
        {
            Response::from_result(request.id, Ok(json!(true)))
        }
        Err(_) if request.method == "service.status" => Response::from_result(
            request.id,
            Ok(json!({"running": false, "version": artifacts::version()})),
        ),
        Err(error) => Response::from_result(request.id, Err(error)),
    }
}

async fn handle(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Json(request): Json<Request>,
) -> (StatusCode, Json<Response>) {
    if headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        != Some(format!("Bearer {}", state.discovery.token).as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(Response::from_result(
                request.id,
                Err("local service authentication failed".into()),
            )),
        );
    }
    let id = request.id.clone();
    if request.method == "service.shutdown" {
        let mut stopping = state.admission.write().await;
        let client = state.client.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<Value> {
            match client.readdir("/proc/", "") {
                Ok(entries) if !entries.is_empty() => {
                    return Err("Agent runtimes are using this service".into())
                }
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into())
                }
                _ => {}
            }
            Ok(json!(true))
        })
        .await
        .unwrap_or_else(|error| Err(error.to_string().into()));
        let result = result.and_then(|value| {
            *stopping = true;
            state
                .shutdown
                .send(true)
                .map_err(|error| error.to_string())?;
            Ok(value)
        });
        return (StatusCode::OK, Json(Response::from_result(id, result)));
    }
    let admission = state.admission.clone().read_owned().await;
    if *admission {
        return (
            StatusCode::OK,
            Json(Response::from_result(
                id,
                Err("Sudocode service is shutting down".into()),
            )),
        );
    }
    let result = tokio::task::spawn_blocking(move || {
        let _admission = admission;
        if request.method == "service.start" || request.method == "service.status" {
            let stat = state.client.stat("/", "")?;
            return Ok(json!({"running": true, "version": state.discovery.version, "zone_id": stat.zone_id,
                "instance": state.discovery.instance, "supervisorPid": state.discovery.supervisor_pid, "nexusPid": state.discovery.nexus_pid}));
        }
        capabilities::dispatch(&state.client, &request.method, &request.params)
    }).await;
    let result = result.unwrap_or_else(|error| Err(error.to_string().into()));
    (StatusCode::OK, Json(Response::from_result(id, result)))
}

/// One supervisor per OS-user service root. File locks are retained for its lifetime.
pub fn run_daemon() -> Result<()> {
    let root = artifacts::root()?;
    let lock = lock_file(&root, "service.lock")?;
    if let Err(error) = lock.try_lock_exclusive() {
        if is_lock_contention(&error) {
            return Ok(());
        }
        return Err(error.into());
    }
    install(&root)?;
    let nexus_listener = TcpListener::bind("127.0.0.1:0")?;
    let nexus_port = nexus_listener.local_addr()?.port();
    drop(nexus_listener);
    let endpoint = format!("http://127.0.0.1:{nexus_port}");
    let mut command = Command::new(artifacts::binary(&root));
    command
        .args([
            "serve-local",
            "--port",
            &nexus_port.to_string(),
            "--hostname",
            "localhost",
            "--data-dir",
        ])
        .arg(root.join("data"))
        .arg("--identity-dir")
        .arg(root.join("identity"))
        .arg("--plugin-dir")
        .arg(artifacts::plugins(&root))
        .env("NEXUS_DATA_DIR", root.join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    let mut child = command.spawn()?;
    let diagnostics = match crate::diagnostics::attach(&mut child, &root) {
        Ok(readers) => readers,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let discovery = Discovery {
        endpoint: format!("http://{}/rpc", listener.local_addr()?),
        token: format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        instance: uuid::Uuid::new_v4().to_string(),
        supervisor_pid: std::process::id(),
        nexus_pid: child.id(),
        version: artifacts::version(),
    };
    let runtime = Runtime {
        child: Arc::new(Mutex::new(child)),
        root,
        discovery: discovery.clone(),
        diagnostics,
        _lock: lock,
    };
    let client = Arc::new(NexusVfsClient::connect(&endpoint)?);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = runtime
            .child
            .lock()
            .map_err(|_| "runtime child lock poisoned")?
            .try_wait()?
        {
            return Err(format!("Nexus exited during startup: {status}").into());
        }
        if client.stat("/", "").is_ok()
            && capabilities::dispatch(
                &client,
                "vault.list",
                &json!({"namespace": "service:scode-readiness"}),
            )
            .is_ok()
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("Nexus or Vault did not become ready".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    artifacts::atomic_write(
        &runtime.root.join("run").join("service.json"),
        &serde_json::to_vec(&discovery)?,
    )?;
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let child = runtime.child.clone();
    executor.block_on(async {
        let (shutdown, mut stopping) = tokio::sync::watch::channel(false);
        let state = ServiceState { client, discovery, shutdown, admission: Arc::new(tokio::sync::RwLock::new(false)) };
        let app = Router::new().route("/rpc", post(handle))
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES as usize)).with_state(state);
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let stop = async move {
            let stopped_child = async {
                let mut interval = tokio::time::interval(Duration::from_millis(200));
                loop {
                    interval.tick().await;
                    if child.lock().is_ok_and(|mut child| !matches!(child.try_wait(), Ok(None))) { break; }
                }
            };
            tokio::select! { _ = shutdown_signal() => {}, _ = stopping.changed() => {}, _ = stopped_child => {} }
        };
        axum::serve(listener, app).with_graceful_shutdown(stop).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })?;
    drop(runtime);
    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
