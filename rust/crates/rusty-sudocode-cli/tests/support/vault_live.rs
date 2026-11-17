//! A real confined nexus runtime for the live Vault acceptance: the released
//! `nexusd-cluster serve-user` with the signed Vault plugin, credentials from
//! nexus's own certgen, and isolated scode homes that run the real
//! `scode vault tool` entry against it.
//!
//! `SUDOCODE_VAULT_LIVE` names a directory holding `nexusd-cluster` and
//! the platform's signed plugin library in `plugins/` (`libnexus_vault.so` or
//! `.dylib`, with its `.sig`), as `e2e/vault-live/lib.sh` lays it out.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nexus_raft::transport::{
    generate_session_agent_cert, generate_user_runtime_cert, generate_zone_ca,
    user_runtime_server_name, AgentCredential,
};
use nexus_vfs_client::NexusVfsClient;
use serde_json::{json, Value};
use service_host::vault_client::{self, proto};
use tempfile::TempDir;

use super::vault_tool::{self, ToolRun};

const READY: &str = "confined user runtime ready";
const LOG_FILTER: &str = "info,h2=warn,hyper=warn,tower=warn,tonic=warn";

/// One CA and a scratch root shared by every runtime and agent in a test.
pub struct Live {
    artifacts: PathBuf,
    root: TempDir,
    ca: Vec<u8>,
    ca_key: Vec<u8>,
}

/// A running `serve-user` process. Dropping it kills the process.
pub struct Runtime {
    child: Child,
    port: u16,
    log: PathBuf,
}

/// An isolated scode home whose user-layer settings point at a runtime.
pub struct Home {
    dir: PathBuf,
}

impl Live {
    /// The live fixture, or `None` when `SUDOCODE_VAULT_LIVE` is unset.
    pub fn from_env() -> Option<Self> {
        let artifacts = PathBuf::from(std::env::var_os("SUDOCODE_VAULT_LIVE")?);
        let (ca, ca_key) = generate_zone_ca("vault-live").expect("zone CA");
        Some(Self {
            artifacts,
            root: tempfile::tempdir().expect("scratch root"),
            ca,
            ca_key,
        })
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Start `serve-user` for `owner` on `data`, with or without the Vault plugin.
    pub fn start(&self, owner: &str, data: &Path, with_vault: bool) -> Runtime {
        let credential = self.path(&format!("runtime-{owner}"));
        if !credential.join("credential.json").exists() {
            let (cert, key) = generate_user_runtime_cert(
                &format!("runtime-{owner}"),
                owner,
                300,
                &self.ca,
                &self.ca_key,
            )
            .expect("runtime credential");
            self.write_bundle(&credential, &format!("runtime-{owner}"), owner, &cert, &key);
        }
        let port = free_port();
        let log = self.path(&format!("serve-user-{owner}-{port}.log"));
        let out = File::create(&log).expect("runtime log");
        let mut command = Command::new(self.artifacts.join("nexusd-cluster"));
        command.arg("--data-dir").arg(data);
        if with_vault {
            command
                .arg("--plugin-dir")
                .arg(self.artifacts.join("plugins"));
        }
        command
            .args([
                "--bind-addr",
                &format!("127.0.0.1:{port}"),
                "serve-user",
                "--owner",
                owner,
            ])
            .arg("--credential")
            .arg(&credential)
            .env("RUST_LOG", LOG_FILTER)
            .env("NEXUS_DATA_DIR", self.path("unrelated-data"))
            .stdin(Stdio::null())
            .stdout(out.try_clone().expect("log handle"))
            .stderr(out);
        let mut runtime = Runtime {
            child: command.spawn().expect("launch serve-user"),
            port,
            log,
        };
        runtime.wait_ready();
        runtime
    }

    /// Mint a session agent `subject` for `owner` into `dir`, naming `owner`'s
    /// runtime as its server. Returns `dir`.
    pub fn agent(&self, dir: &Path, subject: &str, owner: &str, validity_secs: u64) -> PathBuf {
        let (cert, key) =
            generate_session_agent_cert(subject, owner, validity_secs, &self.ca, &self.ca_key)
                .expect("session agent credential");
        self.write_bundle(dir, subject, owner, &cert, &key);
        dir.to_path_buf()
    }

    /// Store `value` at `namespace`/`key` through the Vault as an agent of
    /// `owner`: an RPC body, never a command line.
    pub fn seed(&self, runtime: &Runtime, owner: &str, namespace: &str, key: &str, value: &str) {
        let dir = self.agent(&self.path(&format!("seeder-{owner}")), "seeder", owner, 300);
        let credential = AgentCredential::load(&dir).expect("seeder bundle");
        let client = NexusVfsClient::connect_tls(
            &runtime.endpoint(),
            credential.ca_pem,
            credential.cert_pem,
            credential.key_pem,
            &credential.server_name,
        )
        .expect("seeder connection");
        let _: proto::PutSecretResponse = vault_client::call(
            &client,
            "secret_put",
            &proto::PutSecretRequest {
                namespace: namespace.to_owned(),
                key: key.to_owned(),
                value: value.to_owned(),
                description: None,
            },
        )
        .expect("seed the canary");
    }

    /// A fresh scode home named `name`.
    pub fn home(&self, name: &str) -> Home {
        let dir = self.path(name);
        for sub in ["home", "config", "project"] {
            fs::create_dir_all(dir.join(sub)).expect("home directory");
        }
        Home { dir }
    }

    fn write_bundle(&self, dir: &Path, subject: &str, owner: &str, cert: &[u8], key: &[u8]) {
        fs::create_dir_all(dir).expect("bundle directory");
        let mut manifest = AgentCredential::for_bundle(subject);
        manifest.server_name = user_runtime_server_name(owner).expect("runtime server name");
        fs::write(dir.join(&manifest.ca), &self.ca).expect("CA");
        fs::write(dir.join(&manifest.cert), cert).expect("certificate");
        fs::write(dir.join(&manifest.key), key).expect("key");
        manifest.write_to(dir).expect("manifest");
    }
}

impl Runtime {
    pub fn endpoint(&self) -> String {
        format!("https://127.0.0.1:{}", self.port)
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !self.log_text().contains(READY) {
            if let Some(status) = self.child.try_wait().expect("runtime status") {
                panic!("serve-user exited {status}: {}", self.log_text());
            }
            assert!(
                Instant::now() < deadline,
                "serve-user not ready: {}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Home {
    /// Write `vault` as this home's user-layer settings.
    pub fn configure(&self, vault: &Value) {
        fs::write(
            self.dir.join("config").join("settings.json"),
            json!({ "vault": vault }).to_string(),
        )
        .expect("user settings");
    }

    pub fn run(&self, tool: &str, input: &str) -> ToolRun {
        vault_tool::run(
            &self.dir.join("project"),
            &self.dir.join("home"),
            &self.dir.join("config"),
            tool,
            input,
        )
    }

    /// Each configured operation's availability, in order.
    pub fn availability(&self) -> Vec<String> {
        let run = self.run("list", "{}");
        run.result["operations"]
            .as_array()
            .unwrap_or_else(|| panic!("expected operations: {}", run.stdout))
            .iter()
            .map(|operation| {
                operation["availability"]
                    .as_str()
                    .expect("availability")
                    .to_owned()
            })
            .collect()
    }
}

/// A `vault` section for `owner` at `endpoint` with one http operation per
/// `refs` entry, aimed at `target` (an `http://127.0.0.1:<port>` origin).
pub fn section(
    endpoint: &str,
    owner: &str,
    credentials: &Path,
    target: &str,
    refs: &[&str],
) -> Value {
    let operations: Vec<Value> = refs
        .iter()
        .enumerate()
        .map(|(index, secret_ref)| {
            json!({
                "id": format!("op-{index}"), "kind": "http", "ref": secret_ref,
                "method": "POST", "origin": target, "path": "/v1/use",
                "header": "Authorization", "scheme": "Bearer", "redirect": "deny",
                "response": {"headers": [], "jsonFields": ["/ok"]}
            })
        })
        .collect();
    json!({
        "endpoint": endpoint,
        "owner": owner,
        "credentialDir": credentials,
        "operations": operations,
    })
}

/// Record one case's observed result for the evidence report.
pub fn record(case: &str, observed: &Value) {
    if let Some(out) = std::env::var_os("SUDOCODE_VAULT_LIVE_EVIDENCE") {
        let out = PathBuf::from(out);
        fs::create_dir_all(&out).expect("evidence directory");
        fs::write(out.join(format!("{case}.json")), observed.to_string()).expect("evidence file");
    }
}

/// A random canary value, unique to this run.
pub fn canary() -> String {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .expect("random canary");
    let mut value = String::from("vault-live-canary-");
    for byte in bytes {
        write!(value, "{byte:02x}").expect("format the canary");
    }
    value
}

/// Every process command line on this host that contains `needle`, as `ps`
/// reports them, which reads the same on Linux and macOS.
pub fn command_lines_containing(needle: &str) -> Vec<String> {
    let output = Command::new("ps")
        .args(["-axww", "-o", "args="])
        .output()
        .expect("run ps");
    assert!(output.status.success(), "ps failed: {output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains(needle))
        .map(str::to_owned)
        .collect()
}

/// Every file under `dir` whose bytes contain `needle`.
pub fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if fs::read(&path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes())
            }) {
                hits.push(path);
            }
        }
    }
    hits
}

/// An https endpoint on a loopback port nothing listens on.
pub fn closed_endpoint() -> String {
    format!("https://127.0.0.1:{}", free_port())
}

/// A loopback listener standing in for an operation's target, and its origin.
pub fn target() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the target");
    listener.set_nonblocking(true).expect("nonblocking target");
    let origin = format!("http://{}", listener.local_addr().expect("target address"));
    (listener, origin)
}

/// Whether anything connected to `listener`.
pub fn contacted(listener: &TcpListener) -> bool {
    listener.accept().is_ok()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("free port")
        .port()
}
