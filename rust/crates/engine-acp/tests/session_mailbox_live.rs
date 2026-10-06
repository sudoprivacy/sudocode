#![cfg(feature = "mailbox")]

//! Real provider -> Nexus model mount -> managed co-host -> controller mailbox.
//! Run with ANTHROPIC_API_KEY, ANTHROPIC_BASE_URL and optionally SUDOCODE_TEST_MODEL:
//! cargo test -p engine-acp --features mailbox --test session_mailbox_live -- --ignored --nocapture

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;

use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use kernel::kernel::{Kernel, OperationContext};
use runtime::{FsBackend, KernelFsBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn call(kernel: &Kernel, method: &str, payload: Value) -> Value {
    let ctx = OperationContext::new("test-owner", "root", false, Some("operator"), false);
    let bytes = kernel
        .dispatch_rust_call(
            "managed_agent",
            method,
            payload.to_string().as_bytes(),
            &ctx,
        )
        .expect("managed-agent service")
        .expect("managed-agent call");
    serde_json::from_slice(&bytes).unwrap()
}

fn send(mailbox: &SessionMailbox<Kernel>, message: Value) {
    mailbox.send(SessionPayload::Rpc { message }).unwrap();
}

fn response(mailbox: &SessionMailbox<Kernel>, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match mailbox.receive(100).unwrap() {
            Some(SessionPayload::Rpc { message }) if message["id"] == id => {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
            Some(SessionPayload::Closed { reason }) => panic!("closed: {reason}"),
            _ => {}
        }
    }
    panic!("no response to {id}");
}

fn assert_request_context(directory: &std::path::Path, process_id: &str) {
    let mut requests = 0;
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if !path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-messages.json")
        {
            continue;
        }
        requests += 1;
        let body: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let system = body["system"].to_string();
        assert!(!system.contains("HOST_DIRECTORY_MUST_NOT_ENTER_AGENT_PROMPT"));
        assert!(system.contains(&format!("/proc/{}/workspace", process_id)));
        assert!(system.contains("Nexus virtual filesystem (POSIX paths)"));
        assert!(
            system.contains("/agents/stock-worker/memory"),
            "memory must use the agent's VFS namespace"
        );
    }
    assert!(requests > 0, "no real provider request was captured");
}

#[test]
#[ignore = "funded live integration: ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL required"]
fn controller_approves_real_model_work_and_reads_the_persisted_result() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("live API key");
    let base_url = std::env::var("ANTHROPIC_BASE_URL").expect("live API base URL");
    let model = std::env::var("SUDOCODE_TEST_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());
    let config_home = tempfile::tempdir().unwrap();
    let request_bodies = tempfile::tempdir().unwrap();
    let capture_dir = match std::env::var_os("SUDOCODE_LIVE_ARTIFACTS") {
        Some(root) => {
            std::fs::create_dir_all(&root).unwrap();
            // Each run keeps its own requests. Older process IDs must not
            // contaminate assertions or be overwritten by a subsequent run.
            tempfile::Builder::new()
                .prefix("mailbox-live-")
                .tempdir_in(root)
                .unwrap()
                .keep()
        }
        None => request_bodies.path().to_owned(),
    };
    std::env::set_var("SUDOCODE_DUMP_REQUESTS", &capture_dir);
    std::fs::write(
        config_home.path().join("AGENTS.md"),
        "HOST_DIRECTORY_MUST_NOT_ENTER_AGENT_PROMPT\n",
    )
    .unwrap();
    struct RestoreCwd(std::path::PathBuf);
    impl Drop for RestoreCwd {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.0).unwrap();
        }
    }
    let _cwd = RestoreCwd(std::env::current_dir().unwrap());
    std::env::set_current_dir(config_home.path()).unwrap();
    std::fs::write(
        config_home.path().join("sudocode.json"),
        json!({
            "auth_modes":{"api-key":{"anthropic":{"baseUrl":"nexus:///model"}}},
            "models":{model.clone():{"alias":model,"name":"Live mailbox model","input":["text"],
                "providers":{"api-key":{"provider":"anthropic","model":model}}}}
        })
        .to_string(),
    )
    .unwrap();
    std::env::set_var("SUDO_CODE_CONFIG_HOME", config_home.path());
    let kernel = Arc::new(Kernel::new());
    managed_agent::install_managed_agent_with_spawn(
        &kernel,
        Arc::new(engine_acp::managed_agent::SudoCodeSpawnAdapter),
    )
    .unwrap();
    common::mount_agent_world(&kernel);
    let _provider = common::mount_model(&kernel, "anthropic", &base_url, &key);
    a2a::install_a2a_stamp_hook(&kernel, true).unwrap();
    let conversation =
        a2a::conversation_transcript_path(&a2a::conversation_id("stock-worker", "operator"));
    common::provision_stream_transcript(&kernel, &conversation);
    let started = call(
        &kernel,
        "start_session_v1",
        json!({"agent_id":"stock-worker","model":model}),
    );
    let endpoint: SessionEndpoint =
        serde_json::from_value(started["session_endpoint"].clone()).unwrap();
    let mailbox = SessionMailbox::open(
        Arc::clone(&kernel),
        OperationContext::new("test-owner", "root", false, Some("operator"), false),
        endpoint,
        SessionSide::Controller,
    )
    .unwrap();
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"init","method":"initialize",
        "params":{"protocolVersion":1,"clientCapabilities":{}}}),
    );
    assert_eq!(response(&mailbox, "init")["protocolVersion"], 1);
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"new","method":"session/new",
        "params":{"cwd":"/","mcpServers":[]}}),
    );
    let session = response(&mailbox, "new");
    let sid = session["sessionId"].as_str().unwrap();
    assert_eq!(started["durable_session_id"], sid);
    let fs = KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        "stock-worker",
        format!(
            "/proc/{}/workspace",
            started["session_id"].as_str().unwrap()
        ),
    );
    let nonce = format!(
        "inventory-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    fs.write(
        "inventory.json",
        json!({"batch":nonce,"items":[
            {"sku":"notebook","available":7,"required":19},
            {"sku":"pencil","available":23,"required":18},
            {"sku":"folder","available":2,"required":11}
        ]})
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    send(
        &mailbox,
        json!({"jsonrpc":"2.0","id":"inventory","method":"session/prompt",
        "params":{"sessionId":sid,"prompt":[{"type":"text","text":
            "Read inventory.json in the current workspace. For each item compute max(required - available, 0). Write restock.json with exactly the keys batch (copied from the input) and total_to_order (the sum of those deficits). Use the file tools to read and write the actual files, then confirm briefly."}]}}),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut permissions = 0;
    let mut completed_tools = 0;
    let mut text = String::new();
    let mut finished = false;
    while Instant::now() < deadline {
        match mailbox.receive(100).unwrap() {
            Some(SessionPayload::Rpc { message }) => {
                eprintln!("[mailbox] {message}");
                if message["method"] == "session/request_permission" {
                    if permissions == 0 {
                        assert_request_context(
                            &capture_dir,
                            started["session_id"].as_str().unwrap(),
                        );
                    }
                    permissions += 1;
                    assert!(permissions <= 12, "unexpected tool loop: {message}");
                    let input: Value = serde_json::from_str(
                        message["params"]["toolCall"]["rawInput"]
                            .as_str()
                            .unwrap_or("{}"),
                    )
                    .unwrap();
                    let requested = input["path"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches("./");
                    let workspace = format!(
                        "/proc/{}/workspace/",
                        started["session_id"].as_str().unwrap()
                    );
                    let relative = requested.strip_prefix(&workspace).unwrap_or(requested);
                    if !matches!(relative, "inventory.json" | "restock.json") {
                        mailbox
                            .send(SessionPayload::Closed {
                                reason: "unexpected tool outside the inventory task".into(),
                            })
                            .unwrap();
                        panic!("model requested a tool outside the isolated fixture: {input}");
                    }
                    if input.get("content").is_some()
                        && input["path"]
                            .as_str()
                            .is_some_and(|path| path.ends_with("restock.json"))
                    {
                        assert!(
                            fs.read_to_string("restock.json").is_err(),
                            "model output was written before controller approval"
                        );
                    }
                    let option = message["params"]["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|o| o["kind"] == "allow_once")
                        .unwrap()["optionId"]
                        .clone();
                    send(
                        &mailbox,
                        json!({"jsonrpc":"2.0","id":message["id"],
                        "result":{"outcome":{"outcome":"selected","optionId":option}}}),
                    );
                } else if message["id"] == "inventory" {
                    assert!(message.get("error").is_none(), "{message}");
                    assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
                    finished = true;
                    break;
                } else {
                    let update = &message["params"]["update"];
                    if update["status"] == "completed" {
                        completed_tools += 1;
                    }
                    if update["sessionUpdate"] == "agent_message_chunk" {
                        text.push_str(update["content"]["text"].as_str().unwrap_or_default());
                    }
                }
            }
            Some(SessionPayload::Closed { reason }) => panic!("closed: {reason}"),
            None => {}
        }
    }
    // Close before assertions too: a failed live assertion must release its agent.
    if !finished {
        eprintln!(
            "[session] {}",
            call(
                &kernel,
                "get_session_v1",
                json!({"session_id":started["session_id"]})
            )
        );
        eprintln!(
            "[persisted] {:?}",
            fs.read_to_string(&format!("/sessions/{sid}/transcript.jsonl"))
        );
    }
    mailbox
        .send(SessionPayload::Closed {
            reason: "live acceptance complete".into(),
        })
        .unwrap();
    assert!(finished, "live turn timed out: {text}");
    assert!(
        permissions > 0,
        "controller never received a tool permission request"
    );
    assert!(
        completed_tools >= 2,
        "expected real read and write tool completions"
    );
    let result: Value = serde_json::from_str(&fs.read_to_string("restock.json").unwrap()).unwrap();
    assert_eq!(result, json!({"batch":nonce,"total_to_order":21}));
    let transcript = fs
        .read_to_string(&format!("/sessions/{sid}/transcript.jsonl"))
        .unwrap();
    assert!(transcript.contains("inventory.json"));
    assert!(transcript.contains("restock.json"));
    assert!(
        transcript.contains(&nonce),
        "persisted transcript lacks actual input data"
    );
    assert!(
        !text.trim().is_empty(),
        "controller received no streamed answer"
    );
    assert_request_context(&capture_dir, started["session_id"].as_str().unwrap());
    eprintln!("LIVE MAILBOX PASS: permissions={permissions}, completed_tools={completed_tools}, persisted total=21");
}
