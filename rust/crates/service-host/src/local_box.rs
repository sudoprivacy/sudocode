//! Explicit OS-user ownership of an existing authenticated Node. A client Box
//! record never grants process ownership. Resume uses the Node's native boot
//! contract and existing persistence, with no bootstrap or upgrade operation.

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use nexus_vfs_client::NexusVfsClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{artifacts, Result};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalBox {
    schema_version: u8,
    pub(crate) box_endpoint: String,
    binary: PathBuf,
    binary_sha256: String,
    data_dir: PathBuf,
    identity_dir: PathBuf,
    node_id: String,
    ca_sha256: String,
    certificate_sha256: String,
    /// A numeric loopback address on the Node's existing listening port.
    local_endpoint: SocketAddr,
    advertise_addr: String,
    bind_addr: SocketAddr,
    hostname: String,
    server_name: String,
    #[serde(default)]
    accept_enrollments: bool,
}

fn path(root: &Path) -> PathBuf {
    root.join("run").join("local-box.json")
}

fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 8 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&bytes[..count]);
    }
}

fn pinned_file(path: &Path, expected: &str) -> Result<()> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || digest(path)? != expected
    {
        return Err(format!("Local Box pin does not match {}", path.display()).into());
    }
    Ok(())
}

impl LocalBox {
    pub(crate) fn load(root: &Path) -> Result<Option<Self>> {
        match fs::read(path(root)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn version(&self) -> String {
        format!("pinned:{}", self.binary_sha256)
    }

    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || !self.local_endpoint.ip().is_loopback()
            || self.local_endpoint.port() == 0
            || self.bind_addr.port() != self.local_endpoint.port()
            || !(self.bind_addr.ip().is_unspecified()
                || self.bind_addr.ip() == self.local_endpoint.ip())
            || self.box_endpoint.contains("://")
            || self.advertise_addr.contains("://")
            || self
                .box_endpoint
                .rsplit_once(':')
                .and_then(|(_, p)| p.parse::<u16>().ok())
                != Some(self.local_endpoint.port())
            || self
                .advertise_addr
                .rsplit_once(':')
                .and_then(|(_, p)| p.parse::<u16>().ok())
                != Some(self.local_endpoint.port())
            || self.hostname.is_empty()
            || self.server_name.is_empty()
            || self.node_id.parse::<u64>().ok().is_none_or(|id| id == 0)
        {
            return Err("Invalid existing local Box declaration".into());
        }
        for path in [&self.binary, &self.data_dir, &self.identity_dir] {
            if !path.is_absolute() || !path.exists() {
                return Err("Local Box paths must be absolute and already exist".into());
            }
        }
        let data = fs::canonicalize(&self.data_dir)?;
        let identity = fs::canonicalize(&self.identity_dir)?;
        if identity.starts_with(&data) || data.starts_with(&identity) {
            return Err(
                "Local Box identity and data require separate persistent directories".into(),
            );
        }
        pinned_file(&self.binary, &self.binary_sha256)?;
        pinned_file(&data.join("tls/ca.pem"), &self.ca_sha256)?;
        pinned_file(&data.join("tls/node.pem"), &self.certificate_sha256)?;
        // Nexus persists the opaque ID as exactly eight big-endian bytes.
        // Read only: the native read-or-mint function must never create it here.
        let node_id: [u8; 8] = fs::read(data.join(".node_id"))?
            .try_into()
            .map_err(|_| "Local Box Node ID is not eight bytes")?;
        if u64::from_be_bytes(node_id).to_string() != self.node_id
            || !data.join("tls/node-key.pem").is_file()
            || !data.join("metastore.redb").is_file()
            || !data.join("root/raft/raft.redb").is_file()
        {
            return Err("Local Box original Node identity or persistent state is missing".into());
        }
        let identity: Value = serde_json::from_slice(&fs::read(identity.join("identity.json"))?)?;
        let zones = identity["zones"]
            .as_array()
            .ok_or("Local Box identity has no persisted zones")?;
        if zones.is_empty() {
            return Err("Local Box has no persisted zones; fresh bootstrap is not recovery".into());
        }
        for zone in zones {
            let zone = zone["zone_id"].as_str().ok_or("Local Box zone has no ID")?;
            if zone.is_empty()
                || zone.contains(['/', '\\'])
                || zone == "."
                || zone == ".."
                || !data.join(zone).join("raft/raft.redb").is_file()
            {
                return Err(
                    "Local Box has missing zone persistence; refusing to initialize it".into(),
                );
            }
        }
        Ok(())
    }

    pub(crate) fn client(&self) -> Result<NexusVfsClient> {
        Ok(NexusVfsClient::connect_tls(
            &self.local_endpoint.to_string(),
            fs::read(self.data_dir.join("tls/ca.pem"))?,
            fs::read(self.data_dir.join("tls/node.pem"))?,
            fs::read(self.data_dir.join("tls/node-key.pem"))?,
            &self.server_name,
        )?)
    }

    /// An occupied endpoint belongs to the existing owner. It is never stopped,
    /// adopted by PID, or taken as permission to start another Node.
    pub(crate) fn external_running(&self) -> Result<bool> {
        // Probe the declared listener, not the loopback client address: Windows
        // can bind loopback beside an existing wildcard listener. Binding the
        // same address also avoids inferring liveness from connect timeouts.
        match TcpListener::bind(self.bind_addr) {
            Ok(listener) => {
                drop(listener);
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                self.client()?.stat("/", "")?;
                Ok(true)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn command(&self) -> Result<Command> {
        self.validate()?;
        if self.external_running()? {
            return Err("The local Box is already running outside this service owner".into());
        }
        let mut command = self.node_process();
        command.args([
            "--advertise-addr",
            &self.advertise_addr,
            "--bind-addr",
            &self.bind_addr.to_string(),
            "--hostname",
            &self.hostname,
        ]);
        if self.accept_enrollments {
            command.arg("--accept-enrollments");
        }
        Ok(command)
    }

    /// The pinned Node executable on this Box's own data and identity.
    fn node_process(&self) -> Command {
        let mut command = Command::new(&self.binary);
        // The supervisor's caller cannot inject bootstrap/auth/storage settings.
        // No enrollment token, CA mint, cluster-init, force, or shell is used.
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("NEXUS_")
            {
                command.env_remove(name);
            }
        }
        command
            .current_dir(&self.data_dir)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--identity-dir")
            .arg(&self.identity_dir);
        command
    }

    /// The Node's own online issuance for one Agent name, addressed to its
    /// loopback listener. Never `--allow-existing`: Nexus refuses a name
    /// already claimed anywhere in the cluster instead of rotating it.
    fn mint_agent_command(&self, agent: &str) -> Command {
        let mut command = self.node_process();
        command
            .args(["--bind-addr", &self.local_endpoint.to_string()])
            .args(["auth", "mint", "--subject-type", "agent"])
            .arg(format!("--subject-id={agent}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::supervisor::hide_console(&mut command);
        command
    }

    pub(crate) fn status(&self, managed: bool) -> Value {
        json!({"profile": "existing_box", "endpoint": self.box_endpoint,
            "persistedNodeId": self.node_id, "binarySha256": self.binary_sha256,
            "managed": managed, "running": true, "domainJoinAccepted": false})
    }

    pub(crate) fn matches_stop(&self, params: &Value) -> bool {
        params["nodeId"] == self.node_id
            && params["binarySha256"] == self.binary_sha256
            && params["acknowledgeSharedOutage"] == true
    }
}

/// Explicit local operator entry only; not an RPC or an Agent tool. Registration
/// publishes no client record, starts nothing, and refuses replacement.
pub fn register_local_box(config: &Path, owner_root: Option<&Path>) -> Result<Value> {
    let root = artifacts::owner_root(owner_root)?;
    let profile: LocalBox = serde_json::from_slice(&fs::read(config)?)?;
    profile.validate()?;
    let registration = super::supervisor::lock_file(&root, "registration.lock")?;
    fs2::FileExt::lock_exclusive(&registration)?;
    let lifetime = super::supervisor::lock_file(&root, "service.lock")?;
    fs2::FileExt::try_lock_exclusive(&lifetime)
        .map_err(|_| "The service root is already in use")?;
    if path(&root).exists() || root.join("data").exists() {
        return Err("The service root already has an owner profile or local data".into());
    }
    artifacts::atomic_write(&path(&root), &serde_json::to_vec(&profile)?)?;
    Ok(
        json!({"registered": true, "started": false, "endpoint": profile.box_endpoint,
        "persistedNodeId": profile.node_id, "binarySha256": profile.binary_sha256}),
    )
}

/// What joining a session's own Agent to this user's local Box did.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentEnrollment {
    /// The Agent already had a credential; nothing was issued.
    AlreadyPresent,
    /// The running Node issued the Agent's credential just now.
    Joined,
}

/// Bounds the Node's own online issuance; its RPC timeout is 15 seconds.
const MINT_TIMEOUT: Duration = Duration::from_secs(45);
/// Bounds waiting for another session's issuance on the same Box.
const ENROLLMENT_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// Issue `agent`'s credential through this OS user's registered local Box.
///
/// Only the Box this user owns, as named by the machine's node record, may
/// issue; anything else is an error naming the owner to ask instead. Nothing is
/// issued for a name that already has a bundle, and the Node refuses a name
/// already claimed elsewhere in its cluster, so this never takes over an Agent.
/// An issuance that cannot be confirmed is moved out of the credential
/// directory, so it never becomes the session's identity.
///
/// # Errors
/// No registered or matching local Box, a Box that is not running, an invalid
/// name, a Node refusal, or an issuance without the Node's online receipt.
pub fn enroll_local_agent(
    agent: &str,
    endpoint: &str,
    agents_dir: &Path,
    owner_root: Option<&Path>,
) -> Result<AgentEnrollment> {
    enroll_with(agent, endpoint, agents_dir, owner_root, MINT_TIMEOUT)
}

/// A name that is one directory under `agents_dir` on every platform and one
/// argument to the Node.
fn valid_agent_name(agent: &str) -> bool {
    !agent.is_empty()
        && !agent.starts_with(['-', '.'])
        && !agent.ends_with(['.', ' '])
        && !agent.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    ':' | '/' | '\\' | '@' | '<' | '>' | '"' | '|' | '?' | '*'
                )
        })
}

fn enroll_with(
    agent: &str,
    endpoint: &str,
    agents_dir: &Path,
    owner_root: Option<&Path>,
    timeout: Duration,
) -> Result<AgentEnrollment> {
    if !valid_agent_name(agent) {
        return Err(format!("'{agent}' is not a valid Agent name").into());
    }
    let root = artifacts::owner_root(owner_root)?;
    let profile = LocalBox::load(&root)?
        .ok_or("this OS user has no registered local Box, so it cannot issue Agent credentials")?;
    if profile.box_endpoint != endpoint {
        return Err(format!(
            "the configured node {endpoint} is not this user's local Box ({})",
            profile.box_endpoint
        )
        .into());
    }
    profile.validate()?;
    // The Node writes bundles to `<data_dir>/agents`, which a fresh Box may not
    // have created yet.
    let own_data = fs::canonicalize(&profile.data_dir).ok();
    let named_data = agents_dir
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    if own_data.is_none()
        || named_data != own_data
        || agents_dir.file_name() != Some(std::ffi::OsStr::new("agents"))
    {
        return Err(format!(
            "the node record's credential directory {} is not this local Box's",
            agents_dir.display()
        )
        .into());
    }
    let bundle = agents_dir.join(agent);
    let lock = super::supervisor::lock_file(&root, "enrollment.lock")?;
    let waiting = Instant::now();
    while fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        if waiting.elapsed() >= ENROLLMENT_LOCK_TIMEOUT {
            return Err("another session is still enrolling through this local Box".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if bundle.join("credential.json").is_file() {
        return if manifest_names(&bundle, agent) {
            Ok(AgentEnrollment::AlreadyPresent)
        } else {
            Err(format!(
                "the credential at {} does not name '{agent}'",
                bundle.display()
            )
            .into())
        };
    }
    if bundle.exists() {
        return Err(format!(
            "{} exists without a credential manifest; inspect it before enrolling",
            bundle.display()
        )
        .into());
    }
    TcpStream::connect_timeout(&profile.local_endpoint, Duration::from_secs(2)).map_err(|e| {
        format!(
            "the local Box at {} is not running: {e}",
            profile.local_endpoint
        )
    })?;
    let output = match run_bounded(profile.mint_agent_command(agent), timeout) {
        Ok(output) => output,
        Err(error) => return Err(quarantine(&bundle, &error.to_string())),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(quarantine(
            &bundle,
            &format!(
                "the node refused to issue '{agent}': {}. If another Agent already uses this \
                 name, set a different `agentName` for this workspace",
                last_lines(&stderr)
            ),
        ));
    }
    // An offline cold-start signature would also exit zero; only the Node's
    // own online receipt proves the running cluster recorded the claim.
    let receipt = format!(
        "subject=agent:{agent} via https://{}",
        profile.local_endpoint
    );
    if !stderr.contains(&receipt) {
        return Err(quarantine(
            &bundle,
            &format!("the node returned no online receipt for '{agent}'"),
        ));
    }
    if !manifest_names(&bundle, agent) {
        return Err(quarantine(
            &bundle,
            &format!("the credential written for '{agent}' does not name it"),
        ));
    }
    Ok(AgentEnrollment::Joined)
}

fn manifest_names(bundle: &Path, agent: &str) -> bool {
    fs::read(bundle.join("credential.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .is_some_and(|manifest| manifest["agent"] == agent)
}

/// An issuance that could not be confirmed must never become the session's
/// identity: move whatever it wrote out of the credential directory. Nexus may
/// still hold the name, so what follows is the node owner's decision.
fn quarantine(bundle: &Path, cause: &str) -> Box<dyn std::error::Error + Send + Sync> {
    if !bundle.exists() {
        return cause.to_string().into();
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let name = bundle
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let held = bundle.with_file_name(format!(".unconfirmed-{name}-{stamp}"));
    match fs::rename(bundle, &held) {
        Ok(()) => format!(
            "{cause}. The unconfirmed credential was moved to {} and is not used; ask the \
             node owner whether the name was issued before retrying",
            held.display()
        )
        .into(),
        Err(error) => format!(
            "{cause}. The unconfirmed credential at {} could not be moved aside ({error}); \
             remove it before this session uses A2A",
            bundle.display()
        )
        .into(),
    }
}

fn run_bounded(mut command: Command, timeout: Duration) -> Result<Output> {
    fn drain(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    }
    let mut child = command.spawn()?;
    // Read both pipes while waiting so a chatty Node cannot block on a full pipe.
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the node did not answer within {}s; its issuance outcome is unknown",
                    timeout.as_secs()
                )
                .into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                return Err(error.into());
            }
        }
    }
    Ok(Output {
        status: child.wait()?,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(2)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[cfg(windows)]
    const FAKE_NODE: (&str, &str) = (
        "fake-node.cmd",
        r#"@echo off
setlocal EnableExtensions
set "HERE=%~dp0"
set "DATA=%~2"
set "BIND=%~6"
>>"%HERE%calls.txt" echo %*
set NEXUS_>"%HERE%env.txt" 2>nul
shift & shift & shift & shift & shift & shift & shift & shift & shift & shift
rem cmd may split "--subject-id=NAME" at "=" when it is not quoted.
set "NAME=%~2"
if "%NAME%"=="" for /f "tokens=2 delims==" %%a in ("%~1") do set "NAME=%%a"
set /p MODE=<"%HERE%mode.txt"
if "%MODE%"=="slow" ping -n 3 127.0.0.1 >nul
if "%MODE%"=="refuse" (
  echo error: agent:%NAME% is already registered 1>&2
  exit /b 1
)
set "OWNER=%NAME%"
if "%MODE%"=="wrong" set "OWNER=someone-else"
mkdir "%DATA%\agents\%NAME%"
>"%DATA%\agents\%NAME%\credential.json" echo {"agent":"%OWNER%"}
if "%MODE%"=="noreceipt" (
  echo minted agent cert subject=agent:%NAME% -^> offline 1>&2
  exit /b 0
)
echo minted agent cert subject=agent:%NAME% via https://%BIND% -^> written 1>&2
exit /b 0
"#,
    );

    #[cfg(unix)]
    const FAKE_NODE: (&str, &str) = (
        "fake-node.sh",
        r#"#!/bin/sh
here=$(dirname "$0")
echo "$*" >> "$here/calls.txt"
env | grep '^NEXUS_' > "$here/env.txt"
data=$2; bind=$6; name=${11#--subject-id=}
mode=$(cat "$here/mode.txt")
[ "$mode" = slow ] && sleep 2
if [ "$mode" = refuse ]; then echo "error: agent:$name is already registered" >&2; exit 1; fi
owner=$name; [ "$mode" = wrong ] && owner=someone-else
mkdir -p "$data/agents/$name"
printf '{"agent":"%s"}\n' "$owner" > "$data/agents/$name/credential.json"
if [ "$mode" = noreceipt ]; then echo "minted agent cert subject=agent:$name -> offline" >&2; exit 0; fi
echo "minted agent cert subject=agent:$name via https://$bind -> written" >&2
"#,
    );

    /// A registered local Box whose "Node" is a script and whose listener is
    /// held by the test, so issuance runs without a real cluster.
    struct Fixture {
        dir: tempfile::TempDir,
        listener: Option<TcpListener>,
        endpoint: String,
    }

    impl Fixture {
        fn new(mode: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let bin = root.join("bin");
            let data = root.join("data");
            let identity = root.join("identity");
            for path in [
                bin.clone(),
                data.join("tls"),
                data.join("agents"),
                data.join("root/raft"),
                identity.clone(),
            ] {
                fs::create_dir_all(path).unwrap();
            }
            let node = bin.join(FAKE_NODE.0);
            fs::write(&node, FAKE_NODE.1).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
            }
            fs::write(bin.join("mode.txt"), mode).unwrap();
            for (name, body) in [
                ("tls/ca.pem", "ca"),
                ("tls/node.pem", "node"),
                ("tls/node-key.pem", "key"),
                ("metastore.redb", ""),
                ("root/raft/raft.redb", ""),
            ] {
                fs::write(data.join(name), body).unwrap();
            }
            fs::write(data.join(".node_id"), 7u64.to_be_bytes()).unwrap();
            fs::write(
                identity.join("identity.json"),
                r#"{"zones":[{"zone_id":"root"}]}"#,
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let endpoint = format!("127.0.0.1:{port}");
            let profile = json!({
                "schemaVersion": 1,
                "boxEndpoint": endpoint,
                "binary": node,
                "binarySha256": digest(&node).unwrap(),
                "dataDir": data,
                "identityDir": identity,
                "nodeId": "7",
                "caSha256": digest(&data.join("tls/ca.pem")).unwrap(),
                "certificateSha256": digest(&data.join("tls/node.pem")).unwrap(),
                "localEndpoint": endpoint,
                "advertiseAddr": endpoint,
                "bindAddr": endpoint,
                "hostname": "fixture",
                "serverName": "fixture",
            });
            fs::create_dir_all(root.join("owner/run")).unwrap();
            fs::write(
                root.join("owner/run/local-box.json"),
                serde_json::to_vec(&profile).unwrap(),
            )
            .unwrap();
            Self {
                dir,
                listener: Some(listener),
                endpoint,
            }
        }

        fn owner(&self) -> PathBuf {
            self.dir.path().join("owner")
        }

        fn agents(&self) -> PathBuf {
            self.dir.path().join("data/agents")
        }

        fn enroll(&self, agent: &str) -> Result<AgentEnrollment> {
            enroll_local_agent(agent, &self.endpoint, &self.agents(), Some(&self.owner()))
        }

        fn calls(&self) -> Vec<String> {
            fs::read_to_string(self.dir.path().join("bin/calls.txt"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn bundle(&self, agent: &str) -> PathBuf {
            self.agents().join(agent).join("credential.json")
        }
    }

    fn error(result: Result<AgentEnrollment>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn issues_the_session_agent_through_the_running_local_box() {
        let fixture = Fixture::new("ok");
        std::env::set_var("NEXUS_ENROLL_FIXTURE_LEAK", "1");
        assert_eq!(
            fixture.enroll("worker-1a2b3c").unwrap(),
            AgentEnrollment::Joined
        );
        assert!(fixture.bundle("worker-1a2b3c").is_file());
        let calls = fixture.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(
            calls[0].contains(&format!("--bind-addr {}", fixture.endpoint))
                && calls[0].contains("auth mint --subject-type agent ")
                && calls[0].contains("--subject-id=worker-1a2b3c")
                && !calls[0].contains("allow-existing"),
            "{}",
            calls[0]
        );
        let leaked = fs::read_to_string(fixture.dir.path().join("bin/env.txt")).unwrap_or_default();
        assert!(
            !leaked.contains("NEXUS_"),
            "NEXUS_* reached the Node: {leaked}"
        );
        // A present credential is never issued again.
        assert_eq!(
            fixture.enroll("worker-1a2b3c").unwrap(),
            AgentEnrollment::AlreadyPresent
        );
        assert_eq!(fixture.calls().len(), 1);
    }

    #[test]
    fn only_this_users_running_box_may_issue() {
        let fixture = Fixture::new("ok");
        let unregistered = tempfile::tempdir().unwrap();
        let message = error(enroll_local_agent(
            "worker",
            &fixture.endpoint,
            &fixture.agents(),
            Some(unregistered.path()),
        ));
        assert!(message.contains("no registered local Box"), "{message}");
        let message = error(enroll_local_agent(
            "worker",
            "127.0.0.1:1",
            &fixture.agents(),
            Some(&fixture.owner()),
        ));
        assert!(
            message.contains("is not this user's local Box"),
            "{message}"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        let message = error(enroll_local_agent(
            "worker",
            &fixture.endpoint,
            &elsewhere.path().join("agents"),
            Some(&fixture.owner()),
        ));
        assert!(message.contains("is not this local Box's"), "{message}");
        assert_eq!(fixture.calls(), Vec::<String>::new());
        assert!(!fixture.bundle("worker").exists());
    }

    #[test]
    fn a_stopped_box_is_reported_without_issuing() {
        let mut fixture = Fixture::new("ok");
        drop(fixture.listener.take());
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("is not running"), "{message}");
        assert_eq!(fixture.calls(), Vec::<String>::new());
    }

    #[test]
    fn invalid_names_are_refused_before_the_node_runs() {
        let fixture = Fixture::new("ok");
        for name in [
            "", ".", "..", ".hidden", "-x", "a.", "a ", "a/b", "a\\b", "a:b", "a@b", "a|b", "a\nb",
        ] {
            let message = error(fixture.enroll(name));
            assert!(
                message.contains("is not a valid Agent name"),
                "{name:?}: {message}"
            );
        }
        assert_eq!(fixture.calls(), Vec::<String>::new());
    }

    #[test]
    fn a_node_refusal_carries_its_reason() {
        let fixture = Fixture::new("refuse");
        let message = error(fixture.enroll("taken"));
        assert!(
            message.contains("refused to issue 'taken'") && message.contains("already registered"),
            "{message}"
        );
        assert!(!fixture.bundle("taken").exists());
    }

    /// An unconfirmed issuance is moved aside so the session never uses it.
    fn assert_quarantined(fixture: &Fixture, agent: &str, message: &str) {
        assert!(!fixture.bundle(agent).exists(), "{message}");
        let held = fs::read_dir(fixture.agents())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(".unconfirmed-{agent}-"))
            });
        assert!(held && message.contains("is not used"), "{message}");
    }

    #[test]
    fn issuance_without_the_online_receipt_is_not_a_join() {
        let fixture = Fixture::new("noreceipt");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("no online receipt"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
    }

    #[test]
    fn a_credential_for_another_agent_is_not_a_join() {
        let fixture = Fixture::new("wrong");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("does not name it"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
    }

    #[test]
    fn an_unanswered_issuance_is_unknown_and_never_used() {
        let fixture = Fixture::new("slow");
        let message = error(enroll_with(
            "worker",
            &fixture.endpoint,
            &fixture.agents(),
            Some(&fixture.owner()),
            Duration::from_millis(300),
        ));
        assert!(message.contains("issuance outcome is unknown"), "{message}");
        assert!(!fixture.bundle("worker").exists(), "{message}");
    }

    #[test]
    fn a_present_credential_for_another_agent_is_refused() {
        let fixture = Fixture::new("ok");
        fs::create_dir_all(fixture.agents().join("worker")).unwrap();
        fs::write(fixture.bundle("worker"), r#"{"agent":"someone-else"}"#).unwrap();
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("does not name 'worker'"), "{message}");
        assert_eq!(fixture.calls(), Vec::<String>::new());
    }

    #[test]
    fn concurrent_sessions_issue_one_credential() {
        let fixture = Arc::new(Fixture::new("slow"));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let fixture = Arc::clone(&fixture);
                std::thread::spawn(move || {
                    fixture
                        .enroll("shared-name")
                        .map_err(|error| error.to_string())
                })
            })
            .collect();
        let mut outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap().unwrap())
            .collect();
        outcomes.sort_by_key(|outcome| *outcome == AgentEnrollment::Joined);
        assert_eq!(
            outcomes,
            [AgentEnrollment::AlreadyPresent, AgentEnrollment::Joined]
        );
        assert_eq!(fixture.calls().len(), 1);
    }
}
