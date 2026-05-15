//! Managed-agent loop spawn entry — v2 ConversationRuntime integration.
//!
//! Wires the per-pid agent loop into a full LLM turn-driver that waits
//! on `/proc/{pid}/chat-with-me` for inbound JSON envelopes (via
//! `sys_watch` condvar blocking), drives each prompt through a
//! [`crate::ConversationRuntime`], and writes structured responses back
//! through the same mailbox path.
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
pub use kernel::abi::KernelAbi;
pub use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::OperationContext;

use crate::conversation::{ApiClient, ConversationRuntime, ToolExecutor};
use crate::fs_backend::KernelFsBackend;
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

/// Agent-state values surfaced via `state_callback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLoopState {
    WarmingUp,
    Ready,
    Busy,
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
pub fn spawn_task<K, C, T, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    api_client: C,
    tool_executor: T,
    system_prompt: SystemPrompt,
    permission_policy: PermissionPolicy,
    state_callback: F,
) -> SpawnHandle
where
    K: KernelAbi + Send + Sync + 'static,
    C: ApiClient + 'static,
    T: ToolExecutor + 'static,
    F: Fn(AgentLoopState) + Send + 'static,
{
    let abort_signal = HookAbortSignal::default();
    let abort_for_thread = abort_signal.clone();

    let join = thread::Builder::new()
        .name(format!("managed-agent-{}", desc.pid))
        .spawn(move || {
            run_loop(
                kernel,
                desc,
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

/// Spawn the v1 echo-only loop (retained for backward compatibility
/// and integration tests that don't need a full LLM provider).
#[must_use]
pub fn spawn_task_echo<K: KernelAbi + Send + Sync + 'static>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
) -> SpawnHandle {
    let abort_signal = HookAbortSignal::default();
    let abort_for_thread = abort_signal.clone();

    let join = thread::Builder::new()
        .name(format!("managed-agent-echo-{}", desc.pid))
        .spawn(move || {
            run_echo_loop(&kernel, &desc, &abort_for_thread);
        })
        .expect("OS refused to spawn managed-agent thread");

    SpawnHandle { abort_signal, join }
}

// ---------------------------------------------------------------------------
// v2 loop — ConversationRuntime integration
// ---------------------------------------------------------------------------

fn run_loop<K, C, T, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    api_client: C,
    tool_executor: T,
    system_prompt: SystemPrompt,
    permission_policy: PermissionPolicy,
    abort: HookAbortSignal,
    state_cb: F,
) where
    K: KernelAbi + Send + Sync + 'static,
    C: ApiClient + 'static,
    T: ToolExecutor + 'static,
    F: Fn(AgentLoopState),
{
    // Build a tokio runtime for async run_turn calls.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("managed-agent tokio runtime");

    // -- WARMING_UP --
    state_cb(AgentLoopState::WarmingUp);

    // Build the KernelFsBackend for VFS-backed file operations.
    let _fs_backend = KernelFsBackend::new(
        Arc::clone(&kernel),
        OperationContext::new(&desc.owner_id, &desc.zone_id, false, Some(&desc.name), true),
    );

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
    state_cb(AgentLoopState::Ready);

    let cwm_path = format!("/proc/{}/chat-with-me", desc.pid);
    let agent_id = desc.name.as_str();
    let ctx = OperationContext::new(&desc.owner_id, &desc.zone_id, false, Some(agent_id), true);

    let mut next_offset: u64 = 0;
    while !abort.is_aborted() {
        match kernel.sys_read(&cwm_path, &ctx, READ_TIMEOUT_MS, next_offset) {
            Ok(result) => {
                if let Some(bytes) = result.data.as_ref() {
                    if !bytes.is_empty() {
                        if let Some((sender, prompt)) = parse_inbound(bytes, agent_id) {
                            // -- BUSY --
                            state_cb(AgentLoopState::Busy);

                            let turn_result = rt.block_on(runtime.run_turn(&prompt, None, None));

                            let response = match turn_result {
                                Ok(summary) => {
                                    let text = summary
                                        .assistant_messages
                                        .iter()
                                        .filter_map(|m| {
                                            m.blocks.iter().find_map(|b| match b {
                                                crate::session::ContentBlock::Text { text } => {
                                                    Some(text.as_str())
                                                }
                                                _ => None,
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    serde_json::json!({
                                        "to": sender,
                                        "from": agent_id,
                                        "body": text,
                                    })
                                }
                                Err(e) => {
                                    serde_json::json!({
                                        "to": sender,
                                        "from": agent_id,
                                        "body": format!("error: {e}"),
                                        "error": true,
                                    })
                                }
                            };

                            if let Ok(bytes) = serde_json::to_vec(&response) {
                                let _ = kernel.sys_write(&cwm_path, &ctx, &bytes, 0);
                            }

                            // -- READY --
                            state_cb(AgentLoopState::Ready);
                        }
                    }
                }
                if let Some(advanced) = result.stream_next_offset {
                    next_offset = advanced as u64;
                }
            }
            Err(_) => {
                // Path tear-down (procfs unregister) or transient kernel
                // error — v2 treats every kernel error as terminal because
                // the loop's lifetime is bounded by the pid's procfs subtree.
                break;
            }
        }
        // Block until a FileWrite event fires on the mailbox path, or
        // timeout. Replaces the old `thread::sleep(50ms)` busy-poll
        // with a condvar wait — near-zero idle CPU, sub-millisecond
        // wake latency on new data.
        kernel.sys_watch(&cwm_path, WATCH_TIMEOUT_MS);
    }
}

/// Parse an inbound mailbox envelope.
///
/// Returns `Some((sender, body))` when the envelope is a JSON object
/// with `from != self` and a non-empty `body` field.
fn parse_inbound(bytes: &[u8], self_agent_id: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let obj = value.as_object()?;
    let from = obj.get("from").and_then(|v| v.as_str())?;
    if from == self_agent_id {
        return None;
    }
    let body = obj
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if body.is_empty() {
        return None;
    }
    Some((from.to_string(), body))
}

// ---------------------------------------------------------------------------
// v1 echo loop — retained for tests and backward compatibility
// ---------------------------------------------------------------------------

fn run_echo_loop<K: KernelAbi>(kernel: &Arc<K>, desc: &AgentDescriptor, abort: &HookAbortSignal) {
    let cwm_path = format!("/proc/{}/chat-with-me", desc.pid);
    let agent_id = desc.name.as_str();
    let ctx = OperationContext::new(&desc.owner_id, &desc.zone_id, false, Some(agent_id), true);

    let mut next_offset: u64 = 0;
    while !abort.is_aborted() {
        match kernel.sys_read(&cwm_path, &ctx, READ_TIMEOUT_MS, next_offset) {
            Ok(result) => {
                if let Some(bytes) = result.data.as_ref() {
                    if !bytes.is_empty() {
                        if let Some(reply) = build_echo_reply(bytes, agent_id) {
                            let _ = kernel.sys_write(&cwm_path, &ctx, &reply, 0);
                        }
                    }
                }
                if let Some(advanced) = result.stream_next_offset {
                    next_offset = advanced as u64;
                }
            }
            Err(_) => break,
        }
        kernel.sys_watch(&cwm_path, WATCH_TIMEOUT_MS);
    }
}

fn build_echo_reply(inbound: &[u8], self_agent_id: &str) -> Option<Vec<u8>> {
    let value: serde_json::Value = serde_json::from_slice(inbound).ok()?;
    let obj = value.as_object()?;
    let from = obj.get("from").and_then(|v| v.as_str())?;
    if from == self_agent_id {
        return None;
    }
    let body = obj.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let reply = serde_json::json!({
        "to": from,
        "from": self_agent_id,
        "body": format!("echo: {body}"),
    });
    serde_json::to_vec(&reply).ok()
}

// Tests live under `runtime/tests/spawn_task.rs` as an integration
// test binary so they can compile without bringing in the rest of
// the lib's test target.
