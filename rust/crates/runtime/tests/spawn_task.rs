//! Integration tests for `runtime::spawn_task` — drive the REAL v2
//! `run_loop` (the exact production loop the co-host runs), NOT a scaffold.
//!
//! A scripted mock [`ApiClient`] returns one fixed text turn so the loop's
//! mailbox mechanics are exercised deterministically with no network: inbound
//! envelope parse, `from != self` self-filtering, reply routing for BOTH
//! [`Mailbox`] variants, abort teardown, and the transient-read survival
//! contract — a durable A2A inbox must NOT die on a read error / on being
//! read before it exists (the regression guard for the co-host boot race,
//! where the loop is spawned before the mint has planted the inbox).
//!
//! Lives outside the lib's `#[cfg(test)] mod` so it compiles as its own test
//! binary; it uses the crate's normal deps (`async-trait`, `futures`).

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use kernel::core::agents::registry::{AgentDescriptor, AgentKind};
use kernel::kernel::{Kernel, OperationContext, ReadRequest, WriteRequest};
use runtime::spawn_task::{spawn_task, Mailbox, SpawnHandle};
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, ModelFamilyIdentity,
    PermissionMode, PermissionPolicy, RuntimeError, SystemPromptBuilder, ToolError, ToolExecutor,
};

const DT_STREAM: i32 = 4;
const STREAM_CAPACITY: usize = 65_536;
/// Fixed text the scripted provider replies with — asserted end-to-end.
const REPLY_TEXT: &str = "PONG";

// ── Mock provider: one fixed text turn, no tool calls ──────────────────

/// [`ApiClient`] that streams a single `TextDelta` + `MessageStop`, so a
/// turn resolves to the fixed [`REPLY_TEXT`] with zero network I/O.
struct ScriptedReply;

#[async_trait]
impl ApiClient for ScriptedReply {
    async fn stream(&mut self, _request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(AssistantEvent::TextDelta(REPLY_TEXT.to_string())),
            Ok(AssistantEvent::MessageStop),
        ])))
    }
}

/// The scripted turn emits no `ToolUse`, so the executor is never invoked;
/// a call would be a bug in the loop, so it fails loudly.
struct NoTools;

impl ToolExecutor for NoTools {
    async fn execute(&self, tool_name: &str, _input: &str) -> Result<String, ToolError> {
        Err(ToolError::new(format!(
            "unexpected tool call in test: {tool_name}"
        )))
    }
}

// ── Kernel / mailbox helpers ───────────────────────────────────────────

fn mount(kernel: &Kernel, mount_point: &str) {
    kernel
        .vfs_router_arc()
        .add_mount(mount_point, "root", None, false);
}

fn plant_stream(kernel: &Kernel, path: &str) {
    kernel
        .sys_setattr(
            path,
            DT_STREAM,
            /* backend_name */ "",
            /* backend */ None,
            /* metastore */ None,
            /* raft_backend */ None,
            /* io_profile */ "memory",
            /* zone_id */ "root",
            /* is_external */ false,
            STREAM_CAPACITY,
            /* read_fd */ None,
            /* write_fd */ None,
            /* mime_type */ None,
            /* modified_at_ms */ None,
            /* content_id */ None,
            /* size */ None,
            /* version */ None,
            /* created_at_ms */ None,
            /* link_target */ None,
            /* source */ None,
            /* remote_metastore */ None,
        )
        .expect("plant DT_STREAM");
}

fn make_desc(pid: &str, name: &str) -> AgentDescriptor {
    AgentDescriptor {
        pid: pid.to_string(),
        name: name.to_string(),
        kind: AgentKind::Managed,
        owner_id: "test-owner".to_string(),
        zone_id: "root".to_string(),
        ..Default::default()
    }
}

/// Spawn the REAL `run_loop` (via `spawn_task`) with the scripted mock —
/// the exact loop the co-host runs, minus the network provider.
fn spawn_real(kernel: Arc<Kernel>, desc: AgentDescriptor, mailbox: Mailbox) -> SpawnHandle {
    let system_prompt = SystemPromptBuilder::new()
        .with_model_family(ModelFamilyIdentity::Claude)
        .build();
    spawn_task(
        kernel,
        desc,
        mailbox,
        ScriptedReply,
        NoTools,
        system_prompt,
        PermissionPolicy::new(PermissionMode::Allow),
        |_state| {},
    )
}

fn user_ctx() -> OperationContext {
    OperationContext::new("test-user", "root", false, Some("user-test"), true)
}

fn write_envelope(
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
        .expect("sys_write returned empty vec")
        .expect("write envelope");
}

/// Poll `path` until an envelope `from` the given author with a non-empty
/// body arrives, or `timeout` elapses.
fn wait_for_reply(
    kernel: &Kernel,
    path: &str,
    ctx: &OperationContext,
    from: &str,
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
                        let is_from = v.get("from").and_then(|f| f.as_str()) == Some(from);
                        let has_body = v
                            .get("body")
                            .and_then(|b| b.as_str())
                            .is_some_and(|b| !b.is_empty());
                        if is_from && has_body {
                            return Some(v);
                        }
                    }
                }
            }
            if let Some(next) = result.stream_next_offset {
                offset = next as u64;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Count envelopes on `path` authored by `from`, walking the whole stream.
fn count_from(kernel: &Kernel, path: &str, ctx: &OperationContext, from: &str) -> usize {
    let mut offset = 0u64;
    let mut count = 0;
    loop {
        let reqs = [ReadRequest {
            path: path.to_string(),
            offset,
            len: None,
            timeout_ms: 0,
        }];
        match kernel.sys_read(&reqs, ctx).pop() {
            Some(Ok(result)) => {
                if let Some(bytes) = result.data.as_ref() {
                    if !bytes.is_empty() {
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
                            if v.get("from").and_then(|f| f.as_str()) == Some(from) {
                                count += 1;
                            }
                        }
                    }
                }
                let next = result.stream_next_offset.map_or(offset, |o| o as u64);
                if next == offset {
                    break;
                }
                offset = next;
            }
            _ => break,
        }
    }
    count
}

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
fn local_stream_round_trip_drives_real_run_loop() {
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/proc");
    let path = "/proc/pid-ls/chat-with-me";
    plant_stream(&kernel, path);
    let handle = spawn_real(
        Arc::clone(&kernel),
        make_desc("pid-ls", "scode"),
        Mailbox::LocalStream {
            path: path.to_string(),
            self_id: "scode".to_string(),
        },
    );

    let ctx = user_ctx();
    write_envelope(&kernel, path, &ctx, "user-test", "scode", "hello");
    let reply = wait_for_reply(&kernel, path, &ctx, "scode", Duration::from_secs(5));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("real run_loop produced no reply on the shared stream");
    assert_eq!(reply.get("body").and_then(|b| b.as_str()), Some(REPLY_TEXT));
    assert_eq!(reply.get("to").and_then(|t| t.as_str()), Some("user-test"));
}

#[test]
fn a2a_reads_own_inbox_and_replies_to_senders_inbox() {
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/agents");
    plant_stream(&kernel, "/agents/win-ai/chat-with-me");
    plant_stream(&kernel, "/agents/user-test/chat-with-me");
    let handle = spawn_real(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
    );

    let ctx = user_ctx();
    // Sender writes to win-ai's OWN inbox …
    write_envelope(
        &kernel,
        "/agents/win-ai/chat-with-me",
        &ctx,
        "user-test",
        "win-ai",
        "hi",
    );
    // … and the reply must land in the SENDER's inbox, not win-ai's.
    let reply = wait_for_reply(
        &kernel,
        "/agents/user-test/chat-with-me",
        &ctx,
        "win-ai",
        Duration::from_secs(5),
    );
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("A2A co-host produced no reply in the sender's inbox");
    assert_eq!(reply.get("body").and_then(|b| b.as_str()), Some(REPLY_TEXT));
    assert_eq!(reply.get("to").and_then(|t| t.as_str()), Some("user-test"));
    // The reply went to the sender's box, so win-ai's own inbox holds only
    // the inbound (no self-directed reply).
    assert_eq!(
        count_from(&kernel, "/agents/win-ai/chat-with-me", &ctx, "win-ai"),
        0,
        "reply must route to the sender's inbox, not the agent's own"
    );
}

#[test]
fn loop_exits_on_abort_signal() {
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/proc");
    let path = "/proc/pid-abort/chat-with-me";
    plant_stream(&kernel, path);
    let handle = spawn_real(
        Arc::clone(&kernel),
        make_desc("pid-abort", "scode"),
        Mailbox::LocalStream {
            path: path.to_string(),
            self_id: "scode".to_string(),
        },
    );
    // No message sent — the loop is parked on `sys_watch`. abort() must let
    // it exit on the next `while !abort` check (≤ one watch timeout).
    handle.abort_signal.abort();

    let watcher = thread::Builder::new()
        .spawn(move || handle.join.join())
        .expect("watcher thread");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !watcher.is_finished() {
        assert!(
            Instant::now() < deadline,
            "run_loop did not exit within 3s of abort()"
        );
        thread::sleep(Duration::from_millis(20));
    }
    let _ = watcher.join();
}

#[test]
fn skips_own_writes_no_reply_storm() {
    // LocalStream reads AND replies on the same path, so the agent sees its
    // own reply on the next poll; `from == self` filtering must stop it from
    // replying to itself (which would explode the mailbox).
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/proc");
    let path = "/proc/pid-filter/chat-with-me";
    plant_stream(&kernel, path);
    let handle = spawn_real(
        Arc::clone(&kernel),
        make_desc("pid-filter", "scode"),
        Mailbox::LocalStream {
            path: path.to_string(),
            self_id: "scode".to_string(),
        },
    );

    let ctx = user_ctx();
    write_envelope(&kernel, path, &ctx, "user-test", "scode", "ping");
    let _ = wait_for_reply(&kernel, path, &ctx, "scode", Duration::from_secs(5))
        .expect("first agent reply did not arrive");
    // Settle several watch cycles so any self-reply bug would have written by now.
    thread::sleep(Duration::from_millis(600));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    assert_eq!(
        count_from(&kernel, path, &ctx, "scode"),
        1,
        "agent replied to its own message — the from==self filter is broken"
    );
}

#[test]
fn a2a_inbox_survives_read_before_it_exists() {
    // F1 regression: the co-host boot spawns the loop bound to
    // `/agents/<self>/chat-with-me`, which the mint may not have planted yet
    // (or which a fresh raft replica hasn't resolved). The OLD loop broke on
    // the first `Err` and the agent silently died for the daemon's lifetime.
    // The loop must SURVIVE the transient error and serve the message once
    // the inbox appears.
    let kernel = Arc::new(Kernel::new());
    // Deliberately do NOT mount /agents yet → the loop's first reads all Err
    // (NotMounted). The old `Err(_) => break` would kill the loop here.
    let handle = spawn_real(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
    );
    // Let the loop spin on the error path across several watch cycles.
    thread::sleep(Duration::from_millis(300));

    // Now the "mint" lands: mount + plant the inbox and the sender's box.
    mount(&kernel, "/agents");
    plant_stream(&kernel, "/agents/win-ai/chat-with-me");
    plant_stream(&kernel, "/agents/user-test/chat-with-me");
    let ctx = user_ctx();
    write_envelope(
        &kernel,
        "/agents/win-ai/chat-with-me",
        &ctx,
        "user-test",
        "win-ai",
        "hi",
    );

    let reply = wait_for_reply(
        &kernel,
        "/agents/user-test/chat-with-me",
        &ctx,
        "win-ai",
        Duration::from_secs(5),
    );
    handle.abort_signal.abort();
    let _ = handle.join.join();
    assert!(
        reply.is_some(),
        "loop died on the pre-mint read error (F1 regression) — no reply after the inbox appeared"
    );
}
