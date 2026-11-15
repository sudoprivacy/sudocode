//! Actual CLI observations across the authenticated coordinator-backed cluster.

use super::{spawn_acp_from_binary_with_args, AcpStdio};
use runtime::runtime_status::{ForegroundState, RuntimeAvailability, RuntimeObservation};
use serde_json::{json, Value};
use std::fs;
use std::time::{Duration, Instant};

struct Peer {
    acp: AcpStdio,
    sid: String,
    workspace: tempfile::TempDir,
    _config: tempfile::TempDir,
}

fn peer(endpoint: &str, agent: &str, hold_tool: bool) -> Peer {
    let workspace = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    if hold_tool {
        fs::write(
            config.path().join("settings.json"),
            json!({
                "hooks":{"PostToolUse":["sh hold-status.sh"]}
            })
            .to_string(),
        )
        .unwrap();
        fs::write(workspace.path().join("hold-status.sh"),
            "printf x > progress\nprintf ready > ready\ni=0\nwhile [ ! -f release ] && [ $i -lt 600 ]; do\n printf x >> progress\n sleep 0.1\n i=$((i+1))\ndone\nprintf finished > finished\n"
        ).unwrap();
    }
    let binary = std::env::var_os("SCODE_RECEIVER_TEST_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_scode").into());
    let mut acp = spawn_acp_from_binary_with_args(
        &binary,
        endpoint,
        agent,
        "unused-peer",
        &workspace,
        &config,
        &["--permission-mode", "danger-full-access"],
    );
    let initialized = acp.request("initialize", &json!({"protocolVersion":1}));
    assert_eq!(
        initialized["result"]["_meta"]["sudocode"]["runtimeStatus"]["version"],
        1
    );
    let opened = acp.request(
        "session/new",
        &json!({"cwd":workspace.path(),"mcpServers":[]}),
    );
    let sid = opened["result"]["sessionId"]
        .as_str()
        .expect("real SID")
        .into();
    Peer {
        acp,
        sid,
        workspace,
        _config: config,
    }
}

fn directory(observer: &mut Peer) -> Value {
    let response = observer.acp.request(
        "_sudocode/a2a/agent_list",
        &json!({"sessionId":observer.sid}),
    );
    assert_eq!(response["result"]["isError"], false, "{response}");
    serde_json::from_str(response["result"]["content"].as_str().unwrap()).unwrap()
}

fn wait_for_peers(
    observer: &mut Peer,
    expected: &[(&str, ForegroundState)],
) -> Vec<RuntimeObservation> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = directory(observer);
        assert!(
            result["runtime_discovery"]["unavailable"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{result}"
        );
        let scopes = result["runtime_discovery"]["scopes"].as_array().unwrap();
        assert_eq!(
            scopes.len(),
            2,
            "must query both real coordinator members: {result}"
        );
        assert!(
            scopes.iter().all(|scope| scope["complete"] == true),
            "{result}"
        );
        let rows: Vec<RuntimeObservation> = result["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row.get("runtime").filter(|value| !value.is_null()))
            .cloned()
            .map(|row| serde_json::from_value(row).expect("typed instance observation"))
            .collect();
        assert!(
            rows.iter()
                .all(|row| row.update.as_ref().is_none_or(|facts| facts
                    .identity
                    .session_id
                    .as_deref()
                    != Some(observer.sid.as_str()))),
            "the exact observer instance was not excluded: {result}"
        );
        let matching: Vec<_> = expected
            .iter()
            .filter_map(|(sid, foreground)| {
                rows.iter()
                    .find(|row| {
                        row.availability == RuntimeAvailability::Fresh
                            && row.update.as_ref().is_some_and(|facts| {
                                facts.identity.session_id.as_deref() == Some(*sid)
                                    && facts.status.foreground == *foreground
                            })
                    })
                    .cloned()
            })
            .collect();
        if matching.len() == expected.len() {
            return matching;
        }
        assert!(Instant::now() < deadline, "expected {expected:?}: {result}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_file(path: &std::path::Path, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "actual file event missing: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn verify_delivery(endpoint: &str, path: &str) {
    let credential = runtime::nexus_mailbox::AgentCredential::load(
        &std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL").unwrap(),
    )
    .unwrap();
    let client = runtime::nexus_mailbox::Config {
        endpoint: endpoint.into(),
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut offset = 0;
    let mut bytes = Vec::new();
    loop {
        let frame = client.stream_read_at(path, offset, true, 1000, "").unwrap();
        offset = frame.next_offset;
        bytes.extend(frame.data);
        if String::from_utf8_lossy(&bytes).contains("ACTUAL_CROSS_NODE_RUNTIME_DELIVERY") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "message missing from actual peer-node mailbox stream"
        );
    }
}

struct Release(std::path::PathBuf);
impl Drop for Release {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "cleanup");
    }
}

fn send_while_observed(
    first: &Peer,
    second: &mut Peer,
    observer: &mut Peer,
    agent: &str,
    idle: &[RuntimeObservation],
) -> Value {
    let released = second.workspace.path().join("release");
    let ready = second.workspace.path().join("ready");
    let progress = second.workspace.path().join("progress");
    let finished = second.workspace.path().join("finished");
    let sid = second.sid.clone();
    let receipt = std::thread::scope(|scope| {
        let _release = Release(released.clone());
        let acp = &mut second.acp;
        let request_sid = sid.clone();
        let sent = scope.spawn(move || {
            acp.request(
                "_sudocode/a2a/send",
                &json!({
                    "sessionId":request_sid,"to":agent,"summary":"runtime acceptance",
                    "message":"ACTUAL_CROSS_NODE_RUNTIME_DELIVERY"
                }),
            )
        });
        wait_file(&ready, || ready.exists());
        let running = wait_for_peers(
            observer,
            &[
                (&first.sid, ForegroundState::Idle),
                (&sid, ForegroundState::Running),
            ],
        );
        assert_eq!(running[0].instance, idle[0].instance);
        assert_eq!(running[1].instance, idle[1].instance);
        let length = fs::read(&progress).unwrap().len();
        wait_file(&progress, || fs::read(&progress).unwrap().len() > length);
        fs::write(&released, "release").unwrap();
        sent.join().unwrap()
    });
    assert_eq!(receipt["result"]["isError"], false, "{receipt}");
    assert_eq!(fs::read_to_string(finished).unwrap(), "finished");
    receipt
}

#[test]
#[ignore = "requires the fresh runtime credential and both real nodes from run-cross-node.sh"]
fn live_runtime_directory_tracks_same_agent_instances_across_nodes() {
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").unwrap();
    let other = std::env::var("NEXUS_A2A_TEST_PEER_ENDPOINT").unwrap();
    let credential = runtime::nexus_mailbox::AgentCredential::load(
        &std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL").unwrap(),
    )
    .unwrap();
    let agent = credential.agent;
    let mut first = peer(&endpoint, &agent, false);
    let mut second = peer(&other, &agent, true);
    let mut observer = peer(&endpoint, &agent, false);
    let idle = wait_for_peers(
        &mut observer,
        &[
            (&first.sid, ForegroundState::Idle),
            (&second.sid, ForegroundState::Idle),
        ],
    );
    assert_ne!(idle[0].instance, idle[1].instance);
    assert_eq!(idle[0].agent_id, idle[1].agent_id);
    let receipt = send_while_observed(&first, &mut second, &mut observer, &agent, &idle);
    let delivered: Value =
        serde_json::from_str(receipt["result"]["content"].as_str().unwrap()).unwrap();
    verify_delivery(
        &endpoint,
        delivered["routing"]["mailbox_path"].as_str().unwrap(),
    );
    second
        .acp
        .request("session/close", &json!({"sessionId":second.sid}));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = directory(&mut observer);
        if result["agents"].as_array().unwrap().iter().all(|row| {
            row["runtime"]["instance"] != serde_json::to_value(&idle[1].instance).unwrap()
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "closed instance remains registered: {result}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        wait_for_peers(&mut observer, &[(&first.sid, ForegroundState::Idle)])[0].instance,
        idle[0].instance
    );
    first
        .acp
        .request("session/close", &json!({"sessionId":first.sid}));
    observer
        .acp
        .request("session/close", &json!({"sessionId":observer.sid}));
}
