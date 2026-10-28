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
    local_box::LocalBox,
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
    _node_lock: Option<File>,
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
    local_box: Option<Arc<LocalBox>>,
}

pub(crate) fn lock_file(root: &Path, name: &str) -> Result<File> {
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

fn spawn_runtime(root: &Path) -> Result<Child> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["service", "daemon"])
        .env("SCODE_SERVICE_ROOT", root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console(&mut command);
    #[cfg(windows)]
    stdin_peek::prevent_standard_handle_inheritance()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok(command.spawn()?)
}

fn ensure_runtime(root: &Path, http: &reqwest::blocking::Client) -> Result<Discovery> {
    if let Some(discovery) = read_discovery(root).filter(|discovery| is_serving(http, discovery)) {
        return Ok(discovery);
    }
    if let Some(profile) = LocalBox::load(root)? {
        profile.validate()?;
    } else {
        install(root)?;
    }
    let mut child: Option<Child> = None;
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(discovery) =
            read_discovery(root).filter(|discovery| is_serving(http, discovery))
        {
            return Ok(discovery);
        }
        // Losing a launch race does not transfer our start request to an owner
        // that may already be draining. Reap that launcher and observe the
        // lifetime lock again; only authenticated discovery establishes readiness.
        if let Some(status) = child.as_mut().map(Child::try_wait).transpose()?.flatten() {
            if !status.success() {
                return Err(format!(
                    "Sudocode service startup failed ({status}); inspect the service runtime log"
                )
                .into());
            }
            child = None;
        }
        if Instant::now() >= deadline {
            let lock = lock_file(root, "service.lock")?;
            let owner = match lock.try_lock_exclusive() {
                Ok(()) => "vacant",
                Err(error) if is_lock_contention(&error) => "held",
                Err(error) => return Err(error.into()),
            };
            let launcher = child.as_ref().map_or("none", |_| "running");
            return Err(format!(
                "Sudocode service did not become ready (launcher: {launcher}, service lock: {owner}); inspect the service runtime log"
            )
            .into());
        }
        if child.is_none() {
            let lock = lock_file(root, "service.lock")?;
            match lock.try_lock_exclusive() {
                Ok(()) => {
                    // Do not queue extra daemons behind an active lifetime lock.
                    // Another starter can win after this probe; its loser exits
                    // and is reaped above, without extending the start deadline.
                    drop(lock);
                    child = Some(spawn_runtime(root)?);
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
        wait_for_retirement(root)?;
    }
    Ok(())
}

fn wait_for_retirement(root: &Path) -> Result<()> {
    let lock = lock_file(root, "service.lock")?;
    let deadline = Instant::now() + Duration::from_secs(35);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if is_lock_contention(&error) => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err("Sudocode service is still draining requests".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Operator-only exact-target stop. Ordinary frontend shutdown cannot stop a
/// shared Box. The stop uses `Child::kill`/`wait`, not a claimed graceful Raft drain.
pub fn stop_local_box(
    node_id: &str,
    binary_sha256: &str,
    owner_root: Option<&Path>,
) -> Result<Value> {
    let root = artifacts::owner_root(owner_root)?;
    let profile = LocalBox::load(&root)?.ok_or("No registered local Box")?;
    profile.validate()?;
    let params =
        json!({"nodeId": node_id, "binarySha256": binary_sha256, "acknowledgeSharedOutage": true});
    if !profile.matches_stop(&params) {
        return Err("The maintenance target does not match the registered local Box".into());
    }
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(35))
        .no_proxy()
        .build()?;
    let discovery = read_discovery(&root)
        .filter(|value| is_serving(&http, value))
        .ok_or("No managed local Box is running; an external Node is never stopped")?;
    call(
        &http,
        &discovery,
        &Request {
            id: json!(0),
            method: "service.stopLocalBox".into(),
            params,
        },
    )?;
    wait_for_retirement(&root)?;
    Ok(
        json!({"stopped": true, "nodeId": node_id, "binarySha256": binary_sha256,
        "termination": "child_kill_and_wait", "gracefulRaftDrain": false}),
    )
}

fn control_operation(root: &Path, method: &str) -> Result<Value> {
    if LocalBox::load(root)?.is_some() {
        return Err(
            "An existing Box keeps its pinned installation and coordinated Node lifecycle".into(),
        );
    }
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

/// Inspect the same runtime used by frontend channels without installing or starting it.
pub fn inspect_local_services() -> Result<Value> {
    inspect_local_services_at(None)
}

pub fn inspect_local_services_at(owner_root: Option<&Path>) -> Result<Value> {
    let root = artifacts::owner_root(owner_root)?;
    let profile = LocalBox::load(&root)?;
    let installed = if let Some(profile) = &profile {
        profile.validate()?;
        json!({"profile": "existing_box", "version": profile.version()})
    } else {
        control_operation(&root, "service.checkInstalled")?
    };
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()?;
    let running = read_discovery(&root).filter(|discovery| is_serving(&http, discovery));
    Ok(
        json!({"installation": installed, "running": running.is_some(),
        "runningVersion": running.map(|discovery| discovery.version)}),
    )
}

/// CLI setup uses the frontend's existing install/discovery/lifecycle owner.
pub fn start_local_services() -> Result<Value> {
    let root = artifacts::root()?;
    if LocalBox::load(&root)?.is_some() {
        return Err("This service root owns an existing Box; use its configured Box record".into());
    }
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()?;
    let discovery = ensure_runtime(&root, &http)?;
    if discovery.version != artifacts::version() {
        return Err("The shared Nexus version differs from this CLI; use the service owner's controlled upgrade before selecting shared mode".into());
    }
    call(
        &http,
        &discovery,
        &Request {
            id: json!(0),
            method: "service.status".into(),
            params: Value::Null,
        },
    )?
    .result
    .ok_or_else(|| "missing service status".into())
}

/// Resume only an explicitly registered local owner matching the client record.
/// A remote or unregistered Box remains a connection target, with no process effect.
pub fn ensure_local_box(endpoint: &str, owner_root: Option<&Path>) -> Result<Option<Value>> {
    let root = artifacts::owner_root(owner_root)?;
    let Some(profile) = LocalBox::load(&root)? else {
        if owner_root.is_some() {
            return Err("The selected owner root has no registered local Box".into());
        }
        return Ok(None);
    };
    if profile.box_endpoint != endpoint {
        return Err("The registered local owner does not match the configured Box".into());
    }
    profile.validate()?;
    let http = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()?;
    if let Some(discovery) = read_discovery(&root).filter(|value| is_serving(&http, value)) {
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
        .ok_or_else(|| "missing service status".into())
        .map(Some);
    }
    // An owner holding its lifetime lock may have bound the Node port before
    // publishing authenticated readiness. Join its startup instead of probing
    // that intermediate port as an externally owned Node.
    let lifetime = lock_file(&root, "service.lock")?;
    let external = match lifetime.try_lock_exclusive() {
        Ok(()) => profile.external_running()?,
        Err(error) if is_lock_contention(&error) => false,
        Err(error) => return Err(error.into()),
    };
    drop(lifetime);
    if external {
        return Ok(Some(profile.status(false)));
    }
    let discovery = ensure_runtime(&root, &http)?;
    call(
        &http,
        &discovery,
        &Request {
            id: json!(0),
            method: "service.status".into(),
            params: Value::Null,
        },
    )?
    .result
    .ok_or_else(|| "missing service status".into())
    .map(Some)
}

async fn dispatch_stdio(
    root: Arc<PathBuf>,
    http: &reqwest::Client,
    connected: &tokio::sync::Mutex<Option<Discovery>>,
    request: Request,
) -> Response {
    if request.method == "service.stopLocalBox" {
        return Response::from_result(
            request.id,
            Err("Use the explicit local operator maintenance command".into()),
        );
    }
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
    if request.method == "service.shutdown" || request.method == "service.stopLocalBox" {
        let maintenance = request.method == "service.stopLocalBox";
        if (maintenance
            && !state
                .local_box
                .as_ref()
                .is_some_and(|profile| profile.matches_stop(&request.params)))
            || (!maintenance && state.local_box.is_some())
        {
            return (
                StatusCode::OK,
                Json(Response::from_result(
                    id,
                    Err("An existing shared Box requires its coordinated Node owner stop".into()),
                )),
            );
        }
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
            if let Some(profile) = &state.local_box {
                let mut status = profile.status(true);
                status["instance"] = json!(state.discovery.instance);
                status["supervisorPid"] = json!(state.discovery.supervisor_pid);
                status["nexusPid"] = json!(state.discovery.nexus_pid);
                return Ok(status);
            }
            return Ok(json!({"running": true, "version": state.discovery.version, "zone_id": stat.zone_id,
                "instance": state.discovery.instance, "supervisorPid": state.discovery.supervisor_pid, "nexusPid": state.discovery.nexus_pid}));
        }
        if state.local_box.is_some() {
            return Err("Existing Box owner exposes lifecycle status only; use the Agent's native connection".into());
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
    let profile = LocalBox::load(&root)?.map(Arc::new);
    let mut node_lock = None;
    let (mut command, client) = if let Some(profile) = &profile {
        profile.validate()?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(profile.data_dir().join("scode-service.lock"))?;
        file.try_lock_exclusive()
            .map_err(|_| "Another service owner holds this Node")?;
        node_lock = Some(file);
        (profile.command()?, profile.client()?)
    } else {
        install(&root)?;
        let nexus_listener = TcpListener::bind("127.0.0.1:0")?;
        let nexus_port = nexus_listener.local_addr()?.port();
        drop(nexus_listener);
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
            .env("NEXUS_DATA_DIR", root.join("data"));
        (
            command,
            NexusVfsClient::connect(&format!("http://127.0.0.1:{nexus_port}"))?,
        )
    };
    let client = Arc::new(client);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_console(&mut command);
    // Resolve fallible broker setup before the Node exists, so an early broker
    // error cannot drop a bare Child and leave an unsupervised Node behind.
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let broker_endpoint = format!("http://{}/rpc", listener.local_addr()?);
    let mut child = command.spawn()?;
    let diagnostics = match crate::diagnostics::attach(&mut child, &root) {
        Ok(readers) => readers,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.into());
        }
    };
    let discovery = Discovery {
        endpoint: broker_endpoint,
        token: format!("{}{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4()),
        instance: uuid::Uuid::new_v4().to_string(),
        supervisor_pid: std::process::id(),
        nexus_pid: child.id(),
        version: profile
            .as_ref()
            .map_or_else(artifacts::version, |profile| profile.version()),
    };
    let runtime = Runtime {
        child: Arc::new(Mutex::new(child)),
        root,
        discovery: discovery.clone(),
        diagnostics,
        _lock: lock,
        _node_lock: node_lock,
    };
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
            && (profile.is_some()
                || capabilities::dispatch(
                    &client,
                    "vault.list",
                    &json!({"namespace": "service:scode-readiness"}),
                )
                .is_ok())
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
        let state = ServiceState { client, discovery, shutdown, admission: Arc::new(tokio::sync::RwLock::new(false)), local_box: profile };
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
