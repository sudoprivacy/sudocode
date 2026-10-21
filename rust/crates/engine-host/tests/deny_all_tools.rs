//! Deny-all is stronger than session-injected tool availability.
use std::collections::{BTreeMap, BTreeSet};

use engine_core::EngineDelegate;
use engine_host::{SessionEngine, SessionLifecycle};
use runtime::{
    ContentBlock, PermissionPromptDecision, PermissionPrompter, PermissionRequest, RuntimeObserver,
};

struct Allow;
impl PermissionPrompter for Allow {
    fn decide(&mut self, _: &PermissionRequest) -> PermissionPromptDecision {
        PermissionPromptDecision::Allow
    }
}
struct Observe;
impl RuntimeObserver for Observe {}

const MCP: &str = r"
import json,sys,pathlib
for line in sys.stdin:
    req=json.loads(line)
    method=req.get('method')
    if method=='initialize':
        result={'protocolVersion':req['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'parity','version':'1'}}
    elif method=='tools/list':
        pathlib.Path(sys.argv[1]+'.listed').write_text('discovered')
        result={'tools':[{'name':'echo','inputSchema':{'type':'object','properties':{'text':{'type':'string'}}}}]}
    elif method=='tools/call':
        pathlib.Path(sys.argv[1]).write_text('executed')
        result={'content':[{'type':'text','text':'echo:hello from mcp parity'}]}
    else:
        continue
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}),flush=True)
";

#[test]
fn injected_mcp_cannot_widen_empty_allow_list_on_build_or_rebuild() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mock = rt
        .block_on(mock_anthropic_service::MockAnthropicService::spawn())
        .unwrap();
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", home.path());
    let model = |name: &str| {
        serde_json::json!({"alias":name,"name":name,"input":["text"],
        "providers":{"api-key":{"provider":"anthropic","model":"claude-sonnet-4-6"}}})
    };
    std::fs::write(
        home.path().join("sudocode.json"),
        serde_json::json!({
            "auth_modes":{"api-key":{"anthropic":{"baseUrl":mock.base_url(),"apiKey":"test-key"}}},
            "models":{"first":model("first"),"second":model("second")}
        })
        .to_string(),
    )
    .unwrap();
    let script = workspace.path().join("mcp.py");
    std::fs::write(&script, MCP).unwrap();
    let effect = workspace.path().join("effect");
    let servers = BTreeMap::from([(
        "parity".into(),
        runtime::ScopedMcpServerConfig {
            scope: runtime::ConfigSource::Local,
            config: runtime::McpServerConfig::Stdio(runtime::McpStdioServerConfig {
                command: "python3".into(),
                args: vec![
                    script.to_string_lossy().into(),
                    effect.to_string_lossy().into(),
                ],
                env: BTreeMap::new(),
                current_dir: None,
                tool_call_timeout_ms: Some(5000),
            }),
        },
    )]);
    let build = |allowed| {
        SessionEngine::build(
            workspace.path(),
            &servers,
            runtime::SystemPromptOverrides::default(),
            runtime::memory::MemoryMode::Disabled,
            runtime::SystemPrompt::default(),
            "first".into(),
            Some("first".into()),
            Some(allowed),
            Some(runtime::PermissionMode::DangerFullAccess),
            None,
            Some(engine_core::AuthMode::ApiKey),
        )
        .unwrap()
    };
    let run = |engine: &SessionEngine| {
        engine
            .run_turn(
                vec![ContentBlock::Text {
                    text: "PARITY_SCENARIO:mcp_tool_roundtrip".into(),
                }],
                &mut Observe,
                &mut Allow,
            )
            .unwrap();
    };
    let denied = build(BTreeSet::new());
    assert!(
        effect.with_extension("listed").exists(),
        "MCP discovery must have run"
    );
    run(&denied);
    SessionLifecycle::set_model(&denied, "second").unwrap();
    denied.reset_session().unwrap();
    run(&denied);
    assert!(!effect.exists(), "injected MCP executed despite deny-all");
    let requests = rt.block_on(mock.captured_requests());
    assert!(requests.len() >= 4);
    for request in &requests {
        let body: serde_json::Value = serde_json::from_str(&request.raw_body).unwrap();
        assert!(body["tools"].is_null() || body["tools"].as_array().is_some_and(Vec::is_empty));
        assert!(!request.raw_body.contains("echo:hello from mcp parity"));
    }
    assert!(requests.iter().any(|r| r.raw_body.contains("not enabled")));
    denied.close();
    // Existing non-empty allow-lists still permit explicitly injected MCP.
    let allowed = build(BTreeSet::from(["read_file".into()]));
    run(&allowed);
    assert!(effect.exists());
    allowed.close();
}
