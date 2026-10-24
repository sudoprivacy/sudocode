//! Consumer acceptance through the existing real ACP stdio harness.
//! This exercises unavailable node bindings, not successful node enrollment.

use super::{
    node_home, pin_agent_name, set_session_mode, spawn_stdio_client, AcpTestClient, TestWorkspace,
    Transport,
};
use mock_anthropic_service::MockAnthropicService;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::Duration;

async fn expect_tool_error(
    client: &mut AcpTestClient,
    method: &str,
    params: Value,
    expected: &str,
) -> String {
    let (_, response) = client.send_request(method, params).await;
    assert!(
        response.get("error").is_none(),
        "preserve the actual tool result: {response}"
    );
    assert_eq!(response["result"]["isError"], true);
    let content = response["result"]["content"].as_str().unwrap();
    assert!(
        content.contains(expected),
        "expected {expected:?}: {response}"
    );
    content.to_string()
}

fn pending_workspace(endpoint: &str, agent: &str) -> (TestWorkspace, std::path::PathBuf) {
    pending_workspace_on_node(endpoint, agent, "127.0.0.1:1")
}

fn pending_workspace_on_node(
    endpoint: &str,
    agent: &str,
    node_endpoint: &str,
) -> (TestWorkspace, std::path::PathBuf) {
    let workspace = TestWorkspace::new("pending-node-binding");
    workspace.create();
    workspace.write_sudocode_json(endpoint);
    pin_agent_name(&workspace.root, agent);
    let agents = workspace.root.join("node-credentials");
    fs::create_dir_all(&agents).unwrap();
    let run = workspace.home.join(".nexus/run");
    fs::create_dir_all(&run).unwrap();
    fs::write(
        run.join("node.json"),
        json!({"version": 1, "endpoint": node_endpoint, "agents_dir": agents}).to_string(),
    )
    .unwrap();
    (workspace, agents)
}

#[tokio::test]
async fn a_configured_agent_loads_without_a_bundle_and_resolves_it_in_the_same_process() {
    const AGENT: &str = "pending-node-actor";
    let server = MockAnthropicService::spawn()
        .await
        .expect("start model spy");
    let (workspace, agents) = pending_workspace(&server.base_url(), AGENT);

    let mut client = spawn_stdio_client(&workspace);
    client
        .send_request("initialize", json!({"protocolVersion": 1}))
        .await;
    let (_, created) = client
        .send_request(
            "session/new",
            json!({
                "cwd": workspace.root, "mcpServers": [],
            }),
        )
        .await;
    assert!(
        created.get("error").is_none(),
        "ordinary session must load: {created}"
    );
    let peer = &created["result"]["_meta"]["sudocode"]["peer"];
    assert_eq!(peer["agent"], AGENT);
    assert_eq!(
        peer["namespace"], "node",
        "missing bundle is not disk: {created}"
    );
    assert!(peer["error"]
        .as_str()
        .is_some_and(|e| e.contains("ask the node owner to mint")));
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    set_session_mode(&mut client, &sid, "workspace-write").await;

    for (method, params) in [
        (
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": "node-peer", "message": "BEFORE_BUNDLE_DISTINCT_OPERATION", "summary": "report the missing bundle"}),
        ),
        ("_sudocode/a2a/agent_list", json!({"sessionId": sid})),
    ] {
        expect_tool_error(&mut client, method, params, "ask the node owner to mint").await;
    }
    let pair_root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    assert!(
        !pair_root.exists(),
        "missing bundle must not create a disk namespace"
    );

    // Local fixture only: no live mint or daemon. Once these files exist, the
    // same loaded session must reach the actual node transport instead of
    // retaining a stale unavailable error or selecting disk.
    node_home::write_untrusted_credential(&agents, AGENT);
    for (method, params) in [
        ("_sudocode/a2a/agent_list", json!({"sessionId": sid})),
        (
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": "node-peer", "message": "AFTER_BUNDLE_DISTINCT_OPERATION", "summary": "reach the selected node transport"}),
        ),
    ] {
        let content = expect_tool_error(&mut client, method, params, "connect").await;
        assert!(
            !content.contains("ask the node owner to mint"),
            "stale binding: {content}"
        );
    }
    client.shutdown().await;
    assert!(!pair_root.exists(), "recovery must keep the node namespace");
    assert!(!workspace.root.join("conversations").exists());
    assert!(!workspace.root.join(".sudocode-inbox").exists());
    node_home::assert_no_disk_mailboxes_below(&workspace.root);
    assert_eq!(
        server
            .captured_requests()
            .await
            .iter()
            .filter(|r| r.path == "/v1/messages")
            .count(),
        0,
        "loading, errors and direct calls must not ask the model"
    );

    workspace.cleanup();
}

/// A real CA-signed bundle arrives after the original receiver started. The
/// selected mailbox must recover in place, including inbound ACP delivery.
#[tokio::test]
#[ignore = "real TLS daemon; e2e/nexus-a2a/run.sh"]
async fn live_pending_node_recovers_send_and_the_original_receiver() {
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("live node endpoint");
    let bundle = std::env::var("NEXUS_A2A_TEST_PENDING_CREDENTIAL")
        .expect("the harness's isolated pending actor bundle");
    let actor = runtime::nexus_mailbox::AgentCredential::load(&bundle)
        .expect("load the fixture actor")
        .agent;
    let observer = runtime::nexus_mailbox::AgentCredential::load(
        &std::env::var("NEXUS_A2A_TEST_CERT_DIR").expect("observer bundle"),
    )
    .expect("load observer");
    assert_ne!(actor, observer.agent);
    let observer_name = observer.agent.clone();
    let connection = runtime::nexus_mailbox::Config {
        endpoint: endpoint.clone(),
        agent: observer.agent,
        tls: observer.tls,
    }
    .connect()
    .expect("connect observer");
    let observer =
        runtime::mailbox::Mailbox::over_nexus_mtls(connection, &observer_name, "", "root");
    let model = MockAnthropicService::spawn().await.expect("model spy");
    let host_port = endpoint
        .split_once("://")
        .map_or(endpoint.as_str(), |(_, address)| address);
    let (workspace, agents) = pending_workspace_on_node(&model.base_url(), &actor, host_port);
    let mut client = spawn_stdio_client(&workspace);
    let original_pid = match &client.transport {
        Transport::Stdio { child, .. } => child.id(),
        Transport::WebSocket { .. } => unreachable!("stdio fixture"),
    };
    client
        .send_request("initialize", json!({"protocolVersion": 1}))
        .await;
    let (_, created) = client
        .send_request(
            "session/new",
            json!({"cwd": workspace.root, "mcpServers": []}),
        )
        .await;
    assert!(created.get("error").is_none(), "{created}");
    assert_eq!(
        created["result"]["_meta"]["sudocode"]["peer"]["namespace"],
        "node"
    );
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    set_session_mode(&mut client, &sid, "workspace-write").await;
    expect_tool_error(
        &mut client,
        "_sudocode/a2a/agent_list",
        json!({"sessionId": sid}),
        "ask the node owner to mint",
    )
    .await;

    // The fixture owner publishes complete PEMs before the manifest. No mint,
    // new scode process, session/load, prompt or receiver restart occurs here.
    let destination = agents.join(&actor);
    fs::create_dir_all(&destination).unwrap();
    for entry in fs::read_dir(&bundle).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() && entry.file_name() != "credential.json" {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
    fs::copy(
        Path::new(&bundle).join("credential.json"),
        destination.join("credential.json"),
    )
    .unwrap();

    let outgoing = format!("PENDING_NODE_SEND_{}", uuid::Uuid::new_v4());
    let (_, sent) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": observer_name, "message": outgoing, "summary": "send after local bundle publication"}),
        )
        .await;
    assert!(sent.get("error").is_none(), "{sent}");
    assert_ne!(sent["result"]["isError"], true, "{sent}");
    let frames = observer
        .read_conversation(&actor)
        .expect("read real node append");
    assert_eq!(
        frames.iter().filter(|frame| frame.body == outgoing).count(),
        1
    );
    assert!(frames
        .iter()
        .filter(|frame| frame.body == outgoing)
        .all(|frame| frame.from == actor));
    assert_eq!(
        model
            .captured_requests()
            .await
            .iter()
            .filter(|request| request.path == "/v1/messages")
            .count(),
        0,
        "session startup, configuration recovery and direct send must not infer"
    );

    let incoming = format!("PENDING_NODE_RECEIVE_{}", uuid::Uuid::new_v4());
    observer
        .send(runtime::agent_mailbox::MailboxEnvelope {
            from: observer_name.clone(),
            to: actor,
            body: incoming.clone(),
            summary: Some("exercise the original pending receiver".into()),
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("append incoming frame on real node");
    let (_, notification) = client
        .recv_until(Duration::from_secs(15), |message| {
            message["method"] == "session/update"
                && message["params"]["sessionId"] == sid
                && message["params"]["update"]["sessionUpdate"] == "user_message_chunk"
                && message["params"]["update"]["content"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(&incoming))
        })
        .await
        .unwrap_or_else(|seen| {
            panic!("original receiver never delivered the node frame: {seen:?}")
        });
    assert_eq!(
        notification["params"]["_meta"]["sudocode"]["a2a"]["from"],
        observer_name
    );
    if let Transport::Stdio { child, .. } = &client.transport {
        assert_eq!(child.id(), original_pid, "same process and native SID");
    }
    // Receiving a nonempty peer message may now run the normal mock-backed
    // autonomous turn. The zero-inference promise above concerns direct calls.
    client.shutdown().await;
    node_home::assert_no_disk_mailboxes_below(&workspace.root);
    workspace.cleanup();
}
