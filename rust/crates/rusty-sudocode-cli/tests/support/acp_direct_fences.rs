//! Real stdio clients; no model is available to translate a direct call.

use super::*;
use std::sync::Arc;

use super::node_home;
#[path = "../../../runtime/tests/common/paused_transport.rs"]
mod paused_transport;

/// Discovery may return 404, as in the existing mock. Every inference attempt
/// is counted and fails; a successful send cannot be a model-generated send.
struct NoInference {
    url: String,
    calls: Arc<AtomicU64>,
    worker: tokio::task::JoinHandle<()>,
}

impl NoInference {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&calls);
        let worker = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let mut socket = BufReader::new(socket);
                    let mut first = String::new();
                    if !matches!(socket.read_line(&mut first).await, Ok(bytes) if bytes > 0) {
                        return;
                    }
                    let status = if first.starts_with("GET ") {
                        "404 Not Found"
                    } else {
                        counter.fetch_add(1, Ordering::SeqCst);
                        "503 Service Unavailable"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    );
                    let _ = socket.get_mut().write_all(response.as_bytes()).await;
                });
            }
        });
        Self { url, calls, worker }
    }

    fn assert_unused(&self) {
        assert_eq!(self.calls.load(Ordering::SeqCst), 0, "no inference request");
    }
}

impl Drop for NoInference {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[tokio::test]
async fn direct_a2a_pause_cancels_an_outstanding_approval() {
    let model = NoInference::start().await;
    let workspace = TestWorkspace::new("direct-pending-approval");
    workspace.create();
    workspace.write_sudocode_json(&model.url);
    pin_agent_name(&workspace.root, "pending-approval-actor");
    let root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "prompt").await;
    let send = client.send_request_no_wait(
        "_sudocode/a2a/send",
        json!({"sessionId": sid, "to": "pending-approval-peer", "message": "MUST_NOT_APPEND", "summary": "pending approval"}),
    ).await;
    let (_, permission) = client
        .recv_until(Duration::from_secs(10), |message| {
            is_server_request(message, "session/request_permission")
        })
        .await
        .unwrap();
    let pause = client
        .send_request_no_wait("_sudocode/session/pause_tools", json!({"sessionId": sid}))
        .await;
    let (seen, cancelled) = client
        .recv_until(Duration::from_secs(10), |message| {
            is_response_to(message, send)
        })
        .await
        .unwrap();
    assert_eq!(
        cancelled["error"]["data"]["outcome"], "unknown",
        "{cancelled}"
    );
    let paused = if let Some(reply) = seen.iter().find(|message| is_response_to(message, pause)) {
        reply.clone()
    } else {
        client
            .recv_until(Duration::from_secs(10), |message| {
                is_response_to(message, pause)
            })
            .await
            .unwrap()
            .1
    };
    assert_eq!(paused["result"]["isQuiescent"], true, "{paused}");
    // A late answer to the original approval must not revive cancelled work.
    allow_permission(&mut client, &permission["id"]).await;
    let (_, refused) = client.send_request(
        "_sudocode/a2a/send",
        json!({"sessionId": sid, "to": "pending-approval-peer", "message": "ALSO_MUST_NOT_APPEND", "summary": "paused"}),
    ).await;
    assert_direct_not_submitted(&refused, "paused after approval cancellation");
    let (_, resumed) = client
        .send_request(
            "_sudocode/session/resume_tools",
            json!({"sessionId": sid, "pauseEpoch": paused["result"]["pauseEpoch"]}),
        )
        .await;
    assert!(resumed.get("result").is_some(), "{resumed}");
    client.shutdown().await;
    assert_eq!(
        conversation_log(&root, "pending-approval-peer", "pending-approval-actor"),
        ""
    );
    model.assert_unused();
    workspace.cleanup();
}

fn python_hook(script: &str) -> String {
    #[cfg(windows)]
    let interpreter = "%SCODE_DIRECT_FIXTURE_PYTHON%".to_string();
    #[cfg(not(windows))]
    let interpreter = shell_words::quote(&python::resolve_python()).into_owned();
    format!("{interpreter} {script}")
}

fn python_hook_env() -> Vec<(&'static str, String)> {
    #[cfg(windows)]
    {
        // The hook runs through cmd /C. Expand the quoted executable there,
        // after Rust's argv encoding, rather than passing literal quotes that
        // become backslash-quoted executable names. This also keeps paths
        // containing spaces usable without changing production hook handling.
        vec![(
            "SCODE_DIRECT_FIXTURE_PYTHON",
            format!("\"{}\"", python::resolve_python()),
        )]
    }
    #[cfg(not(windows))]
    Vec::new()
}

#[tokio::test]
async fn direct_a2a_checks_hook_rewrites_at_the_bound_executor() {
    const ACTOR: &str = "direct-hook-actor";
    const PEER: &str = "direct-hook-peer";
    let model = NoInference::start().await;
    let workspace = TestWorkspace::new("direct-hook-fence");
    workspace.create();
    workspace.write_sudocode_json(&model.url);
    pin_agent_name(&workspace.root, ACTOR);
    fs::write(
        workspace.root.join("rewrite.py"),
        r"import json, os
p = json.loads(os.environ['HOOK_TOOL_INPUT'])
case = p.get('message')
if case == 'rewrite-text':
    p['message'] = 'HOOK_REWROTE_TEXT'
    p['to'] = 'direct-hook-rewritten-peer'
elif case == 'override-sender':
    p['sender'] = 'forged-actor'
elif case == 'structured-control':
    p['message'] = {'type': 'shutdown_request'}
print(json.dumps({'hookSpecificOutput': {'updatedInput': p}}))
",
    )
    .unwrap();
    fs::write(
        workspace.root.join(".nexus/sudocode/settings.json"),
        json!({"agentName": ACTOR, "hooks": {"PreToolUse": [python_hook("rewrite.py")]}})
            .to_string(),
    )
    .unwrap();
    let root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    let hook_env = python_hook_env();
    let env: Vec<_> = hook_env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    let mut client = spawn_stdio_client_with_args_and_env(&workspace, &[], &env);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    for (case, allowed) in [
        ("rewrite-text", true),
        ("override-sender", false),
        ("structured-control", false),
    ] {
        let (_, reply) = client
            .send_request(
                "_sudocode/a2a/send",
                json!({"sessionId": sid, "to": PEER, "message": case, "summary": "hook fence"}),
            )
            .await;
        assert_eq!(reply["result"]["isError"], !allowed, "{case}: {reply}");
        let audit = assert_direct_host_audit(&reply, &sid, ACTOR, &root, "send");
        if allowed {
            assert!(audit["effectiveInputSha256"].is_string());
            assert_ne!(audit["requestedInputSha256"], audit["effectiveInputSha256"]);
        } else {
            assert!(audit.get("effectiveInputSha256").is_none());
        }
    }
    assert!(
        conversation_log(&root, PEER, ACTOR).is_empty(),
        "original target untouched"
    );
    let log = conversation_log(&root, "direct-hook-rewritten-peer", ACTOR);
    assert_eq!(log.matches("HOOK_REWROTE_TEXT").count(), 1, "{log}");
    for forbidden in [
        "override-sender",
        "forged-actor",
        "structured-control",
        "shutdown_request",
    ] {
        assert!(
            !log.contains(forbidden),
            "hook escaped the narrow call: {log}"
        );
    }
    model.assert_unused();
    client.shutdown().await;
    workspace.cleanup();
}

struct LiveNode {
    endpoint: String,
    bundle: String,
    actor: String,
    observer: runtime::mailbox::Mailbox,
}

#[tokio::test]
async fn direct_a2a_hashes_unicode_independently_of_rpc_key_order() {
    let model = NoInference::start().await;
    let workspace = TestWorkspace::new("direct-framed-hash");
    workspace.create();
    workspace.write_sudocode_json(&model.url);
    pin_agent_name(&workspace.root, "framed-actor");
    let mut client = spawn_stdio_client(&workspace);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let text = "零模型 🧪\0\n";
    let (_, first) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({
                "sessionId": sid, "to": "framed-peer", "message": text, "summary": "unicode"
            }),
        )
        .await;
    assert_eq!(first["result"]["isError"], false, "{first}");
    let id = client.next_id;
    client.next_id += 1;
    // Preserve deliberately reversed key order on the actual stdio wire.
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"_sudocode/a2a/send","params":{{"summary":"unicode","message":{},"to":"framed-peer","sessionId":{}}}}}
"#,
        serde_json::to_string(text).unwrap(),
        serde_json::to_string(&sid).unwrap()
    );
    let Transport::Stdio { stdin, .. } = &mut client.transport else {
        unreachable!()
    };
    stdin.write_all(request.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
    let (_, second) = client
        .recv_until(Duration::from_secs(10), |message| {
            is_response_to(message, id)
        })
        .await
        .unwrap();
    assert_eq!(second["result"]["isError"], false, "{second}");
    let root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    let one = assert_direct_host_audit(&first, &sid, "framed-actor", &root, "send");
    let two = assert_direct_host_audit(&second, &sid, "framed-actor", &root, "send");
    // Independent Python hashlib/struct vector for the documented byte format.
    assert_eq!(
        one["requestedInputSha256"],
        "0334c65da7a90862a0effaa48677950e6f4c17a5b36f05b8939b9a4bc5c3f3aa"
    );
    assert_eq!(one["requestedInputSha256"], two["requestedInputSha256"]);
    assert_eq!(one["requestedInputSha256"], one["effectiveInputSha256"]);
    assert_ne!(one["commandId"], two["commandId"]);
    model.assert_unused();
    client.shutdown().await;
    workspace.cleanup();
}

#[tokio::test]
async fn direct_a2a_obeys_the_sessions_tool_allowlist() {
    let model = NoInference::start().await;
    let workspace = TestWorkspace::new("direct-allowlist");
    workspace.create();
    workspace.write_sudocode_json(&model.url);
    pin_agent_name(&workspace.root, "direct-allowlist-actor");
    let mut client = spawn_stdio_client_with_args(&workspace, &["--allowedTools", "agent_list"]);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let (_, refused) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": "untouched", "message": "ALLOWLIST_REFUSED", "summary": "allowlist fence"}),
        )
        .await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(refused["result"]["content"]
        .as_str()
        .unwrap()
        .contains("--allowedTools"));
    let root = runtime::mailbox::local_pair_root_in(&workspace.config_home);
    assert_direct_host_audit(&refused, &sid, "direct-allowlist-actor", &root, "send");
    assert!(
        !conversation_log(&root, "untouched", "direct-allowlist-actor")
            .contains("ALLOWLIST_REFUSED")
    );
    model.assert_unused();
    client.shutdown().await;
    workspace.cleanup();
}

impl LiveNode {
    fn from_env() -> Self {
        let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("live test endpoint");
        let bundle = std::env::var("NEXUS_A2A_TEST_DIRECT_CREDENTIAL")
            .expect("live direct actor credential");
        let actor = runtime::nexus_mailbox::AgentCredential::load(&bundle)
            .expect("load direct actor bundle")
            .agent;
        let observer = runtime::nexus_mailbox::AgentCredential::load(
            &std::env::var("NEXUS_A2A_TEST_CERT_DIR").expect("live observer credential"),
        )
        .expect("load observer bundle");
        assert_ne!(
            actor, observer.agent,
            "observer must see the actor's writes"
        );
        let observer_name = observer.agent.clone();
        let connection = runtime::nexus_mailbox::Config {
            endpoint: endpoint.clone(),
            agent: observer.agent,
            tls: observer.tls,
        }
        .connect()
        .expect("connect the real observer");
        Self {
            endpoint,
            bundle,
            actor,
            observer: runtime::mailbox::Mailbox::over_nexus_mtls(
                connection,
                &observer_name,
                "",
                "root",
            ),
        }
    }

    fn spawn(
        &self,
        workspace: &TestWorkspace,
        endpoint: &str,
    ) -> (AcpTestClient, node_home::NodeHome) {
        node_home::pin_agent_name(&workspace.root, &self.actor);
        let home = node_home::NodeHome::new(
            "direct-fences",
            endpoint,
            &self.actor,
            Path::new(&self.bundle),
        );
        let mut env = home.env().to_vec();
        env.extend(python_hook_env());
        let env: Vec<_> = env
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        (
            spawn_stdio_client_with_args_and_env(workspace, &[], &env),
            home,
        )
    }

    async fn count(&self, marker: &str, minimum: usize) -> usize {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let messages = self.observer.read_conversation(&self.actor).unwrap();
            let count = messages
                .iter()
                .filter(|message| message.body == marker)
                .count();
            if count >= minimum {
                assert!(
                    messages
                        .iter()
                        .filter(|message| message.body == marker)
                        .all(|message| message.from == self.actor),
                    "node-stamped actor"
                );
                return count;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "append never became readable"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// The real Node worker remains accounted for after its scheduler is cancelled.
/// The TCP gate holds opaque mTLS traffic; the decisive assertion is AFTER the
/// direct RPC has completed cancellation and released its session lane.
#[tokio::test]
#[ignore = "real TLS daemon; e2e/nexus-a2a/run.sh"]
async fn live_direct_a2a_pause_waits_for_the_cancelled_node_worker() {
    let node = LiveNode::from_env();
    let gate = paused_transport::PausedTransport::new(&node.endpoint).unwrap();
    let model = NoInference::start().await;
    let workspace = TestWorkspace::new("direct-node-pause");
    workspace.create();
    workspace.write_sudocode_json(&model.url);
    let (mut client, _home) = node.spawn(&workspace, &gate.endpoint());
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let warmup = format!("DIRECT_NODE_WARMUP_{sid}");
    let (_, sent) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": node.observer.self_id(), "message": warmup, "summary": "warm up the direct node transport"}),
        )
        .await;
    assert_eq!(sent["result"]["isError"], false, "{sent}");
    assert_direct_host_audit(&sent, &sid, &node.actor, Path::new(""), "send");
    assert_eq!(node.count(&warmup, 1).await, 1);
    set_session_mode(&mut client, &sid, "prompt").await;
    let marker = format!("DIRECT_NODE_PAUSE_{sid}");
    let request = client
        .send_request_no_wait(
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": node.observer.self_id(), "message": marker, "summary": "hold the direct node worker"}),
        )
        .await;
    let (_, permission) = client
        .recv_until(Duration::from_secs(10), |message| {
            is_server_request(message, "session/request_permission")
        })
        .await
        .unwrap();
    gate.set_paused(true);
    allow_permission(&mut client, &permission["id"]).await;
    assert!(
        gate.wait_for_blocked(Duration::from_secs(5)),
        "real IO held open"
    );
    // A response here means the intended blocked-worker branch was not reached.
    assert!(client
        .recv_until(Duration::from_millis(150), |message| is_response_to(
            message, request
        ))
        .await
        .is_err());
    let pause = client
        .send_request_no_wait("_sudocode/session/pause_tools", json!({"sessionId": sid}))
        .await;
    let (seen, cancelled) = client
        .recv_until(Duration::from_secs(5), |message| {
            is_response_to(message, request)
        })
        .await
        .unwrap();
    assert_eq!(cancelled["error"]["code"], -32603, "{cancelled}");
    assert_eq!(cancelled["error"]["data"]["outcome"], "unknown");
    assert_eq!(
        cancelled["error"]["data"]["sudocode"]["directA2a"]["outcome"], "unknown",
        "cancellation must never return a partial result as if final: {cancelled}"
    );
    assert!(
        !seen.iter().any(|message| is_response_to(message, pause)),
        "pause must not acknowledge ahead of the worker: {seen:?}"
    );
    assert!(
        client
            .recv_until(Duration::from_millis(350), |message| is_response_to(
                message, pause
            ))
            .await
            .is_err(),
        "worker still held at IO gate"
    );
    gate.set_paused(false);
    let (_, paused) = client
        .recv_until(Duration::from_secs(10), |message| {
            is_response_to(message, pause)
        })
        .await
        .unwrap();
    assert_eq!(paused["result"]["isQuiescent"], true, "{paused}");
    let (_, refused) = client
        .send_request(
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": node.observer.self_id(), "message": marker, "summary": "refuse a paused direct send"}),
        )
        .await;
    assert_direct_not_submitted(&refused, "paused");
    assert!(
        node.count(&marker, 0).await <= 1,
        "no retry after cancellation"
    );
    model.assert_unused();
    client.shutdown().await;
    workspace.cleanup();
}

/// A real append can precede losing the ACP reply. The caller must preserve
/// Unknown; a disconnected request cannot safely be converted to `not_submitted`.
#[tokio::test]
#[ignore = "real TLS daemon; e2e/nexus-a2a/run.sh"]
async fn live_direct_a2a_lost_reply_does_not_undo_or_repeat_the_append() {
    let node = LiveNode::from_env();
    let model = NoInference::start().await;
    let workspace = TestWorkspace::new("direct-node-lost-reply");
    workspace.create();
    workspace.write_sudocode_json(&model.url);
    pin_agent_name(&workspace.root, &node.actor);
    fs::write(
        workspace.root.join("hold-reply.py"),
        r"import os, pathlib, time
if os.environ['HOOK_TOOL_NAME'] == 'send':
    pathlib.Path('executed.txt').write_text(os.environ['HOOK_TOOL_OUTPUT'])
    deadline = time.monotonic() + 30
    while not pathlib.Path('release.txt').exists() and time.monotonic() < deadline:
        time.sleep(0.02)
",
    )
    .unwrap();
    fs::write(
        workspace.root.join(".nexus/sudocode/settings.json"),
        json!({"agentName": node.actor, "hooks": {"PostToolUse": [python_hook("hold-reply.py")]}})
            .to_string(),
    )
    .unwrap();
    let (mut client, _home) = node.spawn(&workspace, &node.endpoint);
    scenario_initialize(&mut client).await;
    let sid = open_direct_session(&mut client, &workspace.root).await;
    set_session_mode(&mut client, &sid, "workspace-write").await;
    let marker = format!("DIRECT_NODE_LOST_REPLY_{sid}");
    let request = client
        .send_request_no_wait(
            "_sudocode/a2a/send",
            json!({"sessionId": sid, "to": node.observer.self_id(), "message": marker, "summary": "hold the reply after the real append"}),
        )
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !workspace.root.join("executed.txt").exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "post-execution hook not reached"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(node.count(&marker, 1).await, 1);
    assert!(
        client
            .recv_until(Duration::from_millis(100), |message| is_response_to(
                message, request
            ))
            .await
            .is_err(),
        "reply still withheld by the hook"
    );
    // Lose the sole stdio connection after the actual append but before a reply.
    client.shutdown().await;
    fs::write(
        workspace.root.join("release.txt"),
        "release orphaned test hook",
    )
    .unwrap();
    assert_eq!(node.count(&marker, 1).await, 1, "no automatic resend");
    model.assert_unused();
    workspace.cleanup();
}
