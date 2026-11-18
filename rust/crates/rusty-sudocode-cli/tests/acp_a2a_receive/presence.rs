//! Actual CLI presence across the authenticated coordinator-backed cluster.

use super::{spawn_acp_as, AcpStdio};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Peer {
    acp: AcpStdio,
    sid: String,
    _workspace: tempfile::TempDir,
    _config: tempfile::TempDir,
}

fn credential(variable: &str) -> (PathBuf, String) {
    let path = PathBuf::from(std::env::var(variable).unwrap_or_else(|_| panic!("set {variable}")));
    let agent = runtime::nexus_mailbox::AgentCredential::load(&path.to_string_lossy())
        .unwrap()
        .agent;
    (path, agent)
}

fn peer(endpoint: &str, agent: &str, credential: &Path) -> Peer {
    let workspace = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let binary = std::env::var_os("SCODE_RECEIVER_TEST_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_scode").into());
    let mut acp = spawn_acp_as(
        &binary,
        endpoint,
        agent,
        credential,
        &workspace,
        &config,
        &["--permission-mode", "danger-full-access"],
    );
    acp.request("initialize", &json!({"protocolVersion":1}));
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
        _workspace: workspace,
        _config: config,
    }
}

fn agent_list(observer: &mut Peer) -> Value {
    let response = observer.acp.request(
        "_sudocode/a2a/agent_list",
        &json!({"sessionId":observer.sid}),
    );
    assert_eq!(response["result"]["isError"], false, "{response}");
    serde_json::from_str(response["result"]["content"].as_str().unwrap()).unwrap()
}

/// Wait until `agent`'s row reads `active`, also through `active_only`.
fn wait_active(observer: &mut Peer, agent: &str, active: bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let listing = agent_list(observer);
        assert!(listing.get("runtime_discovery").is_none(), "{listing}");
        let row = listing["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == agent)
            .cloned()
            .unwrap_or_else(|| panic!("{agent} is not listed: {listing}"));
        if row["active"] == active {
            let only = observer.acp.request(
                "_sudocode/a2a/agent_list",
                &json!({"sessionId":observer.sid,"activeOnly":true}),
            );
            let only: Value =
                serde_json::from_str(only["result"]["content"].as_str().unwrap()).unwrap();
            let listed = only["agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["name"] == agent);
            assert_eq!(listed, active, "active_only disagrees: {only}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{agent} never read active={active}: {listing}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "requires both real nodes and two agent credentials from run-cross-node.sh"]
fn live_presence_names_a_peer_online_across_nodes_until_it_exits() {
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").unwrap();
    let other = std::env::var("NEXUS_A2A_TEST_PEER_ENDPOINT").unwrap();
    let (target_credential, target_agent) = credential("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL");
    let (watcher_credential, watcher_agent) = credential("NEXUS_A2A_TEST_SENDER_CREDENTIAL");
    assert_ne!(target_agent, watcher_agent, "two distinct Agents");
    // The target Agent runs on the other node; presence crosses the zone.
    let target = peer(&other, &target_agent, &target_credential);
    let mut watcher = peer(&endpoint, &watcher_agent, &watcher_credential);
    wait_active(&mut watcher, &target_agent, true);
    drop(target);
    wait_active(&mut watcher, &target_agent, false);
    watcher
        .acp
        .request("session/close", &json!({"sessionId":watcher.sid}));
}
