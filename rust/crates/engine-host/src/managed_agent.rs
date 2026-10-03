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
use ::managed_agent::{SpawnHandle as ManagedSpawnHandle, SpawnOptions, SpawnTask};
use runtime::mailbox::Mailbox;
use runtime::spawn_task::{
    cohost_a2a_prompt_section, cohost_shell_prompt_section, spawn_task_with_abort, AgentDescriptor,
    AgentState, KernelConvenience, SpawnHandle,
};
use runtime::{FsBackend, KernelFsBackend, PermissionMode, SystemPrompt};

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
/// # Errors
///
/// When this host cannot run an agent at all: no sudocode configuration to resolve a
/// model and its credentials, or a host-side directory it cannot create. The caller
/// (`ManagedAgentService::start_session`) answers the RPC with it — which is the only
/// place an operator can see it. These used to be `expect`s, so a daemon started
/// without sudocode configuration died on its own thread and printed a backtrace
/// about a missing file in place of a refusal naming it.
#[allow(
    clippy::needless_pass_by_value,
    reason = "preserve the public spawn API"
)]
pub fn spawn_managed_agent<K, F>(
    kernel: Arc<K>,
    desc: AgentDescriptor,
    state_callback: F,
) -> Result<SpawnHandle, String>
where
    K: KernelConvenience + Send + Sync + 'static,
    F: Fn(AgentState, Option<String>) + Send + 'static,
{
    spawn_with_options(&kernel, &desc, &SpawnOptions::default(), state_callback)
        .map(|(handle, _)| handle)
}

fn spawn_with_options<K, F>(
    kernel: &Arc<K>,
    desc: &AgentDescriptor,
    options: &SpawnOptions,
    state_callback: F,
) -> Result<(SpawnHandle, String), String>
where
    K: KernelConvenience + Send + Sync + 'static,
    F: Fn(AgentState, Option<String>) + Send + 'static,
{
    // Nothing in this daemon ticks `crons.json`: the scode scheduler is the
    // `scode cron daemon` / OS-cron path, which is a CLI process. A `CronCreate`
    // here would persist an entry that either never fires or — if a `scode cron`
    // ticker happens to share this machine's config home — fires later as a
    // standalone CLI run under a different identity, in a host directory. Both
    // are worse than a refusal, so the agent is not offered the tools; the fact
    // is declared once, on the same predicate an out-of-process host sets
    // `SUDOCODE_DISABLE_CRON_TOOLS` for.
    tools::declare_no_cron_ticker(
        "this host runs the agent inside nexusd, which does not fire scode crons          — schedule the work from the cluster instead",
    );

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
        Arc::clone(kernel),
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

    // Through the same named constructor the CLI uses, for the same reason the
    // engine is shared: the two hosts' shapes sit next to each other in
    // `runtime_build`, so the difference between them is readable in one place
    // instead of assembled here and inferred there.
    let host = HostContext::for_cohost_agent(fs, &desc.name, Arc::clone(&mailbox))
        .map_err(|e| format!("co-host: create the agent's host-side directory: {e}"))?;

    // Permissions are enforced by the kernel — ReBAC plus the workspace
    // boundary hook — on the far side of every one of these tools. A second
    // policy here would be a copy of a decision the kernel already owns, and
    // the copy is the one that goes stale.
    // Auth and provider configuration resolve exactly as they do for the CLI,
    // from the daemon's own config root. Defaulting them here would be a second
    // answer to a question `resolve_auth_mode` already owns.
    let sudocode_config = require_sudocode_config_for_cwd(&host.config_root).map_err(|e| {
        format!(
            "co-host: no usable sudocode configuration for {}: {e} — a co-hosted agent              resolves its model and credentials the way the CLI does, from              SCODE_GLOBAL_CONFIG_DIR (or ~/.nexus/sudocode) plus this daemon's working              directory",
            host.config_root.display(),
        )
    })?;
    let auth_mode = resolve_auth_mode(&model, None, &sudocode_config)
        .map_err(|e| format!("co-host: resolve auth mode for model {model:?}: {e}"))?;

    // The one prompt section this host contributes, for the same reason the
    // CLI contributes its skills listing: it describes something only this host
    // does. `run_loop` wraps each inbound message as `[message from <sender>]`,
    // and a model shown that framing without being told what it means answers
    // the wrapper instead of the sender.
    let mut system_prompt = SystemPrompt::default();
    system_prompt.append_dynamic_section(cohost_a2a_prompt_section(&desc.name));
    // The second: a co-hosted agent's files and its shell are in different
    // places, and a model not told that reads an unrelated directory and
    // concludes its workspace is empty.
    system_prompt.append_dynamic_section(cohost_shell_prompt_section(
        &workspace_root,
        &host.shell_root,
    ));

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

    let abort = runtime::HookAbortSignal::default();
    let (session, lease) = crate::managed_session::prepare_session(
        kernel,
        desc,
        Arc::clone(&host.fs),
        options.resume_session_id.as_deref(),
        &config.model,
        abort.clone(),
    )?;
    let durable_session_id = session.session_id.clone();

    let built = build_engine_runtime(
        &host,
        session,
        &desc.pid,
        config,
        &BTreeMap::new(),
        runtime::HookAbortSignal::default(),
        None,
    )
    .map_err(|error| format!("co-host: build the agent runtime: {error}"))?;
    let mut built = built;
    let engine = built
        .take_runtime()
        .expect("co-host: build_engine_runtime returned no runtime");

    // `built` goes with it: its `Drop` shuts down the MCP servers and plugins
    // this engine is using, so it has to outlive the loop rather than the call.
    Ok((
        spawn_task_with_abort(
            desc,
            mailbox,
            engine,
            (built, lease),
            host.shell_root.clone(),
            state_callback,
            abort,
        ),
        durable_session_id,
    ))
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
    K: KernelConvenience + Send + Sync + 'static,
{
    fn spawn(
        &self,
        kernel: Arc<K>,
        desc: AgentDescriptor,
        state_observer: Arc<dyn Fn(AgentState, Option<String>) + Send + Sync>,
    ) -> Result<Box<dyn ManagedSpawnHandle>, String> {
        self.spawn_with_options(kernel, desc, SpawnOptions::default(), state_observer)
    }

    fn spawn_with_options(
        &self,
        kernel: Arc<K>,
        desc: AgentDescriptor,
        options: SpawnOptions,
        state_observer: Arc<dyn Fn(AgentState, Option<String>) + Send + Sync>,
    ) -> Result<Box<dyn ManagedSpawnHandle>, String> {
        let (handle, durable_session_id) =
            spawn_with_options(&kernel, &desc, &options, move |state, reason| {
                state_observer(state, reason);
            })?;
        Ok(Box::new(SudoCodeSpawnHandle {
            inner: handle,
            durable_session_id,
        }))
    }
}

/// Exposes only the abort capability the managed-agent service's
/// `on_terminate` observer needs. `abort` signals the loop's shared
/// `HookAbortSignal`; the worker observes it and exits on its next poll
/// (idempotent — the observer may fire concurrently with an in-flight cancel).
struct SudoCodeSpawnHandle {
    inner: SpawnHandle,
    durable_session_id: String,
}

impl ManagedSpawnHandle for SudoCodeSpawnHandle {
    fn durable_session_id(&self) -> Option<&str> {
        Some(&self.durable_session_id)
    }

    fn abort(&self) {
        self.inner.abort_signal.abort();
    }
}
