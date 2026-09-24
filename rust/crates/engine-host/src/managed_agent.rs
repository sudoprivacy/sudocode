//! Hosting a `sudocode` agent loop inside `nexusd-cluster` — the co-host seam.
//!
//! This is a HOST, not a second engine. It builds the same
//! `ConversationRuntime` the CLI builds, through the same
//! [`crate::build_engine_runtime`], and differs only in the values it puts in
//! its [`HostContext`]: a kernel-backed filesystem instead of local disk, the
//! agent's procfs workspace instead of a working directory, and a mailbox that
//! is the agent's own identity.
//!
//! It lives in `engine-host` because that is where hosts live, and because the
//! engine is here: a factory in a lower crate cannot reach it without inverting
//! the dependency between `tools` and this crate.

use std::collections::BTreeMap;

use std::sync::Arc;

// `::` because this module shares its name with that crate.
use ::managed_agent::{SpawnHandle as ManagedSpawnHandle, SpawnTask};
use runtime::mailbox::Mailbox;
use runtime::session_control::SessionStore;
use runtime::spawn_task::{
    cohost_a2a_prompt_section, spawn_task, AgentDescriptor, AgentState, KernelSyscall, SpawnHandle,
};
use runtime::{FsBackend, KernelFsBackend, PermissionMode, Session, SystemPrompt};

use crate::config::{require_sudocode_config_for_cwd, resolve_auth_mode};
use crate::runtime_build::{build_engine_runtime, HostContext, RuntimeConfig};

/// Label key where `ManagedAgentService` stores the model id in the
/// descriptor's `labels` map.
const MODEL_LABEL: &str = "model";

/// Model used when the descriptor carries no `model` label.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Spawn a co-hosted agent: the CLI's engine, driven by a mailbox.
///
/// `ManagedAgentService` (nexus-vfs) calls this after `register_proc_entry`
/// stamps the per-pid procfs subtree.
///
/// The mailbox is NOT a parameter. A co-hosted agent's mailbox is its identity
/// — its own name, over the same VFS backend its file tools use — and
/// everything that determines it is already in `desc`. Accepting one only
/// created the opportunity to be handed a mailbox that disagrees with the
/// descriptor.
///
/// # Arguments
///
/// * `kernel` — shared in-process kernel handle, monomorphised
/// * `desc` — the descriptor `ManagedAgentService` planted
/// * `state_callback` — fired on every transition so the caller can forward to
///   `AgentRegistry::update_state`
pub fn spawn_managed_agent<K, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    state_callback: F,
) -> SpawnHandle
where
    K: KernelSyscall + Send + Sync + 'static,
    F: Fn(AgentState, Option<String>) + Send + 'static,
{
    let model = desc
        .labels
        .get(MODEL_LABEL)
        .filter(|m| !m.is_empty())
        .cloned()
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());

    // File tools reach the kernel, not local disk. That reach is the whole
    // reason to co-host: a write here passes the hooks, the audit trail and the
    // permission checks that a `std::fs` write never sees.
    let workspace_root = format!("/proc/{}/workspace", desc.pid);
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        &desc.owner_id,
        &desc.zone_id,
        &desc.name,
        workspace_root.clone(),
    ));

    // The agent's conversations, over the SAME backend as its file tools. An
    // absolute A2A path bypasses the workspace root, so one backend serves both
    // the workspace and `/conversations` — sender and receiver are two views of
    // one object rather than two implementations that have to agree.
    let mailbox = Arc::new(Mailbox::daemon_absolute(Arc::clone(&fs), desc.name.clone()));

    let host = HostContext {
        fs,
        // Configuration stays on the daemon's own disk. A daemon has to read
        // its configuration before it can serve the VFS that configuration
        // describes, so this is the one root that does not follow `fs`.
        config_root: std::env::current_dir().unwrap_or_default(),
        // The descriptor is the SSOT for who this agent is; nothing here should
        // re-derive it from a path.
        agent_name: Some(desc.name.clone()),
        mailbox: Some(Arc::clone(&mailbox)),
    };

    // Permissions are enforced by the kernel — ReBAC plus the workspace
    // boundary hook — on the far side of every one of these tools. A second
    // policy here would be a copy of a decision the kernel already owns, and
    // the copy is the one that goes stale.
    // Auth and provider configuration resolve exactly as they do for the CLI,
    // from the daemon's own config root. Defaulting them here would be a second
    // answer to a question `resolve_auth_mode` already owns.
    let sudocode_config = require_sudocode_config_for_cwd(&host.config_root)
        .expect("co-host: sudocode configuration is required to build an agent");
    let auth_mode = resolve_auth_mode(&model, None, &sudocode_config)
        .expect("co-host: failed to resolve auth mode");

    // The one prompt section this host contributes, for the same reason the
    // CLI contributes its skills listing: it describes something only this host
    // does. `run_loop` wraps each inbound message as `[message from <sender>]`,
    // and a model shown that framing without being told what it means answers
    // the wrapper instead of the sender.
    let mut system_prompt = SystemPrompt::default();
    system_prompt
        .dynamic_sections
        .push(cohost_a2a_prompt_section(&desc.name));

    let config = RuntimeConfig {
        model,
        system_prompt,
        enable_tools: true,
        // No allow-list: the full tool set is the point. Reading, editing and
        // searching through the kernel is what co-hosting buys, and an agent
        // restricted to messaging would never exercise any of it.
        allowed_tools: None,
        permission_mode: PermissionMode::Allow,
        auth_mode,
        sudocode_config,
        memory: runtime::memory::MemoryMode::default(),
    };

    // The agent's session, rooted where its OWN filesystem says sessions live:
    // `/sessions/<id>/transcript.jsonl` on a kernel, and `create_handle` plants
    // the `/agents/{name}/sessions/<id>` index for it. Nothing here chooses a
    // path — `FsBackend::managed_sessions_root` does, which is why pointing
    // sessions at nexus is a backend swap rather than a second layout to keep in
    // step.
    //
    // The session id is its own, NOT the pid: a pid names a running process and
    // a session names a transcript, and one agent's pid is reused across the
    // sessions it runs.
    let session = Session::new();
    let handle =
        session_store_for(&workspace_root, &host.fs, &desc.name).create_handle(&session.session_id);
    let session = session
        .with_persistence_path(handle.path)
        // The backend too, or persistence would default to `StdFsBackend` and
        // write a VFS-looking path onto the daemon's local disk — the same
        // mistake the mailbox made before #752.
        .with_fs_backend(Arc::clone(&host.fs));

    let built = build_engine_runtime(
        &host,
        session,
        &desc.pid,
        config,
        &BTreeMap::new(),
        runtime::HookAbortSignal::default(),
        None,
    )
    .expect("co-host: failed to build the agent runtime");
    let mut built = built;
    let engine = built
        .take_runtime()
        .expect("co-host: build_engine_runtime returned no runtime");

    // `built` goes with it: its `Drop` shuts down the MCP servers and plugins
    // this engine is using, so it has to outlive the loop rather than the call.
    spawn_task(&desc, mailbox, engine, built, state_callback)
}

/// The session store a co-hosted agent records into.
///
/// Fails loud rather than degrading to a session nobody can find: a store that
/// cannot be built means `/sessions` is unroutable, and an agent that runs
/// anyway would hold its whole transcript in memory and lose it on exit —
/// exactly the state this replaces.
fn session_store_for(
    workspace_root: &str,
    fs: &Arc<dyn FsBackend>,
    agent_name: &str,
) -> SessionStore {
    SessionStore::from_cwd_with(workspace_root, Arc::clone(fs))
        .expect("co-host: the agent's session store must be reachable")
        .with_agent_name(agent_name)
}

/// The `SpawnTask` provider that hosts a `sudocode` agent as a nexus
/// managed-agent runtime body.
///
/// `ManagedAgentService` (nexus-vfs) calls [`SpawnTask::spawn`] after planting
/// the per-pid procfs subtree. nexus only injects
/// `Arc::new(SudoCodeSpawnAdapter)` at boot via
/// `managed_agent::install_managed_agent_with_spawn`; there is no enum map,
/// because both this callback and `SpawnTask`'s observer speak
/// `kernel::AgentState` directly.
pub struct SudoCodeSpawnAdapter;

impl<K> SpawnTask<K> for SudoCodeSpawnAdapter
where
    K: KernelSyscall + Send + Sync + 'static,
{
    fn spawn(
        &self,
        kernel: Arc<K>,
        desc: AgentDescriptor,
        state_observer: Arc<dyn Fn(AgentState, Option<String>) + Send + Sync>,
    ) -> Box<dyn ManagedSpawnHandle> {
        let handle = spawn_managed_agent(kernel, desc, move |state, reason| {
            state_observer(state, reason);
        });
        Box::new(SudoCodeSpawnHandle { inner: handle })
    }
}

/// Exposes only the abort capability the managed-agent service's
/// `on_terminate` observer needs. `abort` signals the loop's shared
/// `HookAbortSignal`; the worker observes it and exits on its next poll
/// (idempotent — the observer may fire concurrently with an in-flight cancel).
struct SudoCodeSpawnHandle {
    inner: SpawnHandle,
}

impl ManagedSpawnHandle for SudoCodeSpawnHandle {
    fn abort(&self) {
        self.inner.abort_signal.abort();
    }
}
