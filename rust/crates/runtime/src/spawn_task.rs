//! Managed-agent loop spawn entry: the co-hosted agent's conversation loop.
//!
//! Wires the per-pid agent loop to the agent's mailbox receiver and drives each
//! inbound [`MailboxEnvelope`] through a [`crate::ConversationRuntime`]. The
//! loop does NOT auto-reply: the agent decides whether to respond by calling
//! the `send` tool during the turn. Not calling it = silence, so a two-agent
//! conversation ends instead of ping-ponging every turn forever.
//!
//! ## One receiver, one sender
//!
//! Receiving, the read position, and delivery all belong to
//! [`crate::mailbox`] and this module holds none of them. It used to hold a
//! second copy of each: its own blocking `sys_read` tail, its own node-local
//! cursor file, its own self-write filter, and its own reply `sys_write`. Two
//! implementations of one contract drift, and these had: the cursor here was
//! node-local, so an agent restarted on another node silently replayed or
//! skipped, while the mailbox's position lived with the conversation.
//!
//! What remains here is the part that is genuinely this module's: turning an
//! envelope into a turn, and the agent state machine around it.
//!
//! ## State machine
//!
//! The loop drives the following agent-state transitions:
//!   WARMING_UP (runtime construction)
//!   -> READY (idle, waiting on the mailbox)
//!   -> BUSY (per turn, while `run_turn` executes)
//!   -> READY (turn complete, back to waiting)
//!
//! State is surfaced to the caller via the `state_callback` closure
//! passed to [`spawn_task`]; the caller (typically nexus's
//! `ManagedAgentService`) is responsible for calling
//! `agent_registry.update_state()` with the reported values.
//!
//! ## Cancellation
//!
//! Callers reuse [`crate::HookAbortSignal`] - the same signal
//! `with_hook_abort_signal` threads into the `ConversationRuntime`.
//! `cancel(Turn)` and `cancel(Session)` both translate to
//! `abort_signal.abort()`; the runtime's built-in abort check
//! short-circuits the current turn, the receiver stops, and the loop exits.

use std::sync::Arc;
use std::thread;

// Re-export kernel types so downstream crates (e.g. `tools`) can
// reference them without adding a direct `kernel` dependency.
pub use kernel::core::agents::registry::{AgentDescriptor, AgentState};
pub use kernel::kernel::syscall::KernelSyscall;

pub use crate::agent_mailbox::MailboxEnvelope;
use crate::mailbox::Mailbox;

use crate::conversation::{ApiClient, ConversationRuntime, ToolExecutor};
use crate::hooks::HookAbortSignal;

/// Blocking-tail read timeout per iteration. A `sys_read` with a non-zero
/// timeout does a fast-path read at the cursor and, on empty, parks on the
/// DT_STREAM's per-path condvar until the next frame lands or the timeout
/// expires (returning `Ok(None)`, at which point the loop re-checks `abort`).
///
/// This replaces the prior `sys_watch` (event notify) + non-blocking `sys_read`
/// pair with ONE tier-1 syscall. It is NOT a latency change: for the WAL A2A
/// mailbox both the old `sys_watch` and this blocking read wake sub-millisecond
/// on a same-node or replicated write (the raft apply observer signals BOTH the
/// file-watch and the stream condvar); for a non-WAL stream a same-node
/// `sys_write` wakes neither (it appends via the backend, not `write_nowait`),
/// so both fall back to this timeout. The wins are (1) DRY 鈥?the same
/// cursor-aware tail primitive the standalone `scode` receiver uses over gRPC
/// (`StreamReadAt` blocking); (2) one syscall that both waits AND returns the
/// frame at the cursor, no follow-up read; (3) atomic check-then-park closes
/// the lost-wakeup gap between the old separate `sys_read` and `sys_watch`.
const READ_BLOCK_MS: u64 = 500;

/// A type-erased "send a message to a peer's mailbox" capability handed to the
/// co-hosted agent's `send` tool. This is the ONE place a co-hosted
/// agent's reply is written: the poll loop no longer auto-forwards turn output,
/// so a reply happens ONLY when the agent deliberately calls the tool. It writes
/// a [`MailboxEnvelope`] (the a2a SSOT) to the recipient's inbox; the a2a stamp
/// hook overwrites `from` with the authenticated caller when auth is armed.
pub type MailboxSender = Arc<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>;

/// Shared handler for the `send` A2A tool: read `{to, message}` from the
/// parsed tool input and hand it to `sender`. Every executor routes its
/// `send` here, so the parse + delivery contract is defined ONCE 鈥?only
/// the `sender` differs by deployment (in-process [`mailbox_sender`] vs gRPC
/// `crate::nexus_mailbox::grpc_sender`).
///
/// `message` is the TOOL's field name, shared with the workspace-mailbox
/// delivery the same tool performs when no A2A sender is configured. It is
/// deliberately NOT the wire field: [`mailbox_sender`] puts the text into a
/// [`MailboxEnvelope`]'s `body`, because `{from,to,body}` is the a2a
/// substrate's contract and is not the tool's to rename.
///
/// # Errors
/// Returns a `String` error when the input lacks a string `to`/`message`, or
/// when the send fails.
pub fn handle_send_message(
    sender: &MailboxSender,
    input: &serde_json::Value,
) -> Result<String, String> {
    let to = input
        .get("to")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "send_message requires a string 'to'".to_string())?;
    let message = input
        .get("message")
        .and_then(|x| x.as_str())
        .ok_or_else(|| "send_message requires a string 'message'".to_string())?;
    (sender)(to, message)?;
    Ok(format!("message delivered to {to}"))
}

/// The system-prompt section that teaches a co-hosted agent the A2A reply
/// contract it runs under, so the model addresses its reply correctly instead
/// of guessing a recipient from the message text.
///
/// It is the prose counterpart of two mechanisms this module owns and MUST stay
/// in step with them:
/// * inbound framing 鈥?`run_loop` hands each message to the turn as
///   `[message from <sender>]\n\n<body>`, so `<sender>` is the reply target;
/// * the reply path 鈥?[`crate::mailbox::Mailbox::sender`] wires the `send` tool as the
///   ONLY way a co-hosted agent replies (appending to the shared transcript).
///
/// Kept next to those two so the wording cannot drift from the framing/tool it
/// describes. `self_id` is the agent's own name (`Mailbox::self_id`).
#[must_use]
pub fn cohost_a2a_prompt_section(self_id: &str) -> String {
    format!(
        "# Agent-to-agent messaging\n\
         Each message you receive is shown as `[message from <sender>]` followed \
         by its text. {}",
        crate::agent_mailbox::a2a_reply_contract(self_id)
    )
}

/// What a co-hosted agent is told about where its shell runs.
///
/// Its files are in the VFS at `workspace` and are reached with the file tools;
/// its `bash` runs on the daemon's host filesystem, starting in `shell_root`,
/// which is NOT the workspace. Without this the model does the reasonable thing —
/// `ls` to see what it is working with — reads an unrelated directory, and
/// concludes its workspace is empty.
///
/// Said once here, beside [`cohost_a2a_prompt_section`], so the two things only
/// this host contributes are written in one place. Nothing restricts where the
/// shell may `cd`: containment is the mount table, which bounds what the FILE
/// TOOLS address, and the process sandbox is the deployment's job — not this
/// sentence's.
#[must_use]
pub fn cohost_shell_prompt_section(workspace: &str, shell_root: &std::path::Path) -> String {
    format!(
        "# Your files and your shell are in different places\n\
         Your workspace is `{workspace}` and you reach it with the file tools \
         (read_file / write_file / edit_file / glob / grep). Your `bash` runs on \
         the host that runs this daemon, starting in `{}` — a directory of your \
         own that is NOT your workspace, so `ls` there will not show your files. \
         Use the file tools for your work and `bash` for commands.",
        shell_root.display()
    )
}

/// Handle returned by [`spawn_task`].
pub struct SpawnHandle {
    /// Shared abort signal 鈥?wired into the [`ConversationRuntime`] via
    /// `with_hook_abort_signal` so both turn-level and session-level
    /// cancellation share the same wire.
    pub abort_signal: HookAbortSignal,
    /// Join handle for the spawned worker thread.
    pub join: thread::JoinHandle<()>,
}

/// Spawn the managed-agent loop for a freshly-allocated pid.
///
/// The caller supplies a fully-constructed `api_client` and
/// `tool_executor`; `spawn_task` owns the `ConversationRuntime` lifecycle
/// and state-transition reporting. Receiving belongs to [`crate::mailbox`].
///
/// `state_callback` is invoked on every state transition so the caller
/// can forward to `AgentRegistry::update_state`.
///
/// `shell_root` is the HOST directory this agent's host-side execution runs in —
/// `bash`, `git`, and every hook that resolves
/// [`crate::workspace_root::current_workspace_root`]. It is entered as a scope on
/// the loop thread, which is where it has to happen: the scope is thread-local,
/// and the tool executor carries it onto its blocking threads with
/// [`crate::WorkspaceRootHandoff`]. Without one, all of the above fell through to
/// the DAEMON's working directory — one directory shared by every co-hosted agent
/// on that daemon, and the daemon's own git repository.
#[must_use]
pub fn spawn_task<C, T, F, R>(
    desc: &AgentDescriptor,
    mailbox: Arc<Mailbox>,
    runtime: ConversationRuntime<C, T>,
    host_resources: R,
    shell_root: std::path::PathBuf,
    state_callback: F,
) -> SpawnHandle
where
    C: ApiClient + 'static,
    T: ToolExecutor + 'static,
    F: Fn(AgentState, Option<String>) + Send + 'static,
    // Whatever the host must keep alive for as long as the loop runs — plugin
    // handles, MCP server processes. Held, never touched. A host that has none
    // passes `()`.
    R: Send + 'static,
{
    let abort_signal = HookAbortSignal::default();
    let abort_for_thread = abort_signal.clone();

    // The abort signal is created here, so it is applied here: a caller
    // cannot hold a signal that does not exist yet.
    let runtime = runtime.with_hook_abort_signal(abort_for_thread.clone());
    let join = thread::Builder::new()
        .name(format!("managed-agent-{}", desc.pid))
        .spawn(move || {
            let _host_resources = host_resources;
            // For the whole life of the loop: this agent's host-side root never
            // changes, and a per-turn scope would leave the gaps between turns
            // resolving to the daemon's directory again.
            let _shell_root = crate::WorkspaceRootScope::enter(shell_root);
            run_loop(mailbox, runtime, abort_for_thread, state_callback);
        })
        .expect("OS refused to spawn managed-agent thread");

    SpawnHandle { abort_signal, join }
}

// ---------------------------------------------------------------------------
// v2 loop 鈥?ConversationRuntime integration
// ---------------------------------------------------------------------------

fn run_loop<C, T, F>(
    mailbox: Arc<Mailbox>,
    mut runtime: ConversationRuntime<C, T>,
    abort: HookAbortSignal,
    state_cb: F,
) where
    C: ApiClient + 'static,
    T: ToolExecutor + 'static,
    F: Fn(AgentState, Option<String>),
{
    // Build a tokio runtime for async run_turn calls.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("managed-agent tokio runtime");

    // -- WARMING_UP --
    state_cb(AgentState::WarmingUp, None);

    // -- READY --
    state_cb(AgentState::Ready, None);

    let self_id = mailbox.self_id().to_string();

    // Inbound delivery runs on the receiver's tails, one per conversation, but a
    // turn must run HERE: there is one `ConversationRuntime` and `run_turn`
    // takes it by `&mut`, so turns are serial by construction - which is also
    // what an agent is. A tail hands its envelope over and blocks on the
    // acknowledgement, so "the consumer has taken responsibility" stays true
    // literally: the read position advances only after the turn it drove has
    // returned. That back-pressure is why this is a rendezvous and not a queue -
    // a queue would let the position advance past messages still waiting, and a
    // crash would lose them with the sender already told "delivered".
    let (inbound_tx, inbound_rx) =
        std::sync::mpsc::channel::<(MailboxEnvelope, std::sync::mpsc::SyncSender<bool>)>();
    let receiver = crate::mailbox::spawn_inbox_poller(
        mailbox,
        READ_BLOCK_MS,
        "cohost",
        abort.clone(),
        move |env| {
            // Nothing to drive a turn with. Accepting it is correct: it is a
            // real envelope that has been read, and refusing would park the
            // conversation on it forever. Envelopes this agent itself wrote are
            // already filtered out by the mailbox, not here.
            if env.body.is_empty() {
                return true;
            }
            let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
            if inbound_tx.send((env.clone(), ack_tx)).is_err() {
                // The loop is gone, so this envelope was NOT handled. Leaving
                // the position where it is means the next run sees it.
                return false;
            }
            ack_rx.recv().unwrap_or(false)
        },
    );

    while !abort.is_aborted() {
        let (env, ack) =
            match inbound_rx.recv_timeout(std::time::Duration::from_millis(READ_BLOCK_MS)) {
                Ok(inbound) => inbound,
                // Idle: nothing arrived this interval. Re-check `abort` and wait again.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                // Every tail is gone, so nothing can arrive again.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };

        state_cb(AgentState::Busy, None);

        // The agent decides whether to reply by calling the `send` tool DURING
        // the turn - the loop does NOT harvest the turn's text and forward it.
        // Not calling `send` means silence, so the conversation ends instead of
        // two agents bouncing every turn's output back at each other forever.
        //
        // The body is a peer's text - another organisation's on a cross-org hop
        // - so its harness markup is made inert before it becomes part of a
        // prompt. Without this a peer can spell a `<system-reminder>`, the one
        // tag the system prompt tells this model to treat as authoritative.
        let body = crate::agent_mailbox::neutralize_untrusted_markup(&env.body);
        let turn_input = format!("[message from {}]\n\n{body}", env.from);
        let outcome = rt.block_on(runtime.run_turn(&turn_input, None, None));

        // A turn that ERRORED was still delivered and driven, so it counts as
        // handled: redelivering it re-runs a turn that already failed, forever.
        // A turn cut short by `abort` did NOT run, so it stays unread and the
        // next spawn of this agent picks it up.
        let handled = match &outcome {
            Ok(_) => true,
            Err(e) => {
                let cancelled = abort.is_aborted();
                if !cancelled {
                    eprintln!("[managed-agent {self_id}] turn error: {e:?}");
                }
                !cancelled
            }
        };
        let _ = ack.send(handled);

        state_cb(AgentState::Ready, None);
    }

    // Joining is what makes "the loop returned" mean the receiver has stopped:
    // without it the tails outlive the runtime they were delivering into.
    let _ = receiver.join();
}

// Loop tests live under `runtime/tests/spawn_task.rs` as an integration
// test binary so they can compile without bringing in the rest of the
// lib's test target. Mailbox path routing is tested where it now lives,
// in `crate::mailbox`.

#[cfg(test)]
mod tests {
    #[test]
    fn cohost_prompt_teaches_reply_to_sender_via_send_message() {
        let section = super::cohost_a2a_prompt_section("chatbot");
        // Names the agent so the model knows its own identity 鈥?
        assert!(section.contains("chatbot"));
        // 鈥?names the ONLY reply path 鈥?
        assert!(section.contains("send"));
        // 鈥?mirrors the `[message from <sender>]` framing `run_loop` emits 鈥?
        assert!(section.contains("[message from <sender>]"));
        // 鈥?and encodes the fix: reply target is the sender, never a word
        // lifted from the message body (the exact mistake this prevents).
        assert!(section.contains("never a word copied"));
    }
}
