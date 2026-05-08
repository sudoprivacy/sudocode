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

use runtime::spawn_task::{AgentDescriptor, AgentLoopState, KernelAbi, SpawnHandle};
use runtime::{
    ModelFamilyIdentity, PermissionMode, PermissionPolicy, SystemPromptBuilder, ToolError,
    ToolExecutor,
};

use crate::{execute_tool, ProviderRuntimeClient};

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
/// * `state_callback` — called on every state transition so the caller
///   can forward to `AgentRegistry::update_state`
pub fn spawn_managed_agent<K, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    state_callback: F,
) -> SpawnHandle
where
    K: KernelAbi + Send + Sync + 'static,
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

    // -- ToolExecutor: dispatch through the global tool registry --
    let tool_executor = ManagedToolExecutor;

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
        api_client,
        tool_executor,
        system_prompt,
        permission_policy,
        state_callback,
    )
}

/// Tool executor that dispatches through the `tools` crate's global
/// registry. Wraps `execute_tool(name, input)` into the `ToolExecutor`
/// trait expected by `ConversationRuntime`.
struct ManagedToolExecutor;

impl ToolExecutor for ManagedToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let input_value: serde_json::Value =
            serde_json::from_str(input).map_err(|e| ToolError::new(e.to_string()))?;
        execute_tool(tool_name, &input_value).map_err(ToolError::new)
    }
}
