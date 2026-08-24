//! Factory for the managed-agent spawn body (v2 ConversationRuntime).
//!
//! Constructs the `ApiClient`, `ToolExecutor`, `SystemPrompt`, and
//! `PermissionPolicy` dependencies from the `AgentDescriptor` metadata
//! and calls `runtime::spawn_task::spawn_task` to launch the full LLM
//! loop. The co-located [`SudoCodeSpawnAdapter`] (this file) wraps it as a
//! `managed_agent::SpawnTask` that nexus injects at boot.
//!
//! Lives in the `tools` crate because it needs both the `api` crate
//! (for `ProviderClient` / `resolve_provider_from_config`) and the
//! `runtime` crate (for `spawn_task`, `SystemPrompt`, etc.). The
//! `tools` crate is the natural composition point that already depends
//! on both.

use std::collections::BTreeSet;
use std::sync::Arc;

use managed_agent::{SpawnHandle as ManagedSpawnHandle, SpawnTask};
use runtime::spawn_task::{
    mailbox_sender, AgentDescriptor, AgentState, KernelSyscall, Mailbox, MailboxSender, SpawnHandle,
};
use runtime::{
    FsBackend, KernelFsBackend, ModelFamilyIdentity, PermissionMode, PermissionPolicy,
    SystemPromptBuilder, ToolError, ToolExecutor,
};

use crate::{execute_tool_with_backend, ProviderRuntimeClient};

/// Label key where `ManagedAgentService` stores the model id in the
/// `AgentDescriptor.labels` map.
const MODEL_LABEL: &str = "model";

/// Default model used when the descriptor has no `model` label.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Spawn a managed-agent loop with the full ConversationRuntime.
///
/// The caller ([`SudoCodeSpawnAdapter`], below) invokes this after
/// `register_proc_entry` stamps the per-pid procfs subtree.
///
/// # Arguments
///
/// * `kernel` — shared kernel handle (in-process, monomorphised)
/// * `desc` — agent descriptor planted by `ManagedAgentService`
/// * `mailbox` — where the loop reads inbound + writes replies: a
///   node-local `/proc/{pid}` stream, or a raft-replicated A2A
///   `/agents/<name>` per-recipient inbox for cross-machine conversation
/// * `state_callback` — called on every state transition so the caller
///   can forward to `AgentRegistry::update_state`
pub fn spawn_managed_agent<K, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    mailbox: Mailbox,
    state_callback: F,
) -> SpawnHandle
where
    K: KernelSyscall + Send + Sync + 'static,
    F: Fn(AgentState) + Send + 'static,
{
    let model = desc
        .labels
        .get(MODEL_LABEL)
        .filter(|m| !m.is_empty())
        .cloned()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // -- ApiClient: provider chain from model id --
    // The co-hosted agent is A2A-capable, so its tool set includes
    // `send_message` (its ONLY, deliberate reply path — see the ping-pong fix).
    // The full tool set (file ops, etc.) is gated behind the agent-profile work;
    // the duet needs only send_message.
    let allowed_tools: BTreeSet<String> = std::iter::once("send_message".to_string()).collect();
    let api_client = ProviderRuntimeClient::new(model, allowed_tools)
        .expect("failed to construct API client from model label");

    // -- FsBackend: in-process VFS, NOT host std::fs. The co-hosted
    // agent's file tools (read/write/edit/glob/grep) route through
    // `KernelFsBackend` so they hit the kernel trie via syscalls, with
    // the agent's procfs workspace as the relative-path root.
    let workspace_root = format!("/proc/{}/workspace", desc.pid);
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        &desc.owner_id,
        &desc.zone_id,
        &desc.name,
        workspace_root,
    ));

    // -- Mailbox sender: the co-host's deliberate-reply capability, built from
    // this agent's kernel + mailbox + operation identity. Clone the mailbox and
    // identity here because `spawn_task` below moves the originals. --
    let send = mailbox_sender(
        Arc::clone(&kernel),
        mailbox.clone(),
        desc.owner_id.clone(),
        desc.zone_id.clone(),
    );

    // -- ToolExecutor: file tools in-process via the VFS backend; `send_message`
    // routes through the mailbox sender. --
    let tool_executor = ManagedToolExecutor { fs, send };

    // -- SystemPrompt: minimal prompt for managed-agent context --
    let system_prompt = SystemPromptBuilder::new()
        .with_model_family(ModelFamilyIdentity::Claude)
        .build();

    // -- PermissionPolicy: managed agents run with full access --
    // Nexus enforces permissions at the VFS layer (ReBAC +
    // WorkspaceBoundaryHook), so the in-process runtime grants all
    // tool invocations.
    let permission_policy = PermissionPolicy::new(PermissionMode::Allow);

    runtime::spawn_task::spawn_task(
        kernel,
        desc,
        mailbox,
        api_client,
        tool_executor,
        system_prompt,
        permission_policy,
        state_callback,
    )
}

/// Tool executor that dispatches through the `tools` crate's global
/// registry, bound to a VFS [`FsBackend`]. Wraps
/// `execute_tool_with_backend(name, input, fs)` into the `ToolExecutor`
/// trait expected by `ConversationRuntime`, so the file tools stay
/// in-process against the kernel trie.
struct ManagedToolExecutor {
    fs: Arc<dyn FsBackend>,
    /// The co-hosted agent's outbound-message capability. `send_message` is the
    /// ONLY way this agent replies to a peer — the poll loop no longer
    /// auto-forwards turn output (the ping-pong fix), so a reply happens ONLY
    /// when the agent deliberately calls the tool.
    send: MailboxSender,
}

impl ToolExecutor for ManagedToolExecutor {
    async fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let input_value: serde_json::Value =
            serde_json::from_str(input).map_err(|e| ToolError::new(e.to_string()))?;

        // `send_message` is the co-host's deliberate-reply path — a quick
        // in-process mailbox write bound to THIS agent's identity, not a file
        // op, so it routes through the mailbox sender, not the fs backend.
        if tool_name == "send_message" {
            let to = input_value
                .get("to")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new("send_message requires a string 'to'"))?;
            let body = input_value
                .get("body")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new("send_message requires a string 'body'"))?;
            (self.send)(to, body).map_err(ToolError::new)?;
            return Ok(format!("message delivered to {to}"));
        }

        // Offload the blocking in-process syscall to the blocking pool so a
        // concurrency-safe batch of read-only tools overlaps (I/O
        // interleaving, not thread parallelism): only the `Arc<fs>` clone and
        // owned args cross the thread boundary — the dispatcher never leaves
        // its thread, so it needs no `Sync`.
        let fs = Arc::clone(&self.fs);
        let tool_name = tool_name.to_string();
        tokio::task::spawn_blocking(move || {
            execute_tool_with_backend(&tool_name, &input_value, fs.as_ref()).map_err(ToolError::new)
        })
        .await
        .map_err(|e| ToolError::new(format!("tool task join error: {e}")))?
    }
}

/// The `SpawnTask` provider that hosts a `sudocode` agent loop as a nexus
/// managed-agent runtime body — the co-host seam.
///
/// `ManagedAgentService` (nexus-vfs) calls [`SpawnTask::spawn`] after planting
/// the per-pid procfs subtree; this impl builds the sudocode
/// `ConversationRuntime` loop via [`spawn_managed_agent`] and binds it to the
/// agent's REPLICATED A2A inbox `/agents/<name>/chat-with-me` (raft-replicated
/// when federated), so two co-hosted agents on different hosts converse over
/// A2A with no bridge/relay.
///
/// Lives here — next to [`spawn_managed_agent`], the loop it wraps — rather
/// than at the nexus binary edge: the adapter IS sudocode's. nexus only injects
/// `Arc::new(SudoCodeSpawnAdapter)` at boot via
/// `managed_agent::install_managed_agent_with_spawn`. There is NO enum map:
/// both `spawn_managed_agent`'s `state_callback` and `SpawnTask`'s observer
/// speak `kernel::AgentState` directly (the SSOT).
pub struct SudoCodeSpawnAdapter;

impl<K> SpawnTask<K> for SudoCodeSpawnAdapter
where
    K: KernelSyscall + Send + Sync + 'static,
{
    fn spawn(
        &self,
        kernel: Arc<K>,
        desc: AgentDescriptor,
        state_observer: Arc<dyn Fn(AgentState) + Send + Sync>,
    ) -> Box<dyn ManagedSpawnHandle> {
        // The co-host agent's mailbox is its persistent, cross-machine A2A
        // inbox `/agents/<name>/chat-with-me`, so a duet partner on another
        // host addresses it by name; raft replicates the reply back.
        let mailbox = Mailbox::A2aInbox {
            base: "/agents".to_string(),
            self_name: desc.name.clone(),
        };
        let handle = spawn_managed_agent(kernel, desc, mailbox, move |state| state_observer(state));
        Box::new(SudoCodeSpawnHandle { inner: handle })
    }
}

/// Wraps sudocode's [`SpawnHandle`] so the managed-agent service sees only the
/// abort capability its `on_terminate` observer needs. `abort` signals the
/// loop's shared `HookAbortSignal`; the worker thread observes it and exits on
/// its next poll (idempotent — the observer may fire concurrently with an
/// in-flight `cancel(Session)`).
struct SudoCodeSpawnHandle {
    inner: SpawnHandle,
}

impl ManagedSpawnHandle for SudoCodeSpawnHandle {
    fn abort(&self) {
        self.inner.abort_signal.abort();
    }
}
