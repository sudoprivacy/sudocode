//! Actual ACP child and authenticated HTTP MCP requests with server budgets.

use super::*;

async fn delayed_server(proof: &Path) -> (Child, String) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http_mcp.py");
    let mut child = Command::new(python::resolve_python())
        .args(["-u", script.to_str().expect("fixture path")])
        .arg(proof)
        .args(["fixture-budget-token", "0.75"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start real delayed HTTP MCP server");
    let mut ready = BufReader::new(child.stdout.take().expect("MCP readiness"));
    let mut url = String::new();
    timeout(SERVER_STARTUP_TIMEOUT, ready.read_line(&mut url))
        .await
        .expect("MCP readiness deadline")
        .expect("MCP readiness read");
    assert!(url.starts_with("http://127.0.0.1:"));
    (child, url.trim().to_string())
}

fn server(url: &str, budget: Option<u64>) -> Value {
    let mut server = json!({"name":"parity", "type":"http", "url":url,
        "headers":[{"name":"Authorization", "value":"Bearer fixture-budget-token"}]});
    if let Some(budget) = budget {
        server["_meta"] = json!({"sudocode":{"toolCallTimeoutMs":budget}});
    }
    server
}

async fn echo(client: &mut AcpTestClient, session: &str) -> String {
    let (updates, response) = client
        .send_request(
            "session/prompt",
            json!({
                "sessionId":session,
                "prompt":[{"type":"text", "text":format!("{SCENARIO_PREFIX}mcp_tool_roundtrip")}]
            }),
        )
        .await;
    assert!(response.get("error").is_none(), "prompt: {response}");
    serde_json::to_string(&updates).expect("ACP updates")
}

#[tokio::test]
async fn acp_mcp_timeout_is_scoped_and_survives_model_switch_and_reload() {
    let model = MockAnthropicService::spawn().await.expect("model fixture");
    let workspace = TestWorkspace::new("mcp-server-budget");
    workspace.create();
    workspace.write_sudocode_json(&model.base_url());
    let proof = workspace.root.join("remote-calls.jsonl");
    let (mut remote, url) = delayed_server(&proof).await;
    let mut client = spawn_stdio_client_danger(&workspace);
    let (_, initialized) = client
        .send_request("initialize", json!({"protocolVersion":1}))
        .await;
    assert_eq!(
        initialized["result"]["_meta"]["sudocode"]["mcpToolCallTimeout"]["version"],
        1
    );
    let (_, created) = client
        .send_request(
            "session/new",
            json!({
                "cwd":workspace.root, "mcpServers":[server(&url, Some(100))]
            }),
        )
        .await;
    let short = created["result"]["sessionId"]
        .as_str()
        .expect("short-budget session");
    let failed = echo(&mut client, short).await;
    assert!(
        failed.contains("is_error=true"),
        "short budget must expire: {failed}"
    );
    assert!(
        !failed.contains("echo:hello from mcp parity"),
        "timed-out result must not be fabricated"
    );

    let (_, saved) = client
        .send_request(
            "session/new",
            json!({
                "cwd":workspace.root, "mcpServers":[server(&url, Some(100))]
            }),
        )
        .await;
    let saved = saved["result"]["sessionId"]
        .as_str()
        .expect("saved session");
    // Changing the timeout in the same server configuration must rebuild the
    // connection. Otherwise the manager retains the previous 100 ms budget.
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({
                "sessionId":saved, "cwd":workspace.root, "mcpServers":[server(&url, Some(5_000))]
            }),
        )
        .await;
    assert!(loaded.get("error").is_none(), "reload: {loaded}");
    let (_, switched) = client
        .send_request(
            "session/set_model",
            json!({"sessionId":saved, "modelId":"haiku"}),
        )
        .await;
    assert!(switched.get("error").is_none(), "switch: {switched}");
    assert!(echo(&mut client, saved)
        .await
        .contains("echo:hello from mcp parity"));

    let (_, created) = client
        .send_request(
            "session/new",
            json!({
                "cwd":workspace.root, "mcpServers":[server(&url, None)]
            }),
        )
        .await;
    let ordinary = created["result"]["sessionId"]
        .as_str()
        .expect("default-budget session");
    assert!(echo(&mut client, ordinary)
        .await
        .contains("echo:hello from mcp parity"));
    let calls = fs::read_to_string(proof).expect("real remote receipts");
    assert_eq!(
        calls
            .lines()
            .filter(
                |line| serde_json::from_str::<Value>(line).expect("receipt")["method"]
                    == "tools/call"
            )
            .count(),
        3
    );
    client.shutdown().await;
    remote.kill().await.expect("stop owned remote fixture");
    workspace.cleanup();
}

#[tokio::test]
async fn acp_rejects_invalid_mcp_timeout_before_connecting() {
    let model = MockAnthropicService::spawn().await.expect("model fixture");
    let workspace = TestWorkspace::new("mcp-invalid-budget");
    workspace.create();
    workspace.write_sudocode_json(&model.base_url());
    let mut client = spawn_stdio_client_danger(&workspace);
    scenario_initialize(&mut client).await;
    for transport in ["http", "sse", "stdio"] {
        for invalid in [json!(0), json!(-1), json!("5000"), json!(1.5)] {
            let mut config = server("http://127.0.0.1:1/mcp", None);
            config["type"] = json!(transport);
            if transport == "stdio" {
                config.as_object_mut().expect("config").remove("type");
                config["command"] = json!(python::resolve_python());
                config["args"] = json!([]);
                config["env"] = json!([]);
            }
            config["_meta"] = json!({"sudocode":{"toolCallTimeoutMs":invalid}});
            let (_, rejected) = client
                .send_request(
                    "session/new",
                    json!({
                        "cwd":workspace.root, "mcpServers":[config]
                    }),
                )
                .await;
            assert_eq!(rejected["error"]["code"], -32602, "{transport}: {rejected}");
            assert!(rejected["error"].to_string().contains("toolCallTimeoutMs"));
        }
    }
    client.shutdown().await;
    workspace.cleanup();
}
