//! The co-host harness both co-host tests drive.
//!
//! One scaffolding, two models: the scripted one (`cohost_mock_llm`, a CI gate)
//! and a real one (`cohost_live_llm`, opt-in). Shared deliberately — a mock run
//! only says something about the live path if both stand the agent up the same
//! way, and two copies of this setup would drift into two different co-hosts.

// A `tests/common` module is compiled into EVERY test binary that declares it,
// so a helper only one of them needs reads as dead code in the other.
#![allow(dead_code)]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use kernel::core::agents::registry::{AgentDescriptor, AgentKind};
use kernel::kernel::{Kernel, OperationContext, ReadRequest};
use runtime::mailbox::Mailbox;

/// Mount everything a co-hosted agent's session needs.
///
/// All three get a CONTENT backend, `/agents` included. It used to be mounted
/// metadata-only on the reasoning that a chat list holds only DT_LINKs — but a
/// chat-list entry is a plain WRITE now (a link is unmakeable over gRPC, where
/// `Setattr` carries no target), and bytes written to a backend-less mount
/// leave no child for `readdir`. The receiver finds its conversations by
/// listing that directory, so an empty listing is an agent that hears nothing:
/// it reaches READY, polls a chat list with no entries, and waits forever.
///
/// In production this cannot arise — a founder mounts every prefix its
/// subsystems declare (`default_replicated_prefixes` = A2A's prefixes plus
/// `SESSIONS_BASE`) onto its zone — which is exactly why the harness has to
/// model the same set. `/sessions` is in it because a co-hosted agent records
/// its turns there.
pub fn mount_agent_world(kernel: &Kernel) {
    for point in ["/proc", "/conversations", "/agents", "/sessions"] {
        kernel.vfs_router_arc().add_mount(
            point,
            "root",
            Some(Arc::new(runtime::test_support::MemObjectStore::default())),
            false,
        );
    }
}

/// A managed-agent descriptor of the shape `ManagedAgentService` plants.
pub fn make_desc(pid: &str, name: &str, model: &str) -> AgentDescriptor {
    let mut desc = AgentDescriptor {
        pid: pid.to_string(),
        name: name.to_string(),
        kind: AgentKind::Managed,
        owner_id: "test-owner".to_string(),
        zone_id: "root".to_string(),
        ..Default::default()
    };
    desc.labels.insert("model".to_string(), model.to_string());
    desc
}

/// The user side of the conversation — a person, not an agent.
#[must_use]
pub fn user_ctx() -> OperationContext {
    OperationContext::new("test-user", "root", false, Some("user-test"), true)
}

/// The VFS path a co-hosted agent's relative tool paths resolve against.
#[must_use]
pub fn agent_workspace(pid: &str) -> String {
    format!("/proc/{pid}/workspace")
}

/// Give the pair's transcript the shape the DAEMON gives it: a DT_STREAM.
///
/// `Mailbox::ensure_conversation` asks for one with `io_profile = "wal"`, which
/// needs federation, and degrades to a DT_REG when there is none. A bare kernel
/// has no federation, so without this the harness would run the whole co-host on
/// the JSONL fallback — a transcript with no record framing and no
/// `stream_next_offset` — and prove nothing about the path production takes.
///
/// `"memory"` is the in-process stream profile: same offset-addressed framed
/// records, without raft behind them. Called BEFORE `ensure_conversation`, whose
/// creation is idempotent and leaves an existing entry alone.
pub fn provision_stream_transcript(kernel: &Kernel, path: &str) {
    use kernel::abc::meta_store::DT_STREAM;
    let parent = path.rsplit_once('/').map_or("/", |(dir, _)| dir);
    let _ = kernel.sys_setattr(
        parent, 1, // DT_DIR
        "", None, None, None, "balanced", "root", false, 0, None, None, None, None, None, None,
        None, None, None, None, None,
    );
    kernel
        .sys_setattr(
            path,
            DT_STREAM as i32,
            "",
            None,
            None,
            None,
            "memory",
            "root",
            false,
            1 << 20, // capacity: the harness never approaches it
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
        .expect("provision the transcript as a stream");
}

/// Send one message to `to`, through the sender production uses.
///
/// A `Mailbox`, not a hand-rolled `sys_write`: the transcript's shape decides
/// how a message is framed (a DT_STREAM record, or a newline-terminated JSONL
/// line), and a writer that bypasses that put an unterminated object in front of
/// the agent's reply — one line neither side could parse. Writing through the
/// real sender is also the only way a harness can claim the receiving half works.
pub fn send_prompt(mailbox: &Arc<Mailbox>, to: &str, body: &str) {
    mailbox.sender()(to, body).expect("the user's message reaches the conversation");
}

/// Poll the transcript until an envelope from `agent_id` with a non-empty body
/// arrives, or `timeout` elapses.
pub fn wait_for_agent_reply(
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
                // One payload, two possible shapes: a DT_STREAM read returns a
                // single framed record, and a byte-addressed transcript returns
                // however many JSONL lines have accumulated. Scanning lines
                // reads both, which is the point — the shape depends on whether
                // the conversation's stream could be created, and this harness
                // exercises a co-host on either.
                for line in String::from_utf8_lossy(bytes).lines() {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                        continue;
                    };
                    if v.get("from").and_then(|f| f.as_str()) != Some(agent_id) {
                        continue;
                    }
                    if !v
                        .get("body")
                        .and_then(|b| b.as_str())
                        .unwrap_or_default()
                        .is_empty()
                    {
                        return Some(v);
                    }
                }
            }
            match result.stream_next_offset {
                Some(next) => offset = next as u64,
                // Byte-addressed: advance past what was just read, or the next
                // poll re-reads it forever.
                None => offset += result.data.as_ref().map_or(0, |b| b.len() as u64),
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}
