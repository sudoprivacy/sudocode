//! Integration tests for `runtime::spawn_task` 鈥?drive the REAL v2
//! `run_loop` (the exact production loop the co-host runs), NOT a scaffold.
//!
//! A scripted mock [`ApiClient`] returns one fixed text turn so the loop's
//! CohostMailbox mechanics are exercised deterministically with no network: inbound
//! envelope parse, `from != self` self-filtering, reply routing for BOTH
//! [`CohostMailbox`] variants, abort teardown, and the transient-read survival
//! contract 鈥?a durable A2A inbox must NOT die on a read error / on being
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
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::spawn_task::{spawn_task, MailboxEnvelope, MailboxSender, SpawnHandle};
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, ConversationRuntime, FsBackend,
    KernelFsBackend, PermissionMode, PermissionPolicy, RuntimeError, Session, SystemPromptBuilder,
    ToolError, ToolExecutor,
};

const DT_STREAM: i32 = 4;
const STREAM_CAPACITY: usize = 65_536;
/// Fixed text the scripted provider replies with 鈥?asserted end-to-end.
const REPLY_TEXT: &str = "PONG";

// 鈹€鈹€ Mock provider: one fixed text turn, no tool calls 鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€

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
/// `to` with `body` 鈥?the co-host's DELIBERATE reply path. Emits a single
/// `ToolUse` + `MessageStop` so the loop drives the tool without network I/O.
struct SendsReply {
    to: String,
    body: String,
    /// Whether the one `send_message` call has been issued. The turn's tool
    /// loop calls `stream` again after executing the tool; that follow-up round
    /// must end the turn (no further tool) 鈥?otherwise the agent would send on
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

/// Executor mirroring the production send path: routes `send_message` through
/// the [`MailboxSender`] (the SSOT send-write), so a scripted `send_message`
/// turn actually writes to the recipient's inbox.
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

// 鈹€鈹€ Kernel / CohostMailbox helpers 鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€

fn mount(kernel: &Kernel, mount_point: &str) {
    kernel
        .vfs_router_arc()
        .add_mount(mount_point, "root", None, false);
}

/// Mount `mount_point` backed by an in-memory `ObjectStore` so DT_REG **content**
/// (not just metadata) round-trips. The `None`-backend `mount` above carries
/// only metadata, so a DT_REG write "succeeds" but the read returns FileNotFound
/// 鈥?the durable cursor is a DT_REG, so its persistence needs a content backend
/// (production uses host-fs at `/`).
fn mount_with_backend(kernel: &Kernel, mount_point: &str) {
    kernel.vfs_router_arc().add_mount(
        mount_point,
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
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

/// The mailbox a co-hosted agent runs on, built the way production builds it
/// (`tools::managed_agent::spawn_managed_agent`): its own name over the
/// in-process VFS backend, through the same `for_cohost` constructor. A test
/// that assembled backend + convention by hand could drift onto a pairing the
/// daemon never uses, and prove nothing about the daemon.
fn cohost_mailbox(kernel: &Arc<Kernel>, desc: &AgentDescriptor) -> Arc<Mailbox> {
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(kernel),
        &desc.owner_id,
        &desc.zone_id,
        &desc.name,
        format!("/proc/{}/workspace", desc.pid),
    ));
    Arc::new(Mailbox::daemon_absolute(fs, desc.name.clone()))
}

/// The loop's host-side root. A per-test directory, for the same reason the
/// co-host gives each agent its own: a shared one is a shared `bash` cwd and a
/// shared git repository.
fn shell_root(desc: &AgentDescriptor) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spawn-task-shell-{}-{}",
        std::process::id(),
        desc.pid
    ));
    std::fs::create_dir_all(&dir).expect("shell root");
    dir
}

/// Spawn the REAL `run_loop` (via `spawn_task`) with the scripted mock — the
/// exact loop the co-host runs, minus the network provider.
fn spawn_real(kernel: &Arc<Kernel>, desc: &AgentDescriptor) -> SpawnHandle {
    spawn_task(
        desc,
        cohost_mailbox(kernel, desc),
        test_runtime(ScriptedReply, NoTools),
        (),
        shell_root(desc),
        |_state, _reason| {},
    )
}

/// The engine the loop drives.
///
/// A host builds this — the co-host through `engine_host`, which cannot be
/// reached from here without inverting the dependency — so these tests assemble
/// the same `ConversationRuntime` directly from the pieces a host would supply.
fn test_runtime<C, T>(api_client: C, tool_executor: T) -> ConversationRuntime<C, T>
where
    C: runtime::ApiClient + 'static,
    T: runtime::ToolExecutor + 'static,
{
    ConversationRuntime::new(
        Session::new(),
        api_client,
        tool_executor,
        PermissionPolicy::new(PermissionMode::Allow),
        SystemPromptBuilder::new().build(),
    )
}

/// Spawn the REAL `run_loop` with a scripted `send` turn: on each inbound
/// message the agent DELIBERATELY replies `reply_body` to `reply_to` through
/// the mailbox's own sender — the production reply path, and now the same
/// object the loop receives on.
fn spawn_sending(
    kernel: &Arc<Kernel>,
    desc: &AgentDescriptor,
    reply_to: &str,
    reply_body: &str,
) -> SpawnHandle {
    let mailbox = cohost_mailbox(kernel, desc);
    let send = mailbox.sender();
    spawn_task(
        desc,
        mailbox,
        test_runtime(
            SendsReply {
                to: reply_to.to_string(),
                body: reply_body.to_string(),
                sent: false,
            },
            SendingTools { send },
        ),
        (),
        shell_root(desc),
        |_state, _reason| {},
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
        summary: None,
        timestamp: 0,
        color: None,
        kind: String::new(),
        request_id: None,
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

/// How long a wait for something that SHOULD happen is given.
///
/// Generous on purpose. The happy path returns as soon as the value appears —
/// this whole suite runs in about two seconds locally — so the budget is only
/// ever spent on a run that is already failing. Five seconds was enough locally
/// and not enough on a loaded Windows CI runner sharing the box with nine other
/// spawn threads, where the timeout read as "the agent never replied".
///
/// Waits that assert ABSENCE do not use this: there the wall-clock IS the test,
/// and every second would be paid on every green run.
const HAPPENS_BUDGET: Duration = Duration::from_secs(30);

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

/// The transcript two agents share, derived the way BOTH sides derive it. A
/// literal path here would assert against a location the code never writes to.
fn transcript_of(a: &str, b: &str) -> String {
    InboxConvention::new(String::new()).transcript_path(a, b)
}

/// `agent`'s chat-list entry for `peer` — what the receiver lists to learn
/// which conversations to tail.
fn chat_list_of(agent: &str, peer: &str) -> String {
    InboxConvention::new(String::new()).chat_list_path(agent, peer)
}

/// Mount what a conversation needs.
///
/// `/conversations` gets a CONTENT backend, not the metadata-only mount: a
/// reader register is a DT_REG, and over the `None`-backend mount its write
/// "succeeds" while the read comes back FileNotFound — so every position would
/// read as zero and every restart would replay. `/agents` holds only DT_LINKs,
/// which are metadata, so the plain mount is enough there.
fn mount_conversations(kernel: &Kernel) {
    mount_with_backend(kernel, "/conversations");
    mount(kernel, "/agents");
}

fn plant_dir(kernel: &Kernel, path: &str) {
    let _ = kernel.sys_setattr(
        path,
        i32::from(kernel::meta_store::DT_DIR),
        /* backend_name */ "",
        /* backend */ None,
        /* metastore */ None,
        /* raft_backend */ None,
        /* io_profile */ "",
        /* zone_id */ "root",
        /* is_external */ false,
        0,
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
    );
}

fn plant_link(kernel: &Kernel, alias: &str, target: &str) {
    kernel
        .sys_setattr(
            alias,
            i32::from(kernel::meta_store::DT_LINK),
            /* backend_name */ "",
            /* backend */ None,
            /* metastore */ None,
            /* raft_backend */ None,
            /* io_profile */ "",
            /* zone_id */ "root",
            /* is_external */ false,
            0,
            /* read_fd */ None,
            /* write_fd */ None,
            /* mime_type */ None,
            /* modified_at_ms */ None,
            /* content_id */ None,
            /* size */ None,
            /* version */ None,
            /* created_at_ms */ None,
            Some(target),
            /* source */ None,
            /* remote_metastore */ None,
        )
        .expect("plant DT_LINK");
}

/// Provision a conversation the way a sender's first `send` does: the shared
/// transcript, plus the chat-list entry under BOTH agents.
///
/// Both entries, because the side that has to DISCOVER the conversation is the
/// one that did not provision it. Planting only the sender's would leave the
/// co-host with an empty chat list and nothing to tail — which looks exactly
/// like a broken receiver.
fn plant_conversation(kernel: &Kernel, a: &str, b: &str) {
    let root = InboxConvention::new(String::new()).conversation_root(a, b);
    plant_dir(kernel, "/conversations");
    plant_dir(kernel, &root);
    plant_stream(kernel, &transcript_of(a, b));
    plant_dir(kernel, "/agents");
    for (owner, other) in [(a, b), (b, a)] {
        plant_dir(kernel, &format!("/agents/{owner}"));
        plant_dir(kernel, &format!("/agents/{owner}/conversations"));
        plant_link(kernel, &chat_list_of(owner, other), &root);
    }
}

/// Read `agent`'s durable position in its conversation with `peer`, or 0 if it
/// has none yet.
///
/// White-box: the register is a DT_REG holding the reader's JSON, written with
/// the agent's own ctx (owner `test-owner`, per [`make_desc`]); read it back
/// the same way so ownership matches regardless of any perm enforcement.
fn read_position(kernel: &Kernel, agent: &str, peer: &str) -> u64 {
    let ctx = OperationContext::new("test-owner", "root", false, Some(agent), true);
    let path = InboxConvention::new(String::new()).reader_path(agent, peer, agent);
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
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("read_offset").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

/// The tail (next unread offset) of stream `path` 鈥?walk to the end and return
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

// 鈹€鈹€ Tests 鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€鈹€

#[test]
fn a_missing_path_reports_not_found() {
    // The two backends must agree here. `StdFsBackend` reports a missing file
    // as `ErrorKind::NotFound` and callers branch on it, so a kernel-backed
    // read that reported the same condition as `Other` turns "this reader has
    // no position yet" into "this read failed": the receiver never claims its
    // seat and hears nothing, while the error scrolls past as a retry. That is
    // exactly how it failed.
    //
    // The classification reads the kernel error's debug text (the type is
    // opaque at that boundary), so this test is what keeps a renamed variant
    // from breaking it silently.
    let kernel = Arc::new(Kernel::new());
    mount_with_backend(&kernel, "/");
    let desc = make_desc("probe", "probe-agent");
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        &desc.owner_id,
        &desc.zone_id,
        &desc.name,
        "/".to_string(),
    ));

    let err = fs
        .read("/definitely-not-here")
        .expect_err("a missing path must not read back as success");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "a kernel-backed read reported a missing path as {:?}, so every caller \
         that branches on NotFound silently takes the error path instead: {err}",
        err.kind()
    );
}

#[test]
fn a_reply_lands_in_the_shared_transcript() {
    let kernel = Arc::new(Kernel::new());
    mount_conversations(&kernel);
    plant_conversation(&kernel, "win-ai", "user-test");
    let desc = make_desc("cohost-win-ai", "win-ai");
    let handle = spawn_sending(&kernel, &desc, "user-test", REPLY_TEXT);

    let ctx = user_ctx();
    let transcript = transcript_of("win-ai", "user-test");
    write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", "hi");
    let reply = wait_for_reply(&kernel, &transcript, &ctx, "win-ai", HAPPENS_BUDGET);
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.expect("the co-host produced no reply in the shared transcript");
    assert_eq!(reply.get("body").and_then(|b| b.as_str()), Some(REPLY_TEXT));
    assert_eq!(reply.get("to").and_then(|t| t.as_str()), Some("user-test"));
    // Exactly one. Both sides append to ONE log, so an agent that answered its
    // own reply would show up here as a second `from: win-ai` entry rather than
    // as traffic in some other inbox where nothing was looking.
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        1,
        "the agent replied more than once to a single inbound message"
    );
}

#[test]
fn loop_exits_on_abort_signal() {
    let kernel = Arc::new(Kernel::new());
    mount_conversations(&kernel);
    plant_conversation(&kernel, "scode", "user-test");
    let desc = make_desc("pid-abort", "scode");
    let handle = spawn_real(&kernel, &desc);
    // No message sent: the tail is parked on the transcript and the discovery
    // loop is sleeping between listings. abort() has to bring BOTH down — the
    // receiver only joins once every tail it started has joined.
    handle.abort_signal.abort();

    let watcher = thread::Builder::new()
        .spawn(move || handle.join.join())
        .expect("watcher thread");
    let deadline = Instant::now() + HAPPENS_BUDGET;
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
    // Both sides append to ONE transcript, so the agent reads its own reply
    // back on the next poll; `from == self` filtering is what stops it
    // answering itself forever. Conversations make this MORE load-bearing than
    // the per-recipient inboxes did: there a reply went to a path the agent was
    // not reading, so the filter could be broken and nothing would show it.
    let kernel = Arc::new(Kernel::new());
    mount_conversations(&kernel);
    plant_conversation(&kernel, "scode", "user-test");
    let desc = make_desc("pid-filter", "scode");
    let handle = spawn_sending(&kernel, &desc, "user-test", REPLY_TEXT);

    let ctx = user_ctx();
    let transcript = transcript_of("scode", "user-test");
    write_envelope(&kernel, &transcript, &ctx, "user-test", "scode", "ping");
    let _ = wait_for_reply(&kernel, &transcript, &ctx, "scode", HAPPENS_BUDGET)
        .expect("first agent reply did not arrive");
    // Settle several poll cycles so a self-reply bug would have written by now.
    thread::sleep(Duration::from_millis(600));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "scode"),
        1,
        "the agent answered its own message: the from==self filter is broken"
    );
}

#[test]
fn the_loop_survives_a_conversation_that_does_not_exist_yet() {
    // F1 regression: the co-host boot spawns the loop before the conversation
    // is there — a peer that has not sent yet, a fresh raft replica that has
    // not resolved it. The OLD loop broke on the first `Err` and the agent
    // silently died for the daemon's lifetime. It must SURVIVE and serve the
    // message once the conversation appears.
    let kernel = Arc::new(Kernel::new());
    // Deliberately unmounted: every listing and every read Errs (NotMounted).
    let desc = make_desc("cohost-win-ai", "win-ai");
    let handle = spawn_sending(&kernel, &desc, "user-test", REPLY_TEXT);
    // Let the receiver spin on the error path across several cycles.
    thread::sleep(Duration::from_millis(300));

    // Now the peer's first send lands: mount and provision.
    mount_conversations(&kernel);
    plant_conversation(&kernel, "win-ai", "user-test");
    let ctx = user_ctx();
    let transcript = transcript_of("win-ai", "user-test");
    write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", "hi");

    let reply = wait_for_reply(&kernel, &transcript, &ctx, "win-ai", HAPPENS_BUDGET);
    handle.abort_signal.abort();
    let _ = handle.join.join();
    assert!(
        reply.is_some(),
        "the receiver died on the pre-provision error (F1 regression): no reply \
         after the conversation appeared"
    );
}

#[test]
fn text_only_turn_writes_no_reply_the_ping_pong_fix() {
    // THE ping-pong fix: a turn that produces TEXT but does NOT call `send`
    // must write nothing back. The old loop harvested the turn's text and
    // auto-forwarded it, so every message bounced a reply forever; now silence
    // lets the exchange end.
    let kernel = Arc::new(Kernel::new());
    mount_conversations(&kernel);
    plant_conversation(&kernel, "win-ai", "user-test");
    // `spawn_real` = ScriptedReply (text only) + NoTools (`send` never called).
    let desc = make_desc("cohost-win-ai", "win-ai");
    let handle = spawn_real(&kernel, &desc);

    let ctx = user_ctx();
    let transcript = transcript_of("win-ai", "user-test");
    write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", "hi");
    // Ample time for the loop to run the turn and (wrongly) auto-forward. NOT
    // `HAPPENS_BUDGET`: this asserts that nothing arrives, so the wall-clock is
    // the test and every second of it is paid on every green run.
    let leaked = wait_for_reply(&kernel, &transcript, &ctx, "win-ai", Duration::from_secs(2));
    handle.abort_signal.abort();
    let _ = handle.join.join();

    assert!(
        leaked.is_none(),
        "a text-only turn auto-forwarded a reply: the ping-pong is back"
    );
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        0,
        "the agent wrote to the transcript on a silent turn"
    );
}

#[test]
fn probe_dt_reg_round_trips_on_test_mount() {
    // Isolation probe: does a DT_REG (the reader register's shape) actually
    // persist + read back on the test's `/` mount? If not, the respawn tests'
    // failures are a test-mount artifact, not the fix.
    let kernel = Arc::new(Kernel::new());
    mount_with_backend(&kernel, "/");
    let ctx = user_ctx();
    let w = kernel
        .sys_write(
            &[WriteRequest {
                path: "/.probe-register".to_string(),
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
                path: "/.probe-register".to_string(),
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
fn respawn_resumes_from_the_durable_position_and_does_not_replay() {
    // #81 root fix: a RESPAWNED co-host must resume past what it already
    // processed, NOT replay the conversation and re-answer every historical
    // message (the storm seen live when Mac respawned mac-ai and it re-answered
    // the whole exchange).
    //
    // What makes it durable now is WHERE the position lives: in the
    // conversation, beside the transcript. The node-local cursor file it
    // replaced did not survive the agent being restarted on another node.
    let kernel = Arc::new(Kernel::new());
    mount_conversations(&kernel);
    plant_conversation(&kernel, "win-ai", "user-test");
    let ctx = user_ctx();
    let transcript = transcript_of("win-ai", "user-test");

    // Spawn #1: deliver + reply to one message; the position advances.
    let d1 = make_desc("cohost-win-ai-1", "win-ai");
    let h1 = spawn_sending(&kernel, &d1, "user-test", REPLY_TEXT);
    write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", "first");
    let r1 = wait_for_reply(&kernel, &transcript, &ctx, "win-ai", HAPPENS_BUDGET);
    assert!(r1.is_some(), "spawn #1 did not reply to its message");
    thread::sleep(Duration::from_millis(200)); // let the commit land
    h1.abort_signal.abort();
    let _ = h1.join.join();
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        1,
        "spawn #1 should reply exactly once"
    );

    // Spawn #2 = RESPAWN of the SAME identity onto the SAME conversation, which
    // still holds "first". The durable position must make it resume PAST it.
    let d2 = make_desc("cohost-win-ai-2", "win-ai");
    let h2 = spawn_sending(&kernel, &d2, "user-test", REPLY_TEXT);
    thread::sleep(Duration::from_millis(700)); // ample time to (wrongly) replay
    h2.abort_signal.abort();
    let _ = h2.join.join();
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        1,
        "the respawn re-answered an already-processed message: the durable \
         position was not honoured (#81 storm)"
    );

    // Liveness: a NEW message after the respawn IS answered, so the position
    // did not over-skip live traffic the way a seek-to-tail would.
    write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", "second");
    let d3 = make_desc("cohost-win-ai-3", "win-ai");
    let h3 = spawn_sending(&kernel, &d3, "user-test", REPLY_TEXT);
    // Poll for a SECOND reply: spawn #1's is still in the transcript, so a
    // plain "any reply?" check would succeed on the stale one.
    let deadline = Instant::now() + HAPPENS_BUDGET;
    while count_from(&kernel, &transcript, &ctx, "win-ai") < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    h3.abort_signal.abort();
    let _ = h3.join.join();
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        2,
        "the post-respawn message was not answered: the position over-skipped \
         the live tail"
    );
}

#[test]
fn respawn_resumes_past_silently_processed_messages_not_only_replied_ones() {
    // Deepens the #81 fix past a single REPLIED message: a co-host usually
    // reads a message and stays SILENT (the ping-pong fix — it replies only
    // when it calls `send`). Those silently-processed messages must ALSO
    // advance the durable position; if it advanced only on messages that
    // produced a reply, a respawn would re-read every silent one and answer it.
    let kernel = Arc::new(Kernel::new());
    mount_conversations(&kernel);
    plant_conversation(&kernel, "win-ai", "user-test");
    let ctx = user_ctx();
    let transcript = transcript_of("win-ai", "user-test");

    // Three inbound. `SendsReply` answers the FIRST only (its `sent` latch), so
    // m1 and m2 are processed silently — the case under test.
    for body in ["m0", "m1", "m2"] {
        write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", body);
    }
    // The tail BEFORE the agent runs: exactly the three inbound messages.
    // Captured here because the transcript is SHARED — once the agent replies
    // its own append moves the tail, and a gate against a moving target never
    // settles. (With separate inboxes the inbound tail stood still on its own.)
    let inbound_tail = tail_offset(&kernel, &transcript, &ctx);

    let d1 = make_desc("cohost-win-ai-1", "win-ai");
    let h1 = spawn_sending(&kernel, &d1, "user-test", REPLY_TEXT);

    // Deterministically wait for spawn #1 to DRAIN all three. The silent ones
    // leave no observable reply, so gate on the position reaching that tail.
    // (Aborting early would leave m1/m2 genuinely unprocessed, and the respawn
    // handling them would be CORRECT, not a replay — a flaky false failure.)
    let deadline = Instant::now() + HAPPENS_BUDGET;
    while read_position(&kernel, "win-ai", "user-test") < inbound_tail && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        read_position(&kernel, "win-ai", "user-test") >= inbound_tail,
        "spawn #1 did not drain all three inbound messages before the respawn"
    );
    h1.abort_signal.abort();
    let _ = h1.join.join();
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        1,
        "spawn #1 should reply exactly once (m0); m1/m2 are processed silently"
    );

    // Respawn: the position sits PAST all three, including the two silent ones.
    // A fresh `SendsReply` (sent=false) would answer anything it re-reads.
    let d2 = make_desc("cohost-win-ai-2", "win-ai");
    let h2 = spawn_sending(&kernel, &d2, "user-test", REPLY_TEXT);
    thread::sleep(Duration::from_millis(700)); // ample time to (wrongly) replay
    h2.abort_signal.abort();
    let _ = h2.join.join();
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        1,
        "the respawn answered a silently-processed message: the position \
         advanced only on replies, not on every message taken (#81)"
    );

    // Liveness: a message AFTER the three IS answered, so the position resumed
    // at the true tail rather than over-skipping it.
    write_envelope(&kernel, &transcript, &ctx, "user-test", "win-ai", "m3");
    let d3 = make_desc("cohost-win-ai-3", "win-ai");
    let h3 = spawn_sending(&kernel, &d3, "user-test", REPLY_TEXT);
    let deadline = Instant::now() + HAPPENS_BUDGET;
    while count_from(&kernel, &transcript, &ctx, "win-ai") < 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    h3.abort_signal.abort();
    let _ = h3.join.join();
    assert_eq!(
        count_from(&kernel, &transcript, &ctx, "win-ai"),
        2,
        "the post-respawn message m3 was not answered: the position over-skipped \
         the live tail"
    );
}
