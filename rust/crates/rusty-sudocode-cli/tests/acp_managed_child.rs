//! `scode acp managed`, the process a host starts for one managed agent, run
//! as the real binary over piped stdio. Every journey here is refused before
//! the first Nexus request and nothing listens anywhere, so a refusal cannot
//! have been reached by contacting a node.
//!
//! The ignored journey is the same process serving its sessions against the
//! authenticated `nexusd-cluster` that `e2e/nexus-a2a/run-managed-subprocess.sh`
//! provides (`NEXUS_A2A_TEST_ENDPOINT`, `NEXUS_A2A_TEST_CERT_DIR`).

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kernel::core::agents::registry::{AgentDescriptor, AgentKind};
use managed_agent::bootstrap::{
    read_reply, write_bootstrap, BootstrapConnection, BootstrapFailure, BootstrapKind,
    BootstrapReply, ManagedBootstrap,
};
use runtime::nexus_mailbox::{AgentCredential, Config};
use runtime::{FsBackend, NexusVfsFsBackend};
use serde_json::{json, Value};

const AGENT: &str = "managed-child-agent";
/// Written into every credential file a journey creates, so a refusal that
/// echoed credential bytes would show it.
const SECRET: &str = "W7-MANAGED-CHILD-SECRET-MATERIAL";

struct Journey {
    home: tempfile::TempDir,
    child: Child,
}

impl Journey {
    fn start() -> Self {
        let home = tempfile::Builder::new()
            .prefix("managed-child-home-")
            .tempdir()
            .expect("home");
        let child = Command::new(env!("CARGO_BIN_EXE_scode"))
            .args(["acp", "managed"])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("SUDO_CODE_CONFIG_HOME", home.path().join("config"))
            .env_remove("SUDOCODE_PLAN_FILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start scode acp managed");
        Self { home, child }
    }

    fn credentials(&self) -> PathBuf {
        self.home.path().join("credential")
    }

    /// The frame a host writes, aimed at this child and an address nothing
    /// listens on.
    fn bootstrap(&self) -> ManagedBootstrap {
        ManagedBootstrap {
            kind: BootstrapKind::V1,
            descriptor: AgentDescriptor {
                pid: "managed-child-pid".into(),
                name: AGENT.into(),
                kind: AgentKind::Managed,
                owner_id: "test-owner".into(),
                zone_id: "root".into(),
                ..AgentDescriptor::default()
            },
            child_os_pid: self.child.id(),
            connection: BootstrapConnection {
                endpoint: "127.0.0.1:9".into(),
                credential_directory: self.credentials(),
            },
            workspace_vfs_root: "/managed-child/workspace".into(),
            shell_root: None,
            authenticated_senders: true,
        }
    }

    /// Hand the child `frame` (or nothing), then read its one reply and exit.
    fn refuse(mut self, frame: Option<&ManagedBootstrap>) -> (BootstrapReply, String, String) {
        let mut stdin = self.child.stdin.take().expect("stdin");
        if let Some(frame) = frame {
            write_bootstrap(&mut stdin, frame).expect("write the bootstrap");
        }
        drop(stdin);
        let mut stdout = BufReader::new(self.child.stdout.take().expect("stdout"));
        let reply = read_reply(&mut stdout).expect("one typed reply");
        let mut rest = String::new();
        stdout
            .read_to_string(&mut rest)
            .expect("stdout after the reply");
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                break status;
            }
            assert!(Instant::now() < deadline, "the refused child did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("stderr");
        assert!(!status.success(), "a refused bootstrap exits non-zero");
        (reply, rest, stderr)
    }
}

fn refused(failure: BootstrapFailure) -> BootstrapReply {
    BootstrapReply::Error { message: failure }
}

fn write_manifest(dir: &Path, agent: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let manifest = serde_json::json!({
        "agent": agent,
        "server_name": "nexus",
        "ca": "ca.pem",
        "cert": "agent.pem",
        "key": "agent-key.pem",
    });
    std::fs::write(dir.join("credential.json"), manifest.to_string()).unwrap();
}

fn assert_refusal(
    (reply, rest, stderr): (BootstrapReply, String, String),
    failure: BootstrapFailure,
) {
    assert_eq!(reply, refused(failure), "stderr:\n{stderr}");
    assert!(
        rest.is_empty(),
        "nothing follows a refusal on stdout: {rest:?}"
    );
    assert!(
        !stderr.contains(SECRET),
        "credential bytes never reach the diagnostics: {stderr}"
    );
}

#[test]
fn a_child_whose_host_sends_nothing_is_refused() {
    let journey = Journey::start();
    assert_refusal(journey.refuse(None), BootstrapFailure::InvalidPrelude);
}

#[test]
fn a_bootstrap_aimed_at_another_process_is_refused() {
    let journey = Journey::start();
    let mut frame = journey.bootstrap();
    frame.child_os_pid += 1;
    assert_refusal(
        journey.refuse(Some(&frame)),
        BootstrapFailure::InvalidPrelude,
    );
}

#[test]
fn a_bootstrap_without_its_credential_bundle_is_refused() {
    let journey = Journey::start();
    let frame = journey.bootstrap();
    assert_refusal(
        journey.refuse(Some(&frame)),
        BootstrapFailure::CredentialRefused,
    );
}

#[test]
fn a_credential_for_another_agent_is_refused() {
    let journey = Journey::start();
    write_manifest(&journey.credentials(), "some-other-agent");
    let frame = journey.bootstrap();
    assert_refusal(
        journey.refuse(Some(&frame)),
        BootstrapFailure::CredentialRefused,
    );
}

#[test]
fn a_credential_whose_keys_cannot_be_read_is_refused() {
    let journey = Journey::start();
    write_manifest(&journey.credentials(), AGENT);
    let frame = journey.bootstrap();
    assert_refusal(
        journey.refuse(Some(&frame)),
        BootstrapFailure::ConnectionRefused,
    );
}

#[test]
fn credential_bytes_never_appear_in_a_refusal() {
    let journey = Journey::start();
    let dir = journey.credentials();
    write_manifest(&dir, AGENT);
    for file in ["ca.pem", "agent.pem", "agent-key.pem"] {
        let mut out = std::fs::File::create(dir.join(file)).unwrap();
        writeln!(
            out,
            "-----BEGIN PRIVATE KEY-----\n{SECRET}\n-----END PRIVATE KEY-----"
        )
        .unwrap();
    }
    let frame = journey.bootstrap();
    let (reply, rest, stderr) = journey.refuse(Some(&frame));
    assert!(
        matches!(
            reply,
            BootstrapReply::Error {
                message: BootstrapFailure::ConnectionRefused | BootstrapFailure::ContextRefused
            }
        ),
        "unusable keys or an unreachable node refuse the context: {reply:?}\n{stderr}"
    );
    assert!(
        rest.is_empty(),
        "nothing follows a refusal on stdout: {rest:?}"
    );
    assert!(
        !stderr.contains(SECRET),
        "credential bytes never reach the diagnostics: {stderr}"
    );
}

/// A served child: its stdin, and its stdout lines after the bootstrap reply.
struct Served {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Served {
    /// Hand `frame` and `first` in one write, so the request sits in the same
    /// buffer as the bootstrap line, then read the bootstrap reply.
    fn start(
        mut journey: Journey,
        frame: &ManagedBootstrap,
        first: &Value,
    ) -> (Self, tempfile::TempDir) {
        let mut stdin = journey.child.stdin.take().expect("stdin");
        let mut bytes = Vec::new();
        write_bootstrap(&mut bytes, frame).expect("encode the bootstrap");
        bytes.extend_from_slice(format!("{first}\n").as_bytes());
        stdin
            .write_all(&bytes)
            .expect("write the bootstrap and the first request");
        stdin.flush().unwrap();
        let mut stdout = BufReader::new(journey.child.stdout.take().expect("stdout"));
        let reply = read_reply(&mut stdout).expect("one typed reply");
        assert_eq!(reply, BootstrapReply::Ready, "the bootstrap is accepted");
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines() {
                let Ok(line) = line else { return };
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        let Journey { home, child } = journey;
        (
            Self {
                child,
                stdin: Some(stdin),
                lines,
            },
            home,
        )
    }

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("connection open");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn response(&mut self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to {id}"));
            let message: Value = serde_json::from_str(&line).expect("the child writes JSON-RPC");
            if message["id"] == id {
                return message;
            }
        }
    }

    /// The host ends its side; the child exits once its sessions are closed.
    fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().expect("child status") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the child outlived its host connection"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn write_config(dir: &Path, base_url: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let config = json!({
        "auth_modes": {"api-key": {"anthropic": {"baseUrl": base_url, "apiKey": "unused"}}},
        "models": {"remote-test": {"alias": "remote-test", "name": "remote test", "input": ["text"],
            "providers": {"api-key": {"provider": "anthropic", "model": "claude-sonnet-4-6"}}}}
    });
    std::fs::write(dir.join("sudocode.json"), config.to_string()).unwrap();
}

#[test]
#[ignore = "requires the authenticated daemon from run-managed-subprocess.sh"]
fn a_managed_child_serves_its_sessions_on_its_daemon() {
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT");
    let bundle = std::env::var("NEXUS_A2A_TEST_CERT_DIR").expect("set NEXUS_A2A_TEST_CERT_DIR");
    let credential = AgentCredential::load(&bundle).expect("the session bundle");
    let agent = credential.agent.clone();
    let client = Config {
        endpoint: endpoint.clone(),
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .expect("the test's own connection");
    // What the daemon holds, read over the same credential but outside the child.
    let daemon: Arc<dyn FsBackend> = Arc::new(NexusVfsFsBackend::from_arc_mtls(
        client,
        String::new(),
        "root",
    ));
    let fresh = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let workspace = format!("/managed-child-{fresh}/workspace");
    let aimed = |journey: &Journey| {
        let mut frame = journey.bootstrap();
        frame.descriptor.pid = format!("managed-child-{fresh}");
        frame.descriptor.name.clone_from(&agent);
        frame
            .descriptor
            .labels
            .insert("model".into(), "remote-test".into());
        frame.connection = BootstrapConnection {
            endpoint: endpoint.clone(),
            credential_directory: PathBuf::from(&bundle),
        };
        frame.workspace_vfs_root.clone_from(&workspace);
        let shell = journey.home.path().join("shell");
        std::fs::create_dir_all(&shell).unwrap();
        frame.shell_root = Some(shell);
        frame
    };
    let sessions = || {
        daemon
            .readdir("/sessions")
            .map_or(0, |entries| entries.len())
    };

    // A workspace the daemon does not answer for starts nothing, not even a
    // sessions root.
    let before = sessions();
    let journey = Journey::start();
    let frame = aimed(&journey);
    assert_refusal(
        journey.refuse(Some(&frame)),
        BootstrapFailure::ContextRefused,
    );
    assert_eq!(
        sessions(),
        before,
        "a refused child creates no session storage"
    );

    // Served: the first request rides in the bootstrap's buffer.
    daemon.create_dir_all(&workspace).unwrap();
    let journey = Journey::start();
    write_config(&journey.home.path().join("config"), "nexus:///model");
    let frame = aimed(&journey);
    let shell = frame.shell_root.clone().unwrap();
    let initialize = json!({"jsonrpc": "2.0", "id": "init", "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}});
    let (mut served, home) = Served::start(journey, &frame, &initialize);
    let init = served.response("init");
    assert!(
        init.get("result").is_some(),
        "the buffered request is answered: {init}"
    );
    served.send(
        &json!({"jsonrpc": "2.0", "id": "new", "method": "session/new",
        "params": {"cwd": shell.to_string_lossy(), "mcpServers": []}}),
    );
    let created = served.response("new");
    let session_id = created["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new: {created}"))
        .to_owned();
    assert!(
        daemon
            .exists(&format!("/sessions/{session_id}/transcript.jsonl"))
            .unwrap(),
        "the child's transcript lives on the daemon"
    );
    assert!(
        !shell.join(".scode").exists() && !home.path().join(".scode").exists(),
        "nothing is stored on the child's local disk"
    );
    let status = served.close();
    assert!(
        status.success(),
        "a host-closed child with confirmed cleanup exits cleanly: {status}"
    );
}
