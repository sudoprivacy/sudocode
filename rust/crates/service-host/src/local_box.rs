//! Explicit OS-user ownership of an existing authenticated Node. A client Box
//! record never grants process ownership. Resume uses the Node's native boot
//! contract and existing persistence, with no bootstrap or upgrade operation.

use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;

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
            .arg(&self.identity_dir)
            .args([
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
