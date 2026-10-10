#![cfg(feature = "mailbox")]

//! Real provider -> Nexus model mount -> managed co-host -> controller mailbox.
//! Run with ANTHROPIC_API_KEY, ANTHROPIC_BASE_URL and optionally SUDOCODE_TEST_MODEL:
//! cargo test -p engine-acp --features mailbox --test session_mailbox_live -- --ignored --nocapture

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;

use a2a::session::{SessionEndpoint, SessionPayload, SessionSide};
use a2a::session_io::SessionMailbox;
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::{Kernel, OperationContext};
use runtime::{FsBackend, KernelFsBackend};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// These workflows change the process cwd and provider capture/config variables.
// Keep their complete lifetimes separate even with the default parallel runner.
static PROCESS_ENVIRONMENT: Mutex<()> = Mutex::new(());

struct RestoreEnvironment {
    cwd: std::path::PathBuf,
    variables: [(&'static str, Option<std::ffi::OsString>); 2],
}

impl RestoreEnvironment {
    fn capture() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap(),
            variables: ["SUDOCODE_DUMP_REQUESTS", "SUDO_CODE_CONFIG_HOME"]
                .map(|name| (name, std::env::var_os(name))),
        }
    }
}

impl Drop for RestoreEnvironment {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.cwd).unwrap();
        for (name, value) in &self.variables {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

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

struct CloseSession<'a>(&'a SessionMailbox<Kernel>);

impl Drop for CloseSession<'_> {
    fn drop(&mut self) {
        let _ = self.0.send(SessionPayload::Closed {
            reason: "live acceptance complete".into(),
        });
    }
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
        if system.contains("tasked with summarizing conversations") {
            assert!(body["tools"].as_array().is_none_or(Vec::is_empty));
            continue;
        }
        assert!(system.contains(&format!("/proc/{}/workspace", process_id)));
        assert!(system.contains("Nexus virtual filesystem (POSIX paths)"));
        assert!(
            system.contains("/agents/stock-worker/memory"),
            "memory must use the agent's VFS namespace"
        );
    }
    assert!(requests > 0, "no real provider request was captured");
}

fn live_text_turn(mailbox: &SessionMailbox<Kernel>, sid: &str, id: &str, prompt: &str) -> String {
    send(
        mailbox,
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt",
        "params":{"sessionId":sid,"prompt":[{"type":"text","text":prompt}]}}),
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut text = String::new();
    while Instant::now() < deadline {
        match mailbox.receive(100).unwrap() {
            Some(SessionPayload::Rpc { message }) => {
                assert_ne!(
                    message["method"], "session/request_permission",
                    "context-only workflow unexpectedly requested a tool: {message}"
                );
                if message["id"] == id {
                    assert!(message.get("error").is_none(), "{message}");
                    assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
                    return text;
                }
                if message["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
                    text.push_str(
                        message["params"]["update"]["content"]["text"]
                            .as_str()
                            .unwrap_or_default(),
                    );
                }
            }
            Some(SessionPayload::Closed { reason }) => panic!("closed: {reason}"),
            None => {}
        }
    }
    panic!("live text turn {id} timed out: {text}");
}

fn run_live_compaction(
    mailbox: &SessionMailbox<Kernel>,
    fs: &dyn FsBackend,
    sid: &str,
    model: &str,
) {
    let nonce = format!(
        "COMPACT_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let units = 9 + std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        % 17;
    let subtotal = units * 29 + 47;
    let total = subtotal + 13;
    // A real review packet must be larger than its continuation checkpoint.
    // Compacting just one short question should correctly preserve history
    // when the generated checkpoint would make it bigger.
    let ledger = (1..=80)
        .map(|index| {
            format!("Archived shipment {index}: warehouse aisle {}, parcel count {}, tracking ARCHIVE-{index:04}, carrier standard freight, dispatch scan verified, packaging inspected, delivery receipt reconciled. This completed shipment has no charge on the current order.\n", index % 12, index % 9 + 1)
        })
        .collect::<String>();
    let reviewed = live_text_turn(mailbox, sid, "order",
        &format!("Review this order using only this conversation: batch {nonce}, units {units}, unit price 29, delivery 47. Calculate units times price plus delivery and reply with the batch and subtotal. Use the supplied data without tools, files or persistent memory. The supporting shipment ledger below is archived and contributes no charges to this order.\n{ledger}"));
    assert!(
        reviewed.contains(&nonce) && reviewed.contains(&subtotal.to_string()),
        "{reviewed}"
    );
    let fee = live_text_turn(mailbox, sid, "fee",
        "Add a fee of 13 to the order subtotal for our final total. Reply with only the final numeric total; no tools are needed.");
    assert!(fee.contains(&total.to_string()), "{fee}");
    // The current review packet also gives token-based tail retention enough
    // recent context to leave the archived ledger in the compactable prefix.
    let checklist = (1..=48)
        .map(|index| {
            format!("Review check {index}: verify the carrier receipt and packaging inspection, reconcile archived shipment records, confirm the current order calculation uses its own prices and delivery charge, and mark the supporting paperwork ready for approval.\n")
        })
        .collect::<String>();
    let ready = live_text_turn(mailbox, sid, "review",
        &format!("Confirm the order is ready for review in one sentence using this review checklist. Do not repeat its identifier or amounts. No tools are needed.\n{checklist}"));
    assert!(!ready.trim().is_empty(), "no review confirmation");
    let compacted = live_text_turn(mailbox, sid, "compact", "/compact");
    eprintln!("[compaction] {compacted}");
    let requests: Vec<Value> = fs
        .readdir("/model")
        .unwrap()
        .into_iter()
        .filter(|entry| entry.name.ends_with(".prompt"))
        .map(|entry| {
            let path = if entry.name.starts_with('/') {
                entry.name
            } else {
                format!("/model/{}", entry.name)
            };
            serde_json::from_str(&fs.read_to_string(&path).unwrap()).unwrap()
        })
        .collect();
    let summary = requests
        .iter()
        .find(|request| {
            let body = &request["body"];
            body["system"]
                .to_string()
                .contains("tasked with summarizing conversations")
                || body["messages"]
                    .as_array()
                    .and_then(|messages| messages.last())
                    .is_some_and(|last| {
                        last["role"] == "user"
                            && last["content"].to_string().contains(
                                "Create a concise checkpoint for continuing this coding task.",
                            )
                    })
        })
        .expect("LLM compaction never crossed the actual Nexus model mount");
    assert_eq!(summary["nexus_http"]["path"], "v1/messages");
    assert_eq!(summary["body"]["model"], model);
    assert!(compacted.contains("llm summary"), "{compacted}");
    // Cache-preserving compaction keeps the original system and tool catalog.
    // The fallback instead uses a dedicated summary system without tools.
    if summary["body"]["system"]
        .to_string()
        .contains("tasked with summarizing conversations")
    {
        assert!(summary["body"]["tools"]
            .as_array()
            .is_none_or(Vec::is_empty));
    }
    assert!(
        summary["body"]["messages"].to_string().contains(&nonce),
        "summary input lost actual order data"
    );
    let recalled = live_text_turn(mailbox, sid, "recall",
        "State our reviewed order's batch code and final total, including the fee. Use the conversation history; no tools are needed.");
    assert!(
        recalled.contains(&nonce) && recalled.contains(&total.to_string()),
        "compaction lost actual order data: {recalled}"
    );
    eprintln!(
        "LIVE COMPACTION PASS: Nexus native summary and post-compaction batch/total verified"
    );
}

#[test]
#[ignore = "funded live integration: ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL required"]
fn controller_approves_real_model_work_and_reads_the_persisted_result() {
    controller_workflow(false);
}

#[test]
#[ignore = "funded live integration: ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL required"]
fn compaction_crosses_the_nexus_model_mount_and_preserves_live_order_data() {
    controller_workflow(true);
}

fn controller_workflow(compact: bool) {
    let _serial = PROCESS_ENVIRONMENT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let _environment = RestoreEnvironment::capture();
    std::env::set_var("SUDOCODE_DUMP_REQUESTS", &capture_dir);
    std::fs::write(
        config_home.path().join("AGENTS.md"),
        "HOST_DIRECTORY_MUST_NOT_ENTER_AGENT_PROMPT\n",
    )
    .unwrap();
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
    let _close = CloseSession(&mailbox);
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
    let fs = KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &AgentDescriptor {
            pid: format!("pid-{}", started["session_id"].as_str().unwrap()),
            name: "stock-worker".to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..AgentDescriptor::default()
        },
        format!(
            "/proc/{}/workspace",
            started["session_id"].as_str().unwrap()
        ),
    );
    if compact {
        run_live_compaction(&mailbox, &fs, sid, &model);
        assert_request_context(&capture_dir, started["session_id"].as_str().unwrap());
        return;
    }
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
