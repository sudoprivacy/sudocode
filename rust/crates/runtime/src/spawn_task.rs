//! Managed-agent loop spawn entry — v2 ConversationRuntime integration.
//!
//! Wires the per-pid agent loop into a full LLM turn-driver that waits
//! on the agent's mailbox (via `sys_watch` condvar blocking) for inbound
//! [`MailboxEnvelope`]s and drives each one through a
//! [`crate::ConversationRuntime`]. The loop does NOT auto-reply: the agent
//! decides whether to respond by calling the `send_message` tool during the
//! turn (routed through [`mailbox_sender`]). Not calling it = silence, so a
//! two-agent conversation ends instead of ping-ponging every turn forever.
//!
//! ## State machine
//!
//! The loop drives the following agent-state transitions:
//!   WARMING_UP (runtime construction)
//!   → READY (idle, polling mailbox)
//!   → BUSY (per turn, while `run_turn` executes)
//!   → READY (turn complete, back to polling)
//!
//! State is surfaced to the caller via the `state_callback` closure
//! passed to [`spawn_task`]; the caller (typically nexus's
//! `ManagedAgentService`) is responsible for calling
//! `agent_registry.update_state()` with the reported values.
//!
//! ## Cancellation
//!
//! Callers reuse [`crate::HookAbortSignal`] — the same signal
//! `with_hook_abort_signal` threads into the `ConversationRuntime`.
//! `cancel(Turn)` and `cancel(Session)` both translate to
//! `abort_signal.abort()`; the runtime's built-in abort check
//! short-circuits the current turn and the loop exits on the next
//! poll iteration.
//!
//! ## v1 → v2 migration
//!
//! v1 (echo scaffolding) is replaced in-place. The function signature
//! is extended with `api_client`, `tool_executor`, `system_prompt`,
//! and `permission_policy` so the caller constructs the provider-
//! specific wiring and spawn_task owns only the loop + state
//! management. The echo-reply helper is removed.

use std::sync::Arc;
use std::thread;

// Re-export kernel types so downstream crates (e.g. `tools`) can
// reference them without adding a direct `kernel` dependency.
pub use kernel::core::agents::registry::{AgentDescriptor, AgentState};
pub use kernel::kernel::syscall::KernelSyscall;
use kernel::kernel::OperationContext;

// The A2A mailbox message contract is owned by the `a2a` substrate (it stamps
// `from` + owns the path suffix). Re-export its SSOT types so this loop and the
// downstream `tools` crate consume the one definition instead of hand-rolling
// `{from,to,body}` JSON or the `chat-with-me` suffix.
pub use a2a::{MailboxEnvelope, CHAT_WITH_ME_SUFFIX};

use crate::conversation::{ApiClient, ConversationRuntime, ToolExecutor};
use crate::hooks::HookAbortSignal;
use crate::permissions::PermissionPolicy;
use crate::prompt::SystemPrompt;
use crate::session::Session;

/// `sys_watch` timeout per iteration. The kernel's `FileWatchRegistry`
/// condvar blocks the thread until a `FileWrite` event fires on the
/// mailbox path or the timeout expires — no busy-polling, near-zero
/// idle CPU. On timeout the loop re-checks `abort.is_aborted()` and
/// re-arms the watch.
const WATCH_TIMEOUT_MS: u64 = 500;

/// Per-call `sys_read` blocking timeout. `0` keeps the call
/// non-blocking — data is already present because `sys_watch` woke us.
const READ_TIMEOUT_MS: u64 = 0;

/// Where the co-hosted agent's chat mailbox lives — determines the path the
/// loop reads for inbound messages and where each reply is written.
#[derive(Debug, Clone)]
pub enum Mailbox {
    /// Node-local single stream (the managed-agent `/proc/{pid}/chat-with-me`
    /// model): the loop reads AND replies on the SAME path; both parties
    /// filter `from != self`. NOT raft-replicated — same-node only.
    LocalStream { path: String, self_id: String },
    /// A2A per-recipient inboxes under `base` (e.g. `/agents`): the loop
    /// reads its OWN inbox `{base}/{self_name}/chat-with-me` and writes each
    /// reply to the SENDER's inbox `{base}/{sender}/chat-with-me`. These
    /// paths are raft-replicated, so two co-hosted agents on different nodes
    /// converse over A2A with no bridge/relay.
    A2aInbox { base: String, self_name: String },
}

impl Mailbox {
    /// Path the loop reads + `sys_watch`es for inbound messages.
    fn inbox_path(&self) -> String {
        match self {
            Mailbox::LocalStream { path, .. } => path.clone(),
            Mailbox::A2aInbox { base, self_name } => {
                format!(
                    "{}/{}{CHAT_WITH_ME_SUFFIX}",
                    base.trim_end_matches('/'),
                    self_name
                )
            }
        }
    }

    /// This agent's own id — filters its own writes out of the inbox and is
    /// stamped as `from` on replies + as the operation actor.
    fn self_id(&self) -> &str {
        match self {
            Mailbox::LocalStream { self_id, .. } => self_id,
            Mailbox::A2aInbox { self_name, .. } => self_name,
        }
    }

    /// Where a reply addressed to `sender` is written. LocalStream replies on
    /// the shared stream; A2aInbox writes to the sender's own inbox.
    fn reply_path(&self, sender: &str) -> String {
        match self {
            Mailbox::LocalStream { path, .. } => path.clone(),
            Mailbox::A2aInbox { base, .. } => {
                format!(
                    "{}/{}{CHAT_WITH_ME_SUFFIX}",
                    base.trim_end_matches('/'),
                    sender
                )
            }
        }
    }
}

/// A type-erased "send a message to a peer's mailbox" capability handed to the
/// co-hosted agent's `send_message` tool. This is the ONE place a co-hosted
/// agent's reply is written: the poll loop no longer auto-forwards turn output,
/// so a reply happens ONLY when the agent deliberately calls the tool. It writes
/// a [`MailboxEnvelope`] (the a2a SSOT) to the recipient's inbox; the a2a stamp
/// hook overwrites `from` with the authenticated caller when auth is armed.
pub type MailboxSender = Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

/// Shared handler for the `send_message` A2A tool: parse `{to, body}` from the
/// raw tool input and hand it to `sender`. BOTH the co-host
/// (`ManagedToolExecutor`) and the standalone CLI executor route their
/// `send_message` here, so the parse + delivery contract is defined ONCE — only
/// the `sender` differs by deployment (in-process [`mailbox_sender`] vs gRPC
/// `crate::nexus_mailbox::grpc_sender`).
///
/// # Errors
/// Returns a `String` error when the input is not `{to, body}` or the send fails.
pub fn handle_send_message(sender: &MailboxSender, input: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(input).map_err(|e| e.to_string())?;
    let to = v
        .get("to")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "send_message requires a string 'to'".to_string())?;
    let body = v
        .get("body")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "send_message requires a string 'body'".to_string())?;
    (sender)(to, body)?;
    Ok(format!("message delivered to {to}"))
}

/// Build the [`MailboxSender`] for a co-hosted agent (its kernel + mailbox +
/// operation identity). Lives here, next to `Mailbox::reply_path`, so the
/// envelope build + reply-path + `sys_write` stay in one place.
#[must_use]
pub fn mailbox_sender<K: KernelSyscall + Send + Sync + 'static>(
    kernel: Arc<K>,
    mailbox: Mailbox,
    owner_id: String,
    zone_id: String,
) -> MailboxSender {
    let self_name = mailbox.self_id().to_string();
    Arc::new(move |to: &str, body: &str| {
        let env = MailboxEnvelope {
            from: self_name.clone(),
            to: to.to_string(),
            body: body.to_string(),
        };
        let ctx = OperationContext::new(&owner_id, &zone_id, false, Some(&self_name), true);
        kernel
            .sys_write(&mailbox.reply_path(to), &ctx, &env.to_bytes(), 0)
            .map(|_| ())
            .map_err(|e| format!("{e:?}"))
    })
}

/// The system-prompt section that teaches a co-hosted agent the A2A reply
/// contract it runs under, so the model addresses its reply correctly instead
/// of guessing a recipient from the message text.
///
/// It is the prose counterpart of two mechanisms this module owns and MUST stay
/// in step with them:
/// * inbound framing — `run_loop` hands each message to the turn as
///   `[message from <sender>]\n\n<body>`, so `<sender>` is the reply target;
/// * the reply path — [`mailbox_sender`] wires the `send_message` tool as the
///   ONLY way a co-hosted agent replies (writing to the sender's inbox).
///
/// Kept next to those two so the wording cannot drift from the framing/tool it
/// describes. `self_id` is the agent's own name (`Mailbox::self_id`).
#[must_use]
pub fn cohost_a2a_prompt_section(self_id: &str) -> String {
    format!(
        "# Agent-to-agent messaging\n\
         You are the agent \"{self_id}\", conversing with other agents by message. \
         Each message you receive is shown as `[message from <sender>]` followed by \
         its text. To reply, call the `send_message` tool with `to` set to that \
         exact `<sender>` name — the agent that messaged you, never a word copied \
         from the message text — and `body` set to your reply. Calling \
         `send_message` is the only way to reply; if you do not call it you stay \
         silent and the conversation ends."
    )
}

/// Handle returned by [`spawn_task`].
pub struct SpawnHandle {
    /// Shared abort signal — wired into the [`ConversationRuntime`] via
    /// `with_hook_abort_signal` so both turn-level and session-level
    /// cancellation share the same wire.
    pub abort_signal: HookAbortSignal,
    /// Join handle for the spawned worker thread.
    pub join: thread::JoinHandle<()>,
}

/// Spawn the managed-agent loop for a freshly-allocated pid.
///
/// The caller supplies a fully-constructed `api_client` and
/// `tool_executor` — spawn_task owns the mailbox poll loop, the
/// `ConversationRuntime` lifecycle, and state-transition reporting.
///
/// `state_callback` is invoked on every state transition so the caller
/// can forward to `AgentRegistry::update_state`.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn spawn_task<K, C, T, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    mailbox: Mailbox,
    api_client: C,
    tool_executor: T,
    system_prompt: SystemPrompt,
    permission_policy: PermissionPolicy,
    state_callback: F,
) -> SpawnHandle
where
    K: KernelSyscall + Send + Sync + 'static,
    C: ApiClient + 'static,
    T: ToolExecutor + 'static,
    F: Fn(AgentState) + Send + 'static,
{
    let abort_signal = HookAbortSignal::default();
    let abort_for_thread = abort_signal.clone();

    let join = thread::Builder::new()
        .name(format!("managed-agent-{}", desc.pid))
        .spawn(move || {
            run_loop(
                kernel,
                desc,
                mailbox,
                api_client,
                tool_executor,
                system_prompt,
                permission_policy,
                abort_for_thread,
                state_callback,
            );
        })
        .expect("OS refused to spawn managed-agent thread");

    SpawnHandle { abort_signal, join }
}

// ---------------------------------------------------------------------------
// v2 loop — ConversationRuntime integration
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_loop<K, C, T, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    mailbox: Mailbox,
    api_client: C,
    tool_executor: T,
    system_prompt: SystemPrompt,
    permission_policy: PermissionPolicy,
    abort: HookAbortSignal,
    state_cb: F,
) where
    K: KernelSyscall + Send + Sync + 'static,
    C: ApiClient + 'static,
    T: ToolExecutor + 'static,
    F: Fn(AgentState),
{
    // Build a tokio runtime for async run_turn calls.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("managed-agent tokio runtime");

    // -- WARMING_UP --
    state_cb(AgentState::WarmingUp);

    // The VFS-backed file tools are constructed by the spawn factory
    // (`tools::managed_agent::spawn_managed_agent`), which injects a
    // `KernelFsBackend` into the `tool_executor` this loop receives — so
    // the loop itself no longer builds one.

    let session = Session::new();
    let mut runtime = ConversationRuntime::new(
        session,
        api_client,
        tool_executor,
        permission_policy,
        system_prompt,
    )
    .with_session_known_date(crate::time::today_local())
    .with_hook_abort_signal(abort.clone());

    // -- READY --
    state_cb(AgentState::Ready);

    // Inbox the loop reads/watches, and the id it filters its own writes by.
    // For A2A this is the replicated `/agents/<self>/chat-with-me`; replies
    // go to the SENDER's inbox (raft-replicated → cross-machine, no bridge).
    let inbox_path = mailbox.inbox_path();
    let self_id = mailbox.self_id().to_string();
    let ctx = OperationContext::new(&desc.owner_id, &desc.zone_id, false, Some(&self_id), true);

    // Durable per-agent read cursor (node-local). A (re)spawned agent RESUMES
    // from the offset it last PROCESSED instead of replaying its whole inbox and
    // re-answering every historical message — the #81 re-reply storm, observed
    // live in the Win↔Mac duet when a respawned agent re-answered the entire
    // conversation. First spawn (no cursor yet) loads 0 and delivers all waiting
    // messages, so there is NO seek-to-tail delivery race. The cursor is a
    // node-local DT_REG (`sys_write` create-or-overwrites it; it lives in the
    // node's durable metastore, so it survives a daemon restart); each node's
    // agent owns its own cursor. All cursor I/O degrades GRACEFULLY (load → 0,
    // save ignored) so a missing / unmounted cursor path never wedges the agent —
    // only the no-replay guarantee weakens to "replay from 0".
    let cursor_path = cursor_path_for(&self_id);
    let mut next_offset: u64 = load_cursor(kernel.as_ref(), &cursor_path, &ctx);
    while !abort.is_aborted() {
        match kernel.sys_read(&inbox_path, &ctx, READ_TIMEOUT_MS, next_offset) {
            Ok(result) => {
                if let Some(bytes) = result.data.as_ref() {
                    if !bytes.is_empty() {
                        if let Some((sender, body)) = parse_inbound(bytes, &self_id) {
                            // -- BUSY --
                            state_cb(AgentState::Busy);

                            // Drive ONE turn on the inbound message. The sender is
                            // surfaced in the prompt so the agent can address a reply.
                            // The agent decides whether to reply by calling the
                            // `send_message` tool DURING the turn — the loop NO LONGER
                            // harvests the turn's text and auto-forwards it. Not
                            // calling `send_message` means silence, so the
                            // conversation ends instead of two agents bouncing every
                            // turn's output back to each other forever (the ping-pong).
                            let turn_input = format!("[message from {sender}]\n\n{body}");
                            if let Err(e) = rt.block_on(runtime.run_turn(&turn_input, None, None)) {
                                eprintln!("[managed-agent {self_id}] turn error: {e:?}");
                            }

                            // -- READY --
                            state_cb(AgentState::Ready);
                        }
                    }
                }
                if let Some(advanced) = result.stream_next_offset {
                    let advanced = advanced as u64;
                    // Persist the cursor only on REAL forward progress (a message
                    // was consumed) — never on idle no-op reads, which would
                    // rewrite the same offset every watch tick. Saving here (after
                    // the turn ran) makes a crash mid-turn re-process only that one
                    // message on respawn (at-least-once), never the whole history.
                    if advanced > next_offset {
                        next_offset = advanced;
                        save_cursor(kernel.as_ref(), &cursor_path, &ctx, next_offset);
                    }
                }
            }
            Err(e) => {
                // A read error is NOT terminal — `abort` (checked by the
                // `while`) is the SOLE terminal signal. For the managed
                // `/proc` stream, teardown fires `abort` via the service's
                // on_terminate observer, so the loop still exits promptly.
                // For the A2A inbox the stream is a durable, raft-replicated
                // path: a cold-read-before-`resolve`, a momentary not-leader,
                // or being read before the mint has planted it are all
                // TRANSIENT — a `break` here would silently kill a co-hosted
                // agent for the daemon's lifetime. Log, then fall through to
                // `sys_watch` (which paces the retry at `WATCH_TIMEOUT_MS`)
                // and re-check `abort`.
                eprintln!(
                    "[managed-agent {self_id}] inbox read error (transient, retrying): {e:?}"
                );
            }
        }
        // Block until a FileWrite event fires on the inbox path, or
        // timeout. Replaces the old `thread::sleep(50ms)` busy-poll
        // with a condvar wait — near-zero idle CPU, sub-millisecond
        // wake latency on new data.
        kernel.sys_watch(&inbox_path, WATCH_TIMEOUT_MS);
    }
}

/// Parse an inbound mailbox envelope.
///
/// Returns `Some((sender, body))` when the envelope is a JSON object
/// with `from != self` and a non-empty `body` field.
fn parse_inbound(bytes: &[u8], self_agent_id: &str) -> Option<(String, String)> {
    let env = MailboxEnvelope::from_bytes(bytes)?;
    // Skip anything without a real sender + body: our OWN writes (self-reply
    // storm guard), an unstamped/senderless envelope, or an empty message.
    if env.from.is_empty() || env.from == self_agent_id || env.body.is_empty() {
        return None;
    }
    Some((env.from, env.body))
}

/// Node-local path holding a co-host agent's durable inbox read cursor. Keyed by
/// the agent's stable identity (NOT its pid) so it survives respawn; sanitised to
/// a flat, path-safe leaf so `sys_write` auto-creates the DT_REG without needing
/// intermediate directories.
fn cursor_path_for(agent_id: &str) -> String {
    let safe: String = agent_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("/.cohost-cursor-{safe}")
}

/// Read the persisted cursor (a decimal offset). Any failure — path unmounted,
/// not-yet-created, or unparsable — yields 0, i.e. start from the inbox head.
fn load_cursor<K: KernelSyscall>(kernel: &K, path: &str, ctx: &OperationContext) -> u64 {
    kernel
        .sys_read(path, ctx, 0, 0)
        .ok()
        .and_then(|r| r.data)
        .and_then(|bytes| {
            std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        })
        .unwrap_or(0)
}

/// Overwrite the persisted cursor with `offset`. Best-effort: a write failure
/// only weakens the no-replay guarantee (never correctness), so it is ignored.
fn save_cursor<K: KernelSyscall>(kernel: &K, path: &str, ctx: &OperationContext, offset: u64) {
    let _ = kernel.sys_write(path, ctx, offset.to_string().as_bytes(), 0);
}

// Loop tests live under `runtime/tests/spawn_task.rs` as an integration
// test binary so they can compile without bringing in the rest of the
// lib's test target. The pure `Mailbox` routing is unit-tested inline.

#[cfg(test)]
mod tests {
    use super::Mailbox;

    #[test]
    fn a2a_inbox_reads_self_replies_to_sender() {
        let mb = Mailbox::A2aInbox {
            base: "/agents".into(),
            self_name: "win-ai".into(),
        };
        // Reads its OWN inbox …
        assert_eq!(mb.inbox_path(), "/agents/win-ai/chat-with-me");
        assert_eq!(mb.self_id(), "win-ai");
        // … and replies to the SENDER's inbox (raft-replicated → the peer's
        // node sees it), NOT its own.
        assert_eq!(mb.reply_path("mac-ai"), "/agents/mac-ai/chat-with-me");
        // A trailing slash on the base is tolerated.
        let mb2 = Mailbox::A2aInbox {
            base: "/agents/".into(),
            self_name: "a".into(),
        };
        assert_eq!(mb2.inbox_path(), "/agents/a/chat-with-me");
    }

    #[test]
    fn local_stream_reads_and_replies_on_the_same_path() {
        let mb = Mailbox::LocalStream {
            path: "/proc/7/chat-with-me".into(),
            self_id: "scode".into(),
        };
        assert_eq!(mb.inbox_path(), "/proc/7/chat-with-me");
        assert_eq!(mb.self_id(), "scode");
        // LocalStream replies on the shared stream regardless of sender.
        assert_eq!(mb.reply_path("anyone"), "/proc/7/chat-with-me");
    }

    #[test]
    fn cohost_prompt_teaches_reply_to_sender_via_send_message() {
        let section = super::cohost_a2a_prompt_section("chatbot");
        // Names the agent so the model knows its own identity …
        assert!(section.contains("chatbot"));
        // … names the ONLY reply path …
        assert!(section.contains("send_message"));
        // … mirrors the `[message from <sender>]` framing `run_loop` emits …
        assert!(section.contains("[message from <sender>]"));
        // … and encodes the fix: reply target is the sender, never a word
        // lifted from the message body (the exact mistake this prevents).
        assert!(section.contains("never a word copied"));
    }
}
