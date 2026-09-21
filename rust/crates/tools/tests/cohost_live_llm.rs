//! LIVE co-host proof 鈥?the real thing behind the whole in-process effort.
//!
//! Drives `tools::managed_agent::spawn_managed_agent` (the EXACT factory the
//! nexusd `SudoCodeSpawnAdapter` calls) on a real in-memory `Kernel`:
//! provisions the pair's conversation exactly as a sender would, spawns the
//! managed-agent loop IN-PROCESS, writes a user prompt into the shared
//! transcript, and reads the agent's reply back out of it. The agent's
//! LLM turn is a real network call through the configured provider.
//!
//! This is the co-host end-to-end minus the gRPC `StartSession` entry + the
//! adapter's enum mapping (both compile+link-verified in nexusd): a real
//! sudocode agent loop, co-hosted on a kernel, conversing over the mailbox
//! with a real LLM using in-process syscalls (no gRPC on its fs/mailbox path).
//!
//! `#[ignore]` 鈥?opt-in, needs a live LLM. Run with:
//!   ANTHROPIC_API_KEY=<sudorouter sk-鈥? \
//!   ANTHROPIC_BASE_URL=https://napi.sudorouter.ai \
//!   cargo test -p tools --test cohost_live_llm -- --ignored --nocapture

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kernel::core::agents::registry::{AgentDescriptor, AgentKind};
use kernel::kernel::{Kernel, OperationContext, ReadRequest, WriteRequest};
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::{FsBackend, KernelFsBackend};
use tools::managed_agent::spawn_managed_agent;

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

/// Mount what a conversation needs.
///
/// `/conversations` gets a CONTENT backend: a reader register is a DT_REG, and
/// a metadata-only mount keeps the metadata while losing the bytes, so every
/// read position would come back zero and the agent would replay its history
/// on each claim. `/agents` holds only the chat-list DT_LINKs, which are
/// metadata, so the plain mount is enough there.
fn mount_conversations(kernel: &Kernel) {
    kernel.vfs_router_arc().add_mount(
        "/conversations",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    kernel
        .vfs_router_arc()
        .add_mount("/agents", "root", None, false);
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
        .expect("user write to the transcript");
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
    mount_conversations(&kernel);
    let pid = "cohost-live-1";
    let agent_id = "scode-live";
    let desc = make_desc(pid, agent_id);

    // Provision the conversation the way a real sender does - through a
    // Mailbox over this same kernel - rather than planting entries by hand.
    // Hand-planting is how a test ends up exercising a shape production never
    // creates, and this one is here precisely to exercise the production path.
    let user_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        "user-test",
        "/".to_string(),
    ));
    let user_mb = Mailbox::daemon_absolute(user_fs, "user-test".to_string());
    user_mb
        .ensure_conversation(agent_id)
        .expect("provision the conversation with the co-host");

    // Spawn the REAL managed-agent loop 鈥?the same factory nexusd's
    // SudoCodeSpawnAdapter calls, and now with the same mailbox too: the
    // factory builds it from the descriptor, so there is no second place a
    // test could disagree with the daemon about what the agent listens on.
    // State transitions are printed, so the agent's lifecycle is observable.
    let handle = spawn_managed_agent(Arc::clone(&kernel), desc, |state, reason| {
        eprintln!("[agent state] {state:?} reason={reason:?}");
    });

    let ctx = user_ctx();
    // Both sides append here; the agent's reply lands in the same log.
    let cwm = InboxConvention::new(String::new()).transcript_path("user-test", agent_id);
    let prompt = "You are being tested over a nexus A2A mailbox. \
                  Reply with exactly one word: PONG";
    eprintln!("[user 鈫?agent] {prompt}");
    write_prompt(&kernel, &cwm, &ctx, "user-test", agent_id, prompt);

    let reply = wait_for_agent_reply(&kernel, &cwm, &ctx, agent_id, Duration::from_secs(90));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("no agent reply within 90s 鈥?LLM turn did not complete");
    let body = reply
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    eprintln!("[agent 鈫?user] {body}");

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
