//! The compiled co-host retains conditional owner content across process restarts.
#![cfg(all(feature = "daemon", target_os = "linux"))]

use std::fs::File;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use lib::transport_primitives::{user_runtime_server_name, AgentCredential, LoadedCredential};
use nexus_raft::transport::{
    generate_session_agent_cert, generate_user_runtime_cert, generate_zone_ca,
};
use nexus_vfs_client::{ConditionalContentResult, NexusVfsClient};

const REGISTER: &str = "/__managed_content_v1__/register";

struct Daemon(Child);

impl Daemon {
    fn start(root: &Path, port: u16) -> Self {
        let log_path = root.join("daemon.log");
        let log = File::create(&log_path).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_nexusd-cohost"));
        for (name, _) in std::env::vars().filter(|(name, _)| name.starts_with("NEXUS_")) {
            command.env_remove(name);
        }
        let mut daemon = Self(
            command
                .args(["--data-dir"])
                .arg(root.join("data"))
                .args([
                    "--bind-addr",
                    &format!("127.0.0.1:{port}"),
                    "serve-user",
                    "--owner",
                    "alice",
                    "--credential",
                ])
                .arg(root.join("server"))
                .arg("--backend-config")
                .arg(root.join("backends.json"))
                .env("RUST_LOG", "info")
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let until = Instant::now() + Duration::from_secs(90);
        loop {
            let text = std::fs::read_to_string(&log_path).unwrap();
            if text.contains("confined user runtime ready") {
                return daemon;
            }
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "co-host exited: {text}"
            );
            assert!(Instant::now() < until, "co-host readiness expired: {text}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        self.0.wait().unwrap();
    }
}

fn bundle(root: &Path, actor: &str, ca: &[u8], cert: &[u8], key: &[u8]) -> LoadedCredential {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join("ca.pem"), ca).unwrap();
    std::fs::write(root.join("agent.pem"), cert).unwrap();
    std::fs::write(root.join("agent-key.pem"), key).unwrap();
    let mut credential = AgentCredential::for_bundle(actor);
    credential.server_name = user_runtime_server_name("alice").unwrap();
    credential.write_to(root).unwrap();
    AgentCredential::load(root).unwrap()
}

fn client(port: u16, credential: &LoadedCredential) -> NexusVfsClient {
    NexusVfsClient::connect_tls(
        &format!("https://127.0.0.1:{port}"),
        credential.ca_pem.clone(),
        credential.cert_pem.clone(),
        credential.key_pem.clone(),
        &credential.server_name,
    )
    .unwrap()
}

#[test]
fn owner_cas_survives_restart_and_rejects_stale_and_other_owner_writers() {
    let root = tempfile::tempdir().unwrap();
    let (ca, ca_key) = generate_zone_ca("root").unwrap();
    let (cert, key) =
        generate_user_runtime_cert("runtime-alice", "alice", 300, &ca, &ca_key).unwrap();
    bundle(
        &root.path().join("server"),
        "runtime-alice",
        &ca,
        &cert,
        &key,
    );
    let mut identities = Vec::new();
    for owner in ["alice", "bob"] {
        let actor = format!("client-{owner}");
        let (cert, key) = generate_session_agent_cert(&actor, owner, 300, &ca, &ca_key).unwrap();
        identities.push(bundle(&root.path().join(owner), &actor, &ca, &cert, &key));
    }
    std::fs::write(
        root.path().join("backends.json"),
        serde_json::to_vec(&serde_json::json!({"mounts": [{
            "path": "/__managed_content_v1__", "backend_type": "cas-local-durable",
            "backend_name": "managed-control", "backend_params": {"local_root": root.path().join("cas")}
        }]}))
        .unwrap(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let daemon = Daemon::start(root.path(), port);
    let owner = Arc::new(client(port, &identities[0]));
    assert!(owner.read_versioned(REGISTER, "").unwrap().is_none());
    let ConditionalContentResult::Committed { version } =
        owner.write_if_version(REGISTER, None, b"", "").unwrap()
    else {
        panic!("the absent register must accept its first image");
    };
    assert_eq!(
        owner.read_versioned(REGISTER, "").unwrap().unwrap().content,
        b""
    );
    let barrier = Arc::new(Barrier::new(2));
    let writers: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
        .into_iter()
        .map(|content| {
            let owner = owner.clone();
            let barrier = barrier.clone();
            let version = version.clone();
            std::thread::spawn(move || {
                barrier.wait();
                owner
                    .write_if_version(REGISTER, Some(&version), content, "")
                    .unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = writers
        .into_iter()
        .map(|writer| writer.join().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ConditionalContentResult::Committed { .. }))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ConditionalContentResult::Conflict { .. }))
            .count(),
        1
    );
    let committed = owner.read_versioned(REGISTER, "").unwrap().unwrap();
    assert!(committed.content == b"first" || committed.content == b"second");
    assert_ne!(committed.version, version);
    drop(owner);
    drop(daemon);

    let _daemon = Daemon::start(root.path(), port);
    let owner = client(port, &identities[0]);
    let restored = owner.read_versioned(REGISTER, "").unwrap().unwrap();
    assert_eq!(restored.content, committed.content);
    assert_eq!(restored.version, committed.version);
    assert!(matches!(
        owner
            .write_if_version(REGISTER, Some(&version), b"stale", "")
            .unwrap(),
        ConditionalContentResult::Conflict { .. }
    ));
    let outsider = client(port, &identities[1]);
    assert!(outsider.read_versioned(REGISTER, "").is_err());
    assert!(outsider
        .write_if_version(REGISTER, Some(&restored.version), b"forbidden", "")
        .is_err());
    let final_image = owner.read_versioned(REGISTER, "").unwrap().unwrap();
    assert_eq!(final_image.content, committed.content);
    assert_eq!(final_image.version, committed.version);
}
