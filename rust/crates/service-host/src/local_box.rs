//! Explicit OS-user ownership of an existing authenticated Node. A client Box
//! record never grants process ownership. Resume uses the Node's native boot
//! contract and existing persistence, with no bootstrap or upgrade operation.

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime};

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

    fn list_credentials_command(&self) -> Command {
        let mut command = self.node_process();
        command
            .args(["--bind-addr", &self.local_endpoint.to_string()])
            .args(["auth", "list"])
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
/// Bounds asking the Node which credentials it recorded.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds waiting for another session's issuance on the same Box: its whole
/// worst case, a leftover marker's wait, two listings and one issuance.
const ENROLLMENT_LOCK_TIMEOUT: Duration =
    Duration::from_secs(MINT_TIMEOUT.as_secs() * 2 + LIST_TIMEOUT.as_secs() * 2 + 30);
/// A certificate may predate the issuance that wrote it by this much clock skew.
const ISSUANCE_SKEW: Duration = Duration::from_secs(600);

/// Issue `agent`'s credential through this OS user's registered local Box.
///
/// Only the Box this user owns, as named by the machine's node record, may
/// issue; anything else is an error naming the owner to ask instead. Nothing is
/// issued for a name that already has a bundle, and the Node refuses a name
/// already claimed elsewhere in its cluster, so this never takes over an Agent.
/// An issuance that does not confirm itself is kept only when the Node lists
/// the Agent and the bundle is this Box's own credential for it, issued during
/// that issuance; anything else is moved out of the credential directory, so
/// it never becomes the session's identity.
///
/// # Errors
/// No registered or matching local Box, a Box that is not running, an invalid
/// name, a Node refusal, or an issuance that could not be confirmed.
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
    // Windows device names stay devices with any extension: `nul`, `com1.txt`.
    let stem = agent
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let port: Vec<char> = stem.chars().collect();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && port.len() == 4
            && matches!(port[3], '0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'));
    !agent.is_empty()
        && !device
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
    // Shared with runtime's `nexus_mailbox::joining_marker`: while it exists,
    // no session uses the bundle.
    let marker = agents_dir.join(format!(".joining-{agent}"));
    let lock = super::supervisor::lock_file(&root, "enrollment.lock")?;
    let waiting = Instant::now();
    while fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        if waiting.elapsed() >= ENROLLMENT_LOCK_TIMEOUT {
            return Err("another session is still enrolling through this local Box".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if marker.exists() {
        if let Some(joined) = settle_interrupted(&profile, agent, &bundle, &marker, timeout)? {
            return Ok(joined);
        }
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
    fs::create_dir_all(agents_dir)?;
    fs::write(&marker, std::process::id().to_string())?;
    let issued = issue(&profile, agent, &bundle, timeout);
    // A bundle that could not be moved aside stays blocked by the marker.
    if issued.is_ok() || !bundle.exists() {
        if let Err(error) = clear_marker(&marker) {
            let outcome = match &issued {
                Ok(_) => "the credential was confirmed".to_string(),
                Err(cause) => cause.to_string(),
            };
            return Err(blocked(&marker, &outcome, &error));
        }
    }
    issued
}

/// Settle what an earlier issuance left behind its marker. `Some` when that
/// issuance had in fact joined; `None` when the name can be issued now.
fn settle_interrupted(
    profile: &LocalBox,
    agent: &str,
    bundle: &Path,
    marker: &Path,
    timeout: Duration,
) -> Result<Option<AgentEnrollment>> {
    // An earlier issuance stopped (its session quit or was killed) before
    // it confirmed what it wrote. Its Node child may outlive it, so give
    // that child the issuance's own bound to finish first. Then the Node's
    // record decides: keep a whole bundle it recorded, set aside one it
    // never did and issue again.
    let written = fs::metadata(marker)
        .and_then(|meta| meta.modified())
        .unwrap_or_else(|_| SystemTime::now());
    let age = written.elapsed().unwrap_or_default();
    if let Some(rest) = timeout.checked_sub(age) {
        std::thread::sleep(rest);
    }
    if bundle.exists() {
        match recorded(profile, agent) {
            Ok(true) if issued_here(profile, bundle, agent, written) => {
                clear_marker(marker).map_err(|error| {
                    blocked(marker, "the node had recorded the earlier join", &error)
                })?;
                return Ok(Some(AgentEnrollment::Joined));
            }
            Ok(true) => {
                let moved = quarantine(
                    profile,
                    bundle,
                    &format!("an earlier join for '{agent}' did not finish"),
                );
                return Err(held_name(profile, agent, &moved.to_string()));
            }
            Ok(false) => {
                let error = quarantine(
                    profile,
                    bundle,
                    &format!(
                        "an earlier join for '{agent}' did not finish, and the node never \
                         recorded it"
                    ),
                );
                if bundle.exists() {
                    return Err(error);
                }
                clear_marker(marker).map_err(|error| {
                    blocked(marker, "what an earlier join wrote was set aside", &error)
                })?;
            }
            Err(error) => {
                return Err(format!(
                    "an earlier join for '{agent}' did not finish; what it wrote stays unused \
                     until the node can be asked about it: {error}"
                )
                .into());
            }
        }
    } else {
        clear_marker(marker)
            .map_err(|error| blocked(marker, "an earlier join wrote nothing", &error))?;
    }
    Ok(None)
}

/// Remove the issuance marker. Windows refuses briefly while a scanner or a
/// reader holds the file.
fn clear_marker(marker: &Path) -> std::io::Result<()> {
    let mut attempts = 0;
    loop {
        match fs::remove_file(marker) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            done => return done,
        }
    }
}

/// A marker that could not be removed keeps the bundle from every session until
/// the next join settles it with the Node.
fn blocked(
    marker: &Path,
    outcome: &str,
    error: &std::io::Error,
) -> Box<dyn std::error::Error + Send + Sync> {
    format!(
        "{outcome}, but {} could not be removed ({error}); sessions wait for the next \
         join to settle it with the node",
        marker.display()
    )
    .into()
}

/// Run the Node's issuance and accept only a confirmed result; anything else
/// it wrote is moved aside.
fn issue(
    profile: &LocalBox,
    agent: &str,
    bundle: &Path,
    timeout: Duration,
) -> Result<AgentEnrollment> {
    let started = SystemTime::now();
    let output = match run_bounded(profile.mint_agent_command(agent), timeout) {
        Ok(output) => output,
        Err(error) => return settle(profile, agent, bundle, &error.to_string(), started),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let refused = quarantine(
            profile,
            bundle,
            &format!(
                "the node refused to issue '{agent}': {}",
                last_lines(&stderr)
            ),
        );
        return Err(held_name(profile, agent, &refused.to_string()));
    }
    if !online_receipt(&stderr, agent, profile.local_endpoint) {
        let cause = format!("the node returned no online receipt for '{agent}'");
        // An explicit offline signature never joins the running Box.
        if stderr.to_ascii_lowercase().contains("offline") {
            return Err(quarantine(profile, bundle, &cause));
        }
        return settle(profile, agent, bundle, &cause, started);
    }
    if !manifest_names(bundle, agent) {
        return Err(quarantine(
            profile,
            bundle,
            &format!("the credential written for '{agent}' does not name it"),
        ));
    }
    Ok(AgentEnrollment::Joined)
}

/// One line naming exactly this subject and exactly this endpoint after `via`.
/// An offline cold-start signature names no endpoint; a fallback warning on
/// another line does not count.
fn online_receipt(stderr: &str, agent: &str, endpoint: SocketAddr) -> bool {
    // Derived names keep the folder name, spaces included, so the subject is
    // matched as a phrase rather than as a whitespace-separated token.
    let receipt = format!("subject=agent:{agent} via https://{endpoint}");
    stderr.lines().any(|line| {
        line.match_indices(&receipt).any(|(at, _)| {
            let before = line[..at].chars().next_back();
            let after = line[at + receipt.len()..].chars().next();
            before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace)
        })
    })
}

/// An issuance that did not confirm itself (it timed out, or its receipt was
/// unfamiliar) still joined if the Node recorded this Agent and the bundle is
/// whole; anything else is set aside.
fn settle(
    profile: &LocalBox,
    agent: &str,
    bundle: &Path,
    cause: &str,
    since: SystemTime,
) -> Result<AgentEnrollment> {
    if !bundle.exists() {
        return Err(cause.into());
    }
    match recorded(profile, agent) {
        Ok(true) if issued_here(profile, bundle, agent, since) => Ok(AgentEnrollment::Joined),
        Ok(true) => Err(held_name(
            profile,
            agent,
            &quarantine(profile, bundle, cause).to_string(),
        )),
        Ok(false) => Err(quarantine(profile, bundle, cause)),
        // Neither kept nor set aside: the marker blocks it until a join can ask.
        Err(error) => Err(format!(
            "{cause}; what it wrote stays unused until the node can be asked about it: {error}"
        )
        .into()),
    }
}

/// The node holds `agent` but this machine has no credential it can use: only
/// the node owner can issue the name again.
fn held_name(
    profile: &LocalBox,
    agent: &str,
    cause: &str,
) -> Box<dyn std::error::Error + Send + Sync> {
    format!(
        "{cause}. If the node already holds '{agent}' for this workspace, its owner can issue it \
         again with {}; otherwise ask the owner for a different name and set it as `agentName`",
        owner_command(
            profile,
            &format!("auth mint --subject-type agent \"--subject-id={agent}\" --allow-existing")
        )
    )
    .into()
}

/// `tail` as a command for the Box's own Node, quoted where a path needs it.
fn owner_command(profile: &LocalBox, tail: &str) -> String {
    let shown = |path: &Path| {
        let path = path.display().to_string();
        if path.contains(char::is_whitespace) {
            format!("\"{path}\"")
        } else {
            path
        }
    };
    format!(
        "`{} --data-dir {} --identity-dir {} --bind-addr {} {tail}`",
        shown(&profile.binary),
        shown(&profile.data_dir),
        shown(&profile.identity_dir),
        profile.local_endpoint
    )
}

/// Whether `bundle` is this Box's own credential for `agent`, issued no earlier
/// than `since`: the Box's CA, a certificate for `nexus://agent/<agent>` and a
/// private key. The Node's listing alone proves only that it holds the name.
fn issued_here(profile: &LocalBox, bundle: &Path, agent: &str, since: SystemTime) -> bool {
    let Some(manifest) = fs::read(bundle.join("credential.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|manifest| manifest["agent"] == agent)
    else {
        return false;
    };
    let file = |field: &str| manifest[field].as_str().map(|name| bundle.join(name));
    let (Some(ca), Some(cert), Some(key)) = (file("ca"), file("cert"), file("key")) else {
        return false;
    };
    let own_ca = digest(&ca).is_ok_and(|ca| ca == profile.ca_sha256);
    // A whole PEM private-key block; a truncated or empty file is not one.
    let key = fs::read(&key).is_ok_and(|key| {
        x509_parser::pem::parse_x509_pem(&key)
            .is_ok_and(|(_, pem)| pem.label.ends_with("PRIVATE KEY") && !pem.contents.is_empty())
    });
    let Ok(cert) = fs::read(&cert) else {
        return false;
    };
    let Ok((_, pem)) = x509_parser::pem::parse_x509_pem(&cert) else {
        return false;
    };
    let Ok(cert) = pem.parse_x509() else {
        return false;
    };
    let names_agent = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .is_some_and(|names| {
            names.value.general_names.iter().any(|name| {
                matches!(name, x509_parser::extensions::GeneralName::URI(uri)
                if *uri == format!("nexus://agent/{agent}"))
            })
        });
    let earliest = since
        .checked_sub(ISSUANCE_SKEW)
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_secs());
    let recent = u64::try_from(cert.validity().not_before.timestamp())
        .is_ok_and(|issued| issued >= earliest);
    own_ca && key && names_agent && recent
}

/// Whether the running Node lists `agent:<agent>` among its credentials.
fn recorded(profile: &LocalBox, agent: &str) -> Result<bool> {
    let output = run_bounded(profile.list_credentials_command(), LIST_TIMEOUT)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "the node did not list its credentials: {}",
            last_lines(&stderr)
        )
        .into());
    }
    // `agent:<name>  agent:<name>  admin=…`: columns are two spaces apart and a
    // name may itself contain spaces, so match both name columns.
    let subject = format!("agent:{agent}  agent:{agent}  ");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .chain(stderr.lines())
        .any(|line| line.starts_with(&subject)))
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
fn quarantine(
    profile: &LocalBox,
    bundle: &Path,
    cause: &str,
) -> Box<dyn std::error::Error + Send + Sync> {
    if !bundle.exists() {
        return cause.to_string().into();
    }
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let name = bundle
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let held = bundle.with_file_name(format!(".unconfirmed-{name}-{stamp}"));
    let check = owner_command(profile, "auth list");
    match fs::rename(bundle, &held) {
        Ok(()) => format!(
            "{cause}. What it wrote was moved to {} and is not used; {check} shows whether \
             the node holds agent:{name}",
            held.display()
        )
        .into(),
        Err(error) => format!(
            "{cause}. What it wrote at {} could not be moved aside ({error}) and stays \
             blocked from use; the node owner can check it with {check}",
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
set /p MODE=<"%HERE%mode.txt"
if "%~8"=="list" goto list
shift & shift & shift & shift & shift & shift & shift & shift & shift & shift
rem cmd may split "--subject-id=NAME" at "=" when it is not quoted.
set "NAME=%~2"
if "%NAME%"=="" for /f "tokens=2 delims==" %%a in ("%~1") do set "NAME=%%a"
if "%MODE%"=="slow" ping -n 3 127.0.0.1 >nul
if "%MODE%"=="refuse" goto refuse
rem Like the Node: a name it already holds is refused, and nothing is written.
if exist "%HERE%recorded.txt" findstr /b /l /c:"agent:%NAME%  agent:%NAME%  " "%HERE%recorded.txt" >nul && goto refuse
set "OWNER=%NAME%"
if "%MODE%"=="wrong" set "OWNER=someone-else"
mkdir "%DATA%\agents\%NAME%"
>"%DATA%\agents\%NAME%\credential.json" echo {"agent":"%OWNER%","ca":"ca.pem","cert":"agent.pem","key":"agent-key.pem"}
if exist "%HERE%issue\%NAME%\agent.pem" (
  for %%f in (ca.pem agent.pem agent-key.pem) do copy /y "%HERE%issue\%NAME%\%%f" "%DATA%\agents\%NAME%\%%f" >nul
) else (
  for %%f in (ca.pem agent.pem agent-key.pem) do >"%DATA%\agents\%NAME%\%%f" echo pem
)
if "%MODE%"=="fallback" (
  echo warning: no answer via https://%BIND% 1>&2
  echo minted agent cert subject=agent:%NAME% -^> offline 1>&2
  exit /b 0
)
if "%MODE%"=="noreceipt" (
  echo minted agent cert subject=agent:%NAME% -^> offline 1>&2
  exit /b 0
)
>>"%HERE%recorded.txt" echo agent:%NAME%  agent:%NAME%  admin=false
if "%MODE%"=="unfamiliar" goto unfamiliar
if "%MODE%"=="unfamiliar-nolist" goto unfamiliar
echo minted agent cert subject=agent:%NAME% via https://%BIND% -^> written 1>&2
exit /b 0
:unfamiliar
echo issued agent:%NAME% at https://%BIND% 1>&2
exit /b 0
:refuse
echo error: agent:%NAME% is already registered 1>&2
exit /b 1
:list
if "%MODE%"=="nolist" exit /b 1
if "%MODE%"=="unfamiliar-nolist" exit /b 1
if exist "%HERE%recorded.txt" type "%HERE%recorded.txt"
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
if [ "$8" = list ]; then
  case "$mode" in nolist|unfamiliar-nolist) exit 1;; esac
  cat "$here/recorded.txt" 2>/dev/null; exit 0
fi
[ "$mode" = slow ] && sleep 2
refuse() { echo "error: agent:$name is already registered" >&2; exit 1; }
[ "$mode" = refuse ] && refuse
# Like the Node: a name it already holds is refused, and nothing is written.
grep -qF "agent:$name  agent:$name  " "$here/recorded.txt" 2>/dev/null && refuse
owner=$name; [ "$mode" = wrong ] && owner=someone-else
mkdir -p "$data/agents/$name"
printf '{"agent":"%s","ca":"ca.pem","cert":"agent.pem","key":"agent-key.pem"}\n' "$owner" > "$data/agents/$name/credential.json"
if [ -f "$here/issue/$name/agent.pem" ]; then
  for file in ca.pem agent.pem agent-key.pem; do cp "$here/issue/$name/$file" "$data/agents/$name/$file"; done
else
  for file in ca.pem agent.pem agent-key.pem; do echo pem > "$data/agents/$name/$file"; done
fi
if [ "$mode" = fallback ]; then echo "warning: no answer via https://$bind" >&2; echo "minted agent cert subject=agent:$name -> offline" >&2; exit 0; fi
if [ "$mode" = noreceipt ]; then echo "minted agent cert subject=agent:$name -> offline" >&2; exit 0; fi
echo "agent:$name  agent:$name  admin=false" >> "$here/recorded.txt"
case "$mode" in unfamiliar|unfamiliar-nolist) echo "issued agent:$name at https://$bind" >&2; exit 0;; esac
echo "minted agent cert subject=agent:$name via https://$bind -> written" >&2
"#,
    );

    /// A registered local Box whose "Node" is a script and whose listener is
    /// held by the test, so issuance runs without a real cluster.
    struct Fixture {
        dir: tempfile::TempDir,
        listener: Option<TcpListener>,
        endpoint: String,
        ca: (String, String),
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
            let ca_key = rcgen::KeyPair::generate().unwrap();
            let mut ca_params = rcgen::CertificateParams::default();
            ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            let ca = (
                ca_params.self_signed(&ca_key).unwrap().pem(),
                ca_key.serialize_pem(),
            );
            fs::write(data.join("tls/ca.pem"), &ca.0).unwrap();
            for (name, body) in [
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
            let fixture = Self {
                dir,
                listener: Some(listener),
                endpoint,
                ca,
            };
            for agent in ["worker", "My Project-1a2b3c"] {
                fixture.issue(agent);
            }
            fixture
        }

        /// What the Node would issue `agent` now: this Box's CA, a certificate
        /// for `nexus://agent/<agent>` and its key. The fake Node copies it.
        fn issue(&self, agent: &str) {
            let issuer = rcgen::Issuer::from_ca_cert_pem(
                &self.ca.0,
                rcgen::KeyPair::from_pem(&self.ca.1).unwrap(),
            )
            .unwrap();
            let key = rcgen::KeyPair::generate().unwrap();
            let mut params = rcgen::CertificateParams::default();
            params.not_before = time::OffsetDateTime::now_utc();
            params.subject_alt_names = vec![rcgen::SanType::URI(
                format!("nexus://agent/{agent}").try_into().unwrap(),
            )];
            let cert = params.signed_by(&key, &issuer).unwrap();
            let output = self.dir.path().join("bin/issue").join(agent);
            fs::create_dir_all(&output).unwrap();
            fs::write(output.join("ca.pem"), &self.ca.0).unwrap();
            fs::write(output.join("agent.pem"), cert.pem()).unwrap();
            fs::write(output.join("agent-key.pem"), key.serialize_pem()).unwrap();
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

        /// A whole bundle, as an issuance writes it, left behind with its marker.
        fn leave_unconfirmed(&self, agent: &str) -> PathBuf {
            let bundle = self.agents().join(agent);
            fs::create_dir_all(&bundle).unwrap();
            fs::write(
                bundle.join("credential.json"),
                json!({"agent": agent, "ca": "ca.pem", "cert": "agent.pem", "key": "agent-key.pem"})
                    .to_string(),
            )
            .unwrap();
            let issued = self.dir.path().join("bin/issue").join(agent);
            for file in ["ca.pem", "agent.pem", "agent-key.pem"] {
                match fs::read(issued.join(file)) {
                    Ok(bytes) => fs::write(bundle.join(file), bytes).unwrap(),
                    Err(_) => fs::write(bundle.join(file), "pem").unwrap(),
                }
            }
            let marker = self.agents().join(format!(".joining-{agent}"));
            fs::write(&marker, "1").unwrap();
            backdate(&marker);
            marker
        }

        fn record(&self, agent: &str) {
            fs::write(
                self.dir.path().join("bin/recorded.txt"),
                format!("agent:{agent}  agent:{agent}  admin=false\n"),
            )
            .unwrap();
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
        let outcome = fixture.enroll("worker-1a2b3c");
        std::env::remove_var("NEXUS_ENROLL_FIXTURE_LEAK");
        assert_eq!(outcome.unwrap(), AgentEnrollment::Joined);
        assert!(!fixture.agents().join(".joining-worker-1a2b3c").exists());
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
            "con", "NUL.txt", "com0", "com1", "Lpt9", "COM¹", "nul .txt",
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

    /// An issuance that stopped long enough ago that no Node child of it runs.
    fn backdate(path: &Path) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(600))
            .unwrap();
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
    fn a_fallback_warning_naming_the_endpoint_is_not_a_receipt() {
        let fixture = Fixture::new("fallback");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("no online receipt"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
    }

    #[test]
    fn an_interrupted_issuance_the_node_recorded_is_kept() {
        let fixture = Fixture::new("ok");
        let marker = fixture.leave_unconfirmed("worker");
        fixture.record("worker");
        assert_eq!(fixture.enroll("worker").unwrap(), AgentEnrollment::Joined);
        assert!(!marker.exists());
        assert!(fixture.bundle("worker").exists());
        let calls = fixture.calls();
        assert!(
            calls.len() == 1 && calls[0].contains("auth list"),
            "{calls:?}"
        );
    }

    #[test]
    fn an_interrupted_issuance_the_node_never_recorded_is_set_aside_and_issued_again() {
        let fixture = Fixture::new("ok");
        let marker = fixture.leave_unconfirmed("worker");
        assert_eq!(fixture.enroll("worker").unwrap(), AgentEnrollment::Joined);
        assert!(!marker.exists());
        assert!(fixture.bundle("worker").exists());
        let held = fs::read_dir(fixture.agents())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".unconfirmed-worker-")
            });
        assert!(held);
        let calls = fixture.calls();
        assert!(
            calls.len() == 2 && calls[0].contains("auth list") && calls[1].contains("auth mint"),
            "{calls:?}"
        );
    }

    #[test]
    fn an_interrupted_issuance_stays_unused_while_the_node_cannot_be_asked() {
        let fixture = Fixture::new("nolist");
        let marker = fixture.leave_unconfirmed("worker");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("stays unused"), "{message}");
        assert!(marker.exists() && fixture.bundle("worker").exists());
    }

    #[test]
    fn a_partial_bundle_is_not_kept_even_when_the_name_is_recorded() {
        let fixture = Fixture::new("ok");
        fixture.leave_unconfirmed("worker");
        fs::remove_file(fixture.agents().join("worker").join("agent.pem")).unwrap();
        fixture.record("worker");
        let message = error(fixture.enroll("worker"));
        // The Node would refuse the held name, so it is not issued again.
        assert!(message.contains("--allow-existing"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
        let calls = fixture.calls();
        assert!(
            calls.len() == 1 && calls[0].contains("auth list"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_bundle_with_a_truncated_key_is_not_kept() {
        let fixture = Fixture::new("ok");
        fixture.leave_unconfirmed("worker");
        let key = fixture.agents().join("worker").join("agent-key.pem");
        let whole = fs::read_to_string(&key).unwrap();
        fs::write(&key, &whole[..whole.len() / 2]).unwrap();
        fixture.record("worker");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("--allow-existing"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
    }

    #[test]
    fn a_bundle_from_another_ca_is_not_kept() {
        let fixture = Fixture::new("ok");
        fixture.leave_unconfirmed("worker");
        let other = rcgen::CertificateParams::default()
            .self_signed(&rcgen::KeyPair::generate().unwrap())
            .unwrap();
        fs::write(fixture.agents().join("worker").join("ca.pem"), other.pem()).unwrap();
        fixture.record("worker");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("--allow-existing"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
    }

    #[test]
    fn an_explicit_offline_issuance_is_never_kept() {
        let fixture = Fixture::new("noreceipt");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("no online receipt"), "{message}");
        assert_quarantined(&fixture, "worker", &message);
        let calls = fixture.calls();
        assert!(
            calls.iter().all(|call| !call.contains("auth list")),
            "{calls:?}"
        );
    }

    #[test]
    fn an_unconfirmed_issuance_stays_unused_while_the_node_cannot_be_asked() {
        let fixture = Fixture::new("unfamiliar-nolist");
        let message = error(fixture.enroll("worker"));
        assert!(message.contains("stays unused"), "{message}");
        assert!(fixture.bundle("worker").exists(), "{message}");
        assert!(
            fixture.agents().join(".joining-worker").exists(),
            "{message}"
        );
    }

    #[test]
    fn a_name_with_a_space_is_issued_and_recognised() {
        // Derived names keep the folder name, spaces included.
        let fixture = Fixture::new("ok");
        let agent = "My Project-1a2b3c";
        assert_eq!(fixture.enroll(agent).unwrap(), AgentEnrollment::Joined);
        let marker = fixture.agents().join(format!(".joining-{agent}"));
        fs::write(&marker, "1").unwrap();
        backdate(&marker);
        // Settled by the Node's list, which must find the spaced name too.
        assert_eq!(fixture.enroll(agent).unwrap(), AgentEnrollment::Joined);
        assert!(!marker.exists() && fixture.bundle(agent).exists());
        let calls = fixture.calls();
        // One issuance confirmed by its receipt, then one list.
        assert!(
            calls.len() == 2 && calls[0].contains("auth mint") && calls[1].contains("auth list"),
            "{calls:?}"
        );
    }

    #[test]
    fn a_fresh_marker_waits_for_an_issuance_that_may_still_be_running() {
        let fixture = Fixture::new("ok");
        fs::write(fixture.agents().join(".joining-worker"), "1").unwrap();
        let started = Instant::now();
        let outcome = enroll_with(
            "worker",
            &fixture.endpoint,
            &fixture.agents(),
            Some(&fixture.owner()),
            Duration::from_millis(4000),
        );
        assert_eq!(outcome.unwrap(), AgentEnrollment::Joined);
        assert!(
            started.elapsed() >= Duration::from_millis(3000),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_receipt_names_exactly_this_agent_and_endpoint() {
        let endpoint: SocketAddr = "127.0.0.1:2026".parse().unwrap();
        let receipt = |stderr: &str| online_receipt(stderr, "ws-1a2b3c", endpoint);
        assert!(receipt(
            "minted agent cert subject=agent:ws-1a2b3c via https://127.0.0.1:2026 -> C:\\agents"
        ));
        assert!(!receipt(
            "minted agent cert subject=agent:ws-1a2b3c via https://127.0.0.1:20261 -> x"
        ));
        assert!(!receipt(
            "minted agent cert subject=agent:ws-1a2b3cd via https://127.0.0.1:2026 -> x"
        ));
        assert!(!receipt(
            "warning: no answer via https://127.0.0.1:2026\nminted agent cert subject=agent:ws-1a2b3c -> offline"
        ));
    }

    #[test]
    fn a_recorded_issuance_with_an_unfamiliar_receipt_is_kept() {
        let fixture = Fixture::new("unfamiliar");
        assert_eq!(fixture.enroll("worker").unwrap(), AgentEnrollment::Joined);
        assert!(fixture.bundle("worker").exists());
        assert!(!fixture.agents().join(".joining-worker").exists());
    }

    #[test]
    fn a_marker_without_a_bundle_does_not_block_issuing() {
        let fixture = Fixture::new("ok");
        let marker = fixture.agents().join(".joining-worker");
        fs::write(&marker, "1").unwrap();
        backdate(&marker);
        assert_eq!(fixture.enroll("worker").unwrap(), AgentEnrollment::Joined);
        assert!(!marker.exists());
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
