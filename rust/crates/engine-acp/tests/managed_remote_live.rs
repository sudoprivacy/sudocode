//! A managed agent served from one remote context, against a real
//! authenticated `nexusd-cluster`. `e2e/nexus-a2a/run-managed-subprocess.sh`
//! provides the daemon (`NEXUS_A2A_TEST_ENDPOINT`) and an owner credential
//! (`NEXUS_A2A_TEST_CERT_DIR`).
//!
//! Every effect must land on that daemon: the transcript, the writer lease,
//! the workspace files and the mailbox. Nothing may land on local disk or
//! reach a model except through the daemon's mount.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use engine_host::managed_agent::prepare_managed_agent;
use engine_host::managed_context::ManagedRuntimeContext;
use engine_host::SessionLifecycle;
use runtime::spawn_task::AgentDescriptor;
use runtime::{FsBackend, NexusVfsFsBackend};
use serde_json::{json, Value};

/// One ACP client connection to `serve_managed`, over in-memory lines. The
/// agent runs on its own thread and runtime, as a host's driver would.
struct Acp {
    to_agent: Option<futures::channel::mpsc::UnboundedSender<std::io::Result<String>>>,
    from_agent: mpsc::Receiver<String>,
    agent: Option<std::thread::JoinHandle<Result<(), String>>>,
    next_id: u64,
}

impl Acp {
    fn serve(context: ManagedRuntimeContext) -> Self {
        let (to_agent, incoming) = futures::channel::mpsc::unbounded::<std::io::Result<String>>();
        let (lines, from_agent) = mpsc::channel::<String>();
        let outgoing = futures::sink::unfold(lines, |lines, line: String| async move {
            lines.send(line).map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>(lines)
        });
        let transport = agent_client_protocol::Lines::new(Box::pin(outgoing), incoming);
        let agent = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?
                .block_on(engine_acp::managed::serve_managed(context, transport))
                .map_err(|e| e.to_string())
        });
        let mut acp = Self {
            to_agent: Some(to_agent),
            from_agent,
            agent: Some(agent),
            next_id: 0,
        };
        let init = acp.request(
            "initialize",
            &json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        assert!(init.get("result").is_some(), "initialize: {init}");
        acp
    }

    /// Send one request and return its response, skipping notifications.
    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.to_agent
            .as_ref()
            .expect("connection open")
            .unbounded_send(Ok(line.to_string()))
            .expect("agent accepts input");
        loop {
            let line = self
                .from_agent
                .recv_timeout(Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("{method} got no response"));
            let message: Value = serde_json::from_str(&line).expect("agent writes JSON-RPC");
            // A managed session runs in Prompt mode: tool approval is the
            // client's, and this client approves once, explicitly.
            if message["method"] == "session/request_permission" {
                let option = message["params"]["options"]
                    .as_array()
                    .expect("permission options")
                    .iter()
                    .find(|option| option["kind"] == "allow_once")
                    .expect("an allow-once option")["optionId"]
                    .clone();
                let answer = json!({"jsonrpc": "2.0", "id": message["id"],
                    "result": {"outcome": {"outcome": "selected", "optionId": option}}});
                self.to_agent
                    .as_ref()
                    .expect("connection open")
                    .unbounded_send(Ok(answer.to_string()))
                    .expect("agent accepts input");
                continue;
            }
            if message.get("method").is_none() && message["id"] == id {
                return message;
            }
        }
    }

    /// End the connection the way a host does, with an error on the incoming
    /// side (a line transport outlives a bare EOF while its output is open),
    /// and wait until the agent released its sessions.
    fn close(mut self) {
        if let Some(to_agent) = self.to_agent.take() {
            let _ = to_agent.unbounded_send(Err(std::io::Error::other("client closed")));
        }
        let agent = self.agent.take().expect("agent thread");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !agent.is_finished() {
            assert!(
                Instant::now() < deadline,
                "serve_managed did not end after its connection closed"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // The transport ends with the client's error; cleanup runs either way.
        let _ = agent.join().expect("agent thread");
    }
}

fn write_config(dir: &Path, base_url: &str) {
    let config = json!({
        "auth_modes": {"api-key": {"anthropic": {"baseUrl": base_url, "apiKey": "unused"}}},
        "models": {"remote-test": {"alias": "remote-test", "name": "remote test", "input": ["text"],
            "providers": {"api-key": {"provider": "anthropic", "model": "claude-sonnet-4-6"}}}}
    });
    std::fs::write(
        dir.join("sudocode.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
}

fn session_params(session_id: Option<&str>, cwd: &Path) -> Value {
    let mut params = json!({"cwd": cwd.to_string_lossy(), "mcpServers": []});
    if let Some(id) = session_id {
        params["sessionId"] = json!(id);
    }
    params
}

#[test]
#[ignore = "requires the authenticated daemon from run-managed-subprocess.sh"]
#[allow(clippy::too_many_lines)] // one agent's whole journey on one daemon
fn a_remote_managed_agent_keeps_every_effect_on_its_daemon() {
    // The one authenticated connection, built the way a host builds it.
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT");
    let credential = runtime::nexus_mailbox::AgentCredential::load(
        &std::env::var("NEXUS_A2A_TEST_CERT_DIR").expect("set NEXUS_A2A_TEST_CERT_DIR"),
    )
    .unwrap_or_else(|e| panic!("load test credential: {e}"));
    let client = runtime::nexus_mailbox::Config {
        endpoint,
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .unwrap_or_else(|e| panic!("{e}"));
    let fresh = format!(
        "{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let workspace = format!("/managed-remote-{fresh}/workspace");
    let mut desc = AgentDescriptor {
        pid: format!("remote-{fresh}"),
        name: format!("remote-agent-{fresh}"),
        owner_id: "remote-owner".into(),
        zone_id: "root".into(),
        ..AgentDescriptor::default()
    };
    desc.labels.insert("model".into(), "remote-test".into());
    let shell = tempfile::tempdir().unwrap();
    let context = || {
        ManagedRuntimeContext::remote(
            &desc,
            Arc::clone(&client),
            &workspace,
            Some(shell.path().to_path_buf()),
            true,
        )
    };
    // What the daemon holds, read over the same credential but outside the agent.
    let daemon: Arc<dyn FsBackend> = Arc::new(NexusVfsFsBackend::from_arc_mtls(
        Arc::clone(&client),
        String::new(),
        "root",
    ));

    // A workspace the connection cannot answer for starts nothing.
    let refused = context()
        .err()
        .expect("an unanswered workspace must be refused");
    assert!(refused.contains("cannot reach workspace"), "{refused}");
    daemon.create_dir_all(&workspace).unwrap();

    // A direct HTTP model route is refused before any session exists.
    let config = tempfile::tempdir().unwrap();
    write_config(config.path(), "http://127.0.0.1:1");
    // This test owns its process and config home.
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config.path());
    let mut acp = Acp::serve(context().unwrap());
    let direct = acp.request("session/new", &session_params(None, shell.path()));
    assert!(
        direct["error"].to_string().contains("nexus:///mount"),
        "a direct model route must be refused: {direct}"
    );
    acp.close();

    write_config(config.path(), "nexus:///model");
    let mut acp = Acp::serve(context().unwrap());
    let created = acp.request("session/new", &session_params(None, shell.path()));
    let session_id = created["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new: {created}"))
        .to_owned();
    assert!(
        daemon
            .exists(&format!("/sessions/{session_id}/transcript.jsonl"))
            .unwrap(),
        "the transcript lives on the daemon"
    );
    assert!(
        !shell.path().join(".scode").exists(),
        "nothing is stored on local disk"
    );

    // Only a confirmed absence is not-found; an alias is not an id.
    let missing = acp.request(
        "session/load",
        &session_params(Some("session-never-persisted"), shell.path()),
    );
    assert_eq!(missing["error"]["code"], -32002, "{missing}");
    assert_eq!(
        missing["error"]["data"]["sessionId"], "session-never-persisted",
        "{missing}"
    );
    let alias = acp.request(
        "session/load",
        &session_params(Some("latest"), shell.path()),
    );
    assert_eq!(alias["error"]["code"], -32602, "{alias}");

    // The mailbox is the same backend: a send lands on the daemon.
    let before = daemon.readdir("/conversations").unwrap_or_default().len();
    let sent = acp.request(
        "_sudocode/a2a/send",
        &json!({"sessionId": session_id, "to": format!("remote-peer-{fresh}"),
            "message": format!("remote hello {fresh}"), "summary": "remote hello"}),
    );
    assert!(sent.get("error").is_none(), "{sent}");
    assert!(
        daemon.readdir("/conversations").unwrap().len() > before,
        "the conversation is created on the daemon"
    );
    acp.close();

    // Another writer holding the lease refuses the resume; its release admits it.
    let lease_path = format!("/sessions/{session_id}");
    let held = client
        .lock(&lease_path, "", 60_000, "")
        .unwrap()
        .expect("the session is not leased once its connection closed");
    let mut acp = Acp::serve(context().unwrap());
    let busy = acp.request(
        "session/load",
        &session_params(Some(&session_id), shell.path()),
    );
    assert!(
        busy["error"]
            .to_string()
            .contains("session is still running"),
        "{busy}"
    );
    assert!(client.unlock(&lease_path, &held, "").unwrap());
    let loaded = acp.request(
        "session/load",
        &session_params(Some(&session_id), shell.path()),
    );
    assert!(loaded.get("result").is_some(), "{loaded}");
    acp.close();

    // The engine's tool backend writes inside the remote workspace.
    let context = context().unwrap();
    let engine = prepare_managed_agent(&context, None)
        .unwrap()
        .open(
            BTreeMap::default(),
            runtime::SystemPromptOverrides::default(),
            runtime::memory::MemoryMode::default(),
        )
        .unwrap();
    let tools = engine.session_snapshot().fs_handle();
    tools.write("from-file-tool.txt", b"remote-bytes").unwrap();
    assert_eq!(
        daemon
            .read(&format!("{workspace}/from-file-tool.txt"))
            .unwrap(),
        b"remote-bytes"
    );
    assert!(!shell.path().join("from-file-tool.txt").exists());
}
