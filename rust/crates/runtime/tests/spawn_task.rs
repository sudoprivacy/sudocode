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
use runtime::spawn_task::{
    mailbox_sender, spawn_task, Mailbox, MailboxEnvelope, MailboxSender, SpawnHandle,
};
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, PermissionMode, PermissionPolicy,
    RuntimeError, SystemPromptBuilder, ToolError, ToolExecutor,
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

/// [`ApiClient`] whose one turn calls the `send_message` tool addressed to
/// `to` with `body` — the co-host's DELIBERATE reply path. Emits a single
/// `ToolUse` + `MessageStop` so the loop drives the tool without network I/O.
struct SendsReply {
    to: String,
    body: String,
    /// Whether the one `send_message` call has been issued. The turn's tool
    /// loop calls `stream` again after executing the tool; that follow-up round
    /// must end the turn (no further tool) — otherwise the agent would send on
    /// every round forever within a single turn.
    sent: bool,
}

#[async_trait]
impl ApiClient for SendsReply {
    async fn stream(&mut self, _request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        if self.sent {
            // Follow-up round after the tool result: end the turn, no more tools.
            return Ok(Box::pin(futures::stream::iter(vec![Ok(
                AssistantEvent::MessageStop,
            )])));
        }
        self.sent = true;
        let input = serde_json::json!({ "to": self.to, "body": self.body }).to_string();
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(AssistantEvent::ToolUse {
                id: "call-1".to_string(),
                name: "send_message".to_string(),
                input,
                thought_signature: None,
            }),
            Ok(AssistantEvent::MessageStop),
        ])))
    }
}

/// Executor mirroring the production `ManagedToolExecutor` send path: routes
/// `send_message` through the [`MailboxSender`] (the SSOT send-write), so a
/// scripted `send_message` turn actually writes to the recipient's inbox.
struct SendingTools {
    send: MailboxSender,
}

impl ToolExecutor for SendingTools {
    async fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        assert_eq!(tool_name, "send_message", "unexpected tool");
        let v: serde_json::Value =
            serde_json::from_str(input).map_err(|e| ToolError::new(e.to_string()))?;
        let to = v.get("to").and_then(|x| x.as_str()).expect("to");
        let body = v.get("body").and_then(|x| x.as_str()).expect("body");
        (self.send)(to, body).map_err(ToolError::new)?;
        Ok("delivered".to_string())
    }
}

// ── Kernel / mailbox helpers ───────────────────────────────────────────

fn mount(kernel: &Kernel, mount_point: &str) {
    kernel
        .vfs_router_arc()
        .add_mount(mount_point, "root", None, false);
}

/// Mount `mount_point` backed by an in-memory `ObjectStore` so DT_REG **content**
/// (not just metadata) round-trips. The `None`-backend `mount` above carries
/// only metadata, so a DT_REG write "succeeds" but the read returns FileNotFound
/// — the durable cursor is a DT_REG, so its persistence needs a content backend
/// (production uses host-fs at `/`).
fn mount_with_backend(kernel: &Kernel, mount_point: &str) {
    kernel.vfs_router_arc().add_mount(
        mount_point,
        "root",
        Some(Arc::new(MemStore::default())),
        false,
    );
}

/// Minimal PAS-style in-memory `ObjectStore` for tests: stores DT_REG content by
/// its (path-derived) content_id. Only the three required trait methods.
#[derive(Default)]
struct MemStore {
    blobs: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl kernel::abc::object_store::ObjectStore for MemStore {
    fn name(&self) -> &str {
        "memtest"
    }
    fn write_content(
        &self,
        content: &[u8],
        content_id: &str,
        _ctx: &OperationContext,
        offset: u64,
    ) -> Result<kernel::abc::object_store::WriteResult, kernel::abc::object_store::StorageError>
    {
        let mut blobs = self.blobs.lock().unwrap();
        let entry = blobs.entry(content_id.to_string()).or_default();
        if offset == 0 {
            entry.clear();
        }
        let off = offset as usize;
        let end = off + content.len();
        if entry.len() < end {
            entry.resize(end, 0);
        }
        entry[off..end].copy_from_slice(content);
        Ok(kernel::abc::object_store::WriteResult {
            content_id: content_id.to_string(),
            version: String::new(),
            size: entry.len() as u64,
        })
    }
    fn read_content(
        &self,
        content_id: &str,
        _ctx: &OperationContext,
    ) -> Result<Vec<u8>, kernel::abc::object_store::StorageError> {
        self.blobs
            .lock()
            .unwrap()
            .get(content_id)
            .cloned()
            .ok_or_else(|| {
                kernel::abc::object_store::StorageError::NotFound(content_id.to_string())
            })
    }
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
    let system_prompt = SystemPromptBuilder::new().build();
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

/// Spawn the REAL `run_loop` with a scripted `send_message` turn: on each
/// inbound message the agent DELIBERATELY replies `reply_body` to `reply_to`
/// via the mailbox sender (the production reply path).
fn spawn_sending(
    kernel: Arc<Kernel>,
    desc: AgentDescriptor,
    mailbox: Mailbox,
    reply_to: &str,
    reply_body: &str,
) -> SpawnHandle {
    let system_prompt = SystemPromptBuilder::new().build();
    let send = mailbox_sender(
        Arc::clone(&kernel),
        mailbox.clone(),
        desc.owner_id.clone(),
        desc.zone_id.clone(),
    );
    spawn_task(
        kernel,
        desc,
        mailbox,
        SendsReply {
            to: reply_to.to_string(),
            body: reply_body.to_string(),
            sent: false,
        },
        SendingTools { send },
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
    let env = MailboxEnvelope {
        from: from.to_string(),
        to: to.to_string(),
        body: body.to_string(),
    };
    let reqs = [WriteRequest {
        path: path.to_string(),
        content: env.to_bytes(),
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

/// Read the durable inbox cursor for `agent`, or 0 if unset. White-box: the
/// co-host cursor is a node-local DT_REG at `/.cohost-cursor-<agent>`, written
/// with the agent's own ctx (owner `test-owner`, per [`make_desc`]); read it
/// back the same way so ownership matches regardless of any perm enforcement.
fn read_cursor(kernel: &Kernel, agent: &str) -> u64 {
    let ctx = OperationContext::new("test-owner", "root", false, Some(agent), true);
    let path = format!("/.cohost-cursor-{agent}");
    kernel
        .sys_read(
            &[ReadRequest {
                path,
                offset: 0,
                len: None,
                timeout_ms: 0,
            }],
            &ctx,
        )
        .pop()
        .and_then(|r| r.ok())
        .and_then(|r| r.data)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// The tail (next unread offset) of stream `path` — walk to the end and return
/// the final `stream_next_offset`, so a test can gate on "fully drained" without
/// depending on whether offsets count messages or bytes.
fn tail_offset(kernel: &Kernel, path: &str, ctx: &OperationContext) -> u64 {
    let mut offset = 0u64;
    loop {
        let reqs = [ReadRequest {
            path: path.to_string(),
            offset,
            len: None,
            timeout_ms: 0,
        }];
        match kernel.sys_read(&reqs, ctx).pop() {
            Some(Ok(result)) => {
                let next = result.stream_next_offset.map_or(offset, |o| o as u64);
                if next == offset {
                    break;
                }
                offset = next;
            }
            _ => break,
        }
    }
    offset
}

// ── Tests ──────────────────────────────────────────────────────────────

#[test]
fn local_stream_round_trip_drives_real_run_loop() {
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/proc");
    let path = "/proc/pid-ls/chat-with-me";
    plant_stream(&kernel, path);
    let handle = spawn_sending(
        Arc::clone(&kernel),
        make_desc("pid-ls", "scode"),
        Mailbox::LocalStream {
            path: path.to_string(),
            self_id: "scode".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );

    let ctx = user_ctx();
    write_envelope(&kernel, path, &ctx, "user-test", "scode", "hello");
    let reply = wait_for_reply(&kernel, path, &ctx, "scode", Duration::from_secs(5));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("agent's send_message produced no reply on the shared stream");
    assert_eq!(reply.get("body").and_then(|b| b.as_str()), Some(REPLY_TEXT));
    assert_eq!(reply.get("to").and_then(|t| t.as_str()), Some("user-test"));
}

#[test]
fn a2a_reads_own_inbox_and_replies_to_senders_inbox() {
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/agents");
    plant_stream(&kernel, "/agents/win-ai/chat-with-me");
    plant_stream(&kernel, "/agents/user-test/chat-with-me");
    let handle = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
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
    // No message sent — the loop is parked on the blocking `sys_read` tail.
    // abort() must let it exit on the next `while !abort` check (≤ one read
    // block timeout).
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
    let handle = spawn_sending(
        Arc::clone(&kernel),
        make_desc("pid-filter", "scode"),
        Mailbox::LocalStream {
            path: path.to_string(),
            self_id: "scode".to_string(),
        },
        "user-test",
        REPLY_TEXT,
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
    let handle = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
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

#[test]
fn text_only_turn_writes_no_reply_the_ping_pong_fix() {
    // THE ping-pong fix: a turn that produces TEXT but does NOT call
    // `send_message` must write nothing back. The old loop harvested the turn's
    // text and auto-forwarded it, so every message bounced a reply forever; now
    // silence (no `send_message`) lets the exchange end.
    let kernel = Arc::new(Kernel::new());
    mount(&kernel, "/agents");
    plant_stream(&kernel, "/agents/win-ai/chat-with-me");
    plant_stream(&kernel, "/agents/user-test/chat-with-me");
    // `spawn_real` = ScriptedReply (text only) + NoTools (send_message never called).
    let handle = spawn_real(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
    );

    let ctx = user_ctx();
    write_envelope(
        &kernel,
        "/agents/win-ai/chat-with-me",
        &ctx,
        "user-test",
        "win-ai",
        "hi",
    );
    // Give the loop ample time to run the turn and (wrongly) auto-forward.
    let leaked = wait_for_reply(
        &kernel,
        "/agents/user-test/chat-with-me",
        &ctx,
        "win-ai",
        Duration::from_secs(2),
    );
    handle.abort_signal.abort();
    let _ = handle.join.join();

    assert!(
        leaked.is_none(),
        "a text-only turn auto-forwarded a reply — the ping-pong (auto-forward) is back"
    );
    assert_eq!(
        count_from(&kernel, "/agents/win-ai/chat-with-me", &ctx, "win-ai"),
        0,
        "the agent wrote to its own inbox on a silent turn"
    );
}

#[test]
fn probe_dt_reg_round_trips_on_test_mount() {
    // Isolation probe: does a DT_REG (the cursor's shape) actually persist +
    // read back on the test's `/` mount? If not, the respawn test's failure is
    // a test-mount artifact, not the fix.
    let kernel = Arc::new(Kernel::new());
    mount_with_backend(&kernel, "/");
    let ctx = user_ctx();
    let w = kernel
        .sys_write(
            &[WriteRequest {
                path: "/.probe-cursor".to_string(),
                content: b"42".to_vec(),
                offset: 0,
            }],
            &ctx,
        )
        .pop()
        .expect("write vec empty");
    assert!(w.is_ok(), "DT_REG write failed on test mount");
    let r = kernel
        .sys_read(
            &[ReadRequest {
                path: "/.probe-cursor".to_string(),
                offset: 0,
                len: None,
                timeout_ms: 0,
            }],
            &ctx,
        )
        .pop()
        .expect("read vec empty")
        .expect("read err");
    assert_eq!(
        r.data.as_deref(),
        Some(&b"42"[..]),
        "DT_REG content did not persist/round-trip on the test `/` mount"
    );
}

#[test]
fn respawn_resumes_from_durable_cursor_and_does_not_replay_history() {
    // #81 root fix: a RESPAWNED co-host agent must resume past what it already
    // processed — via its durable node-local cursor — NOT replay the whole inbox
    // and re-answer every historical message (the storm seen live when Mac
    // respawned mac-ai and it re-answered the entire conversation).
    let kernel = Arc::new(Kernel::new());
    mount_with_backend(&kernel, "/"); // durable cursor lives at /.cohost-cursor-<name>
    mount(&kernel, "/agents");
    plant_stream(&kernel, "/agents/win-ai/chat-with-me");
    plant_stream(&kernel, "/agents/user-test/chat-with-me");
    let ctx = user_ctx();

    // Spawn #1: deliver + reply to one message; the cursor advances + persists.
    let h1 = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai-1", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );
    write_envelope(
        &kernel,
        "/agents/win-ai/chat-with-me",
        &ctx,
        "user-test",
        "win-ai",
        "first",
    );
    let r1 = wait_for_reply(
        &kernel,
        "/agents/user-test/chat-with-me",
        &ctx,
        "win-ai",
        Duration::from_secs(5),
    );
    assert!(r1.is_some(), "spawn #1 did not reply to its message");
    thread::sleep(Duration::from_millis(200)); // let the cursor save land
    h1.abort_signal.abort();
    let _ = h1.join.join();
    assert_eq!(
        count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai"),
        1,
        "spawn #1 should reply exactly once"
    );

    // Spawn #2 = RESPAWN of the SAME identity into the SAME inbox (still holds
    // "first"). The durable cursor must make it resume PAST "first" → no re-reply.
    let h2 = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai-2", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );
    thread::sleep(Duration::from_millis(700)); // ample time to (wrongly) replay
    h2.abort_signal.abort();
    let _ = h2.join.join();
    assert_eq!(
        count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai"),
        1,
        "respawn re-replied to an already-processed message — durable cursor not honored (#81 storm)"
    );

    // Liveness: a NEW message after respawn IS answered (the cursor didn't
    // over-skip live traffic, the way a naive seek-to-tail would).
    write_envelope(
        &kernel,
        "/agents/win-ai/chat-with-me",
        &ctx,
        "user-test",
        "win-ai",
        "second",
    );
    let h3 = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai-3", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );
    // Poll for a SECOND reply (spawn #1's is still in the inbox, so a plain
    // "any reply?" check would spuriously succeed on the stale one).
    let deadline = Instant::now() + Duration::from_secs(5);
    while count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai") < 2
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }
    h3.abort_signal.abort();
    let _ = h3.join.join();
    assert_eq!(
        count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai"),
        2,
        "exactly two replies total ('first' + 'second'); the respawn replayed NONE"
    );
}

#[test]
fn respawn_resumes_past_silently_processed_messages_not_only_replied_ones() {
    // Deepens the #81 fix past a single REPLIED message: a co-host agent usually
    // reads a message and stays SILENT (the ping-pong fix — it only replies when
    // it calls `send_message`). Those silently-processed messages must ALSO
    // advance the durable cursor; if the cursor advanced only on messages that
    // produced a reply, a respawn would re-read every silent one and re-answer it.
    let kernel = Arc::new(Kernel::new());
    mount_with_backend(&kernel, "/"); // durable cursor at /.cohost-cursor-win-ai
    mount(&kernel, "/agents");
    plant_stream(&kernel, "/agents/win-ai/chat-with-me");
    plant_stream(&kernel, "/agents/user-test/chat-with-me");
    let ctx = user_ctx();

    // Three inbound messages. `SendsReply` replies to the FIRST only (its `sent`
    // latch), so m1 and m2 are processed SILENTLY — the case under test.
    for body in ["m0", "m1", "m2"] {
        write_envelope(
            &kernel,
            "/agents/win-ai/chat-with-me",
            &ctx,
            "user-test",
            "win-ai",
            body,
        );
    }
    let h1 = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai-1", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );

    // Deterministically wait for spawn #1 to DRAIN all three. The silent ones
    // leave no observable reply, so gate on the cursor reaching the inbox tail.
    // (Aborting early would leave m1/m2 genuinely unprocessed, and the respawn
    // then handling them would be CORRECT, not a replay — a flaky false failure.)
    let tail = tail_offset(&kernel, "/agents/win-ai/chat-with-me", &ctx);
    let deadline = Instant::now() + Duration::from_secs(8);
    while read_cursor(&kernel, "win-ai") < tail && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        read_cursor(&kernel, "win-ai"),
        tail,
        "spawn #1 did not drain all three inbound messages before respawn"
    );
    h1.abort_signal.abort();
    let _ = h1.join.join();
    assert_eq!(
        count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai"),
        1,
        "spawn #1 should reply exactly once (m0); m1/m2 are processed silently"
    );

    // Respawn: the cursor sits PAST all three, incl. the two silent ones. A fresh
    // `SendsReply` (sent=false) would re-answer anything it re-reads.
    let h2 = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai-2", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );
    thread::sleep(Duration::from_millis(700)); // ample time to (wrongly) replay
    h2.abort_signal.abort();
    let _ = h2.join.join();
    assert_eq!(
        count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai"),
        1,
        "respawn re-answered a silently-processed message — the cursor advanced \
         only on replies, not on every processed message (#81)"
    );

    // Liveness: a message AFTER the three IS answered → the cursor resumed at the
    // true tail, not over-skipped the way a naive seek-to-tail would.
    write_envelope(
        &kernel,
        "/agents/win-ai/chat-with-me",
        &ctx,
        "user-test",
        "win-ai",
        "m3",
    );
    let h3 = spawn_sending(
        Arc::clone(&kernel),
        make_desc("cohost-win-ai-3", "win-ai"),
        Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: "win-ai".to_string(),
        },
        "user-test",
        REPLY_TEXT,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai") < 2
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }
    h3.abort_signal.abort();
    let _ = h3.join.join();
    assert_eq!(
        count_from(&kernel, "/agents/user-test/chat-with-me", &ctx, "win-ai"),
        2,
        "the post-respawn message m3 was not answered — cursor over-skipped the live tail"
    );
}
