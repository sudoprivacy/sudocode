//! LIVE co-host proof — the real thing behind the whole in-process effort.
//!
//! Drives `tools::managed_agent::spawn_managed_agent` (the EXACT factory the
//! nexusd `SudoCodeSpawnAdapter` calls) on a real in-memory `Kernel`: plants
//! the `/proc/{pid}/chat-with-me` DT_STREAM the daemon would stamp, spawns the
//! managed-agent loop IN-PROCESS, writes a user prompt to the mailbox via
//! `sys_write`, and reads back the agent's reply via `sys_read`. The agent's
//! LLM turn is a real network call through the configured provider.
//!
//! This is the co-host end-to-end minus the gRPC `StartSession` entry + the
//! adapter's enum mapping (both compile+link-verified in nexusd): a real
//! sudocode agent loop, co-hosted on a kernel, conversing over the mailbox
//! with a real LLM using in-process syscalls (no gRPC on its fs/mailbox path).
//!
//! `#[ignore]` — opt-in, needs a live LLM. Run with:
//!   ANTHROPIC_API_KEY=<sudorouter sk-…> \
//!   ANTHROPIC_BASE_URL=https://napi.sudorouter.ai \
//!   cargo test -p tools --test cohost_live_llm -- --ignored --nocapture

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kernel::core::agents::registry::{AgentDescriptor, AgentKind};
use kernel::kernel::{Kernel, OperationContext, ReadRequest, WriteRequest};
use runtime::spawn_task::Mailbox;
use tools::managed_agent::spawn_managed_agent;

const DT_STREAM: i32 = 4;
const STREAM_CAPACITY: usize = 65_536;

/// Model the agent runs, read from its descriptor `model` label. SudoRouter
/// serves `claude-sonnet-4-6`; override via `SUDOCODE_TEST_MODEL`.
fn test_model() -> String {
    std::env::var("SUDOCODE_TEST_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".to_string())
}

fn mount_proc(kernel: &Kernel) {
    kernel
        .vfs_router_arc()
        .add_mount("/proc", "root", None, false);
}

#[allow(clippy::too_many_arguments)]
fn plant_chat_stream(kernel: &Kernel, pid: &str) {
    let path = format!("/proc/{pid}/chat-with-me");
    kernel
        .sys_setattr(
            &path,
            DT_STREAM,
            "",
            None,
            None,
            None,
            "memory",
            "root",
            false,
            STREAM_CAPACITY,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("plant /proc/{pid}/chat-with-me DT_STREAM");
}

fn make_desc(pid: &str, name: &str) -> AgentDescriptor {
    let mut desc = AgentDescriptor {
        pid: pid.to_string(),
        name: name.to_string(),
        kind: AgentKind::Managed,
        owner_id: "test-owner".to_string(),
        zone_id: "root".to_string(),
        ..Default::default()
    };
    desc.labels.insert("model".to_string(), test_model());
    desc
}

fn user_ctx() -> OperationContext {
    OperationContext::new("test-user", "root", false, Some("user-test"), true)
}

fn write_prompt(
    kernel: &Kernel,
    path: &str,
    ctx: &OperationContext,
    from: &str,
    to: &str,
    body: &str,
) {
    let env = serde_json::json!({ "from": from, "to": to, "body": body });
    let reqs = [WriteRequest {
        path: path.to_string(),
        content: serde_json::to_vec(&env).unwrap(),
        offset: 0,
    }];
    kernel
        .sys_write(&reqs, ctx)
        .pop()
        .expect("sys_write empty")
        .expect("user write to chat-with-me");
}

/// Poll the mailbox until an envelope from `agent_id` with a non-empty body
/// arrives, or `timeout` elapses.
fn wait_for_agent_reply(
    kernel: &Kernel,
    path: &str,
    ctx: &OperationContext,
    agent_id: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + timeout;
    let mut offset = 0u64;
    while Instant::now() < deadline {
        let reqs = [ReadRequest {
            path: path.to_string(),
            offset,
            len: None,
            timeout_ms: 0,
        }];
        if let Some(Ok(result)) = kernel.sys_read(&reqs, ctx).pop() {
            if let Some(bytes) = result.data.as_ref() {
                if !bytes.is_empty() {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
                        if v.get("from").and_then(|f| f.as_str()) == Some(agent_id) {
                            let body = v.get("body").and_then(|b| b.as_str()).unwrap_or("");
                            if !body.is_empty() {
                                return Some(v);
                            }
                        }
                    }
                }
            }
            if let Some(next) = result.stream_next_offset {
                offset = next as u64;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
#[ignore = "live LLM: set ANTHROPIC_API_KEY + ANTHROPIC_BASE_URL (sudorouter)"]
fn cohost_agent_replies_via_mailbox_with_real_llm() {
    if std::env::var("ANTHROPIC_API_KEY").is_err()
        && std::env::var("PROXY_AUTH_TOKEN").is_err()
        && std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_err()
    {
        eprintln!("SKIP: no LLM credentials in env");
        return;
    }

    let kernel = Arc::new(Kernel::new());
    mount_proc(&kernel);
    let pid = "cohost-live-1";
    let agent_id = "scode-live";
    plant_chat_stream(&kernel, pid);
    let desc = make_desc(pid, agent_id);

    // Spawn the REAL managed-agent loop — the same factory nexusd's
    // SudoCodeSpawnAdapter calls. Node-local single-stream mailbox (the
    // agent reads + replies on /proc/{pid}/chat-with-me). State transitions
    // are printed so WarmingUp → Ready → Busy → Ready is observable.
    let mailbox = Mailbox::LocalStream {
        path: format!("/proc/{pid}/chat-with-me"),
        self_id: agent_id.to_string(),
    };
    let handle = spawn_managed_agent(Arc::clone(&kernel), desc, mailbox, |state, reason| {
        eprintln!("[agent state] {state:?} reason={reason:?}");
    });

    let ctx = user_ctx();
    let cwm = format!("/proc/{pid}/chat-with-me");
    let prompt = "You are being tested over a nexus A2A mailbox. \
                  Reply with exactly one word: PONG";
    eprintln!("[user → agent] {prompt}");
    write_prompt(&kernel, &cwm, &ctx, "user-test", agent_id, prompt);

    let reply = wait_for_agent_reply(&kernel, &cwm, &ctx, agent_id, Duration::from_secs(90));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("no agent reply within 90s — LLM turn did not complete");
    let body = reply
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    eprintln!("[agent → user] {body}");

    assert_eq!(
        reply.get("from").and_then(|f| f.as_str()),
        Some(agent_id),
        "reply must come from the co-hosted agent"
    );
    assert!(
        reply.get("error").is_none(),
        "expected a real LLM reply, got an error envelope: {body}"
    );
    assert!(
        body.to_ascii_uppercase().contains("PONG"),
        "LLM reply did not contain the requested token; got: {body}"
    );
}
