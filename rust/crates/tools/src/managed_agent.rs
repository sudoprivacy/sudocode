//! Factory for the managed-agent spawn body (v2 ConversationRuntime).
//!
//! Constructs the `ApiClient`, `ToolExecutor`, `SystemPrompt`, and
//! `PermissionPolicy` dependencies from the `AgentDescriptor` metadata
//! and calls `runtime::spawn_task::spawn_task` to launch the full LLM
//! loop. The nexus cdylib's `SudoCodeSpawnAdapter` calls this instead
//! of `spawn_task_echo`.
//!
//! Lives in the `tools` crate because it needs both the `api` crate
//! (for `ProviderClient` / `resolve_provider_from_config`) and the
//! `runtime` crate (for `spawn_task`, `SystemPrompt`, etc.). The
//! `tools` crate is the natural composition point that already depends
//! on both.

use std::collections::BTreeSet;
use std::sync::Arc;

use runtime::spawn_task::{AgentDescriptor, AgentLoopState, KernelSyscall, Mailbox, SpawnHandle};
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
/// This is the v2 upgrade of `spawn_task_echo`. The caller
/// (nexus cdylib `SudoCodeSpawnAdapter`) invokes this after
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
    F: Fn(AgentLoopState) + Send + 'static,
{
    let model = desc
        .labels
        .get(MODEL_LABEL)
        .filter(|m| !m.is_empty())
        .cloned()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // -- ApiClient: provider chain from model id --
    let api_client = ProviderRuntimeClient::new(model, BTreeSet::new())
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

    // -- ToolExecutor: dispatch through the global tool registry, bound
    // to the VFS backend so every file tool is in-process. --
    let tool_executor = ManagedToolExecutor { fs };

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
}

impl ToolExecutor for ManagedToolExecutor {
    async fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let input_value: serde_json::Value =
            serde_json::from_str(input).map_err(|e| ToolError::new(e.to_string()))?;
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
