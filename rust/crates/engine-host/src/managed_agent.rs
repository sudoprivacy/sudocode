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

use std::sync::Arc;

// `::` because this module shares its name with that crate.
use ::managed_agent::SpawnOptions;
use runtime::mailbox::Mailbox;
use runtime::spawn_task::{
    cohost_a2a_prompt_section, cohost_shell_prompt_section, AgentDescriptor, KernelConvenience,
};
use runtime::{FsBackend, KernelFsAccess, KernelFsBackend, PermissionMode, SystemPrompt};

use crate::config::{require_sudocode_config_for_cwd, resolve_auth_mode};
use crate::runtime_build::{HostContext, RuntimeConfig};

/// Label key where `ManagedAgentService` stores the model id in the
/// descriptor's `labels` map.
const MODEL_LABEL: &str = "model";

/// Model used when the descriptor carries no `model` label.
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Prepared transcript and host resources. A renderer supplies the initial
/// session options and adopts this into the shared SessionEngine.
pub struct PreparedManagedAgent {
    host: HostContext,
    session: runtime::Session,
    config: RuntimeConfig,
    abort: runtime::HookAbortSignal,
    lease: crate::managed_session::SessionLease,
}

impl PreparedManagedAgent {
    pub fn durable_session_id(&self) -> &str {
        &self.session.session_id
    }
    pub fn model(&self) -> &str {
        &self.config.model
    }
    pub fn abort_signal(&self) -> runtime::HookAbortSignal {
        self.abort.clone()
    }
    pub fn mailbox(&self) -> Option<Arc<Mailbox>> {
        self.host.mailbox.clone()
    }
    pub fn cwd(&self) -> std::path::PathBuf {
        self.host.shell_root.clone()
    }

    pub fn open(
        mut self,
        mcp_servers: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
        memory: runtime::memory::MemoryMode,
    ) -> Result<crate::SessionEngine, String> {
        self.config.memory = memory;
        crate::SessionEngine::for_host(
            self.host,
            self.session,
            self.config,
            mcp_servers,
            prompt_overrides,
            runtime::HookAbortSignal::new(),
            Box::new(self.lease),
        )
    }
}

/// Prepare a new or resumed co-hosted session against the daemon's kernel.
/// No turn loop runs here: the session renderer drives the shared engine.
pub fn prepare_managed_agent<K>(
    kernel: &Arc<K>,
    desc: &AgentDescriptor,
    options: &SpawnOptions,
) -> Result<PreparedManagedAgent, String>
where
    K: KernelConvenience + KernelFsAccess + Send + Sync + 'static,
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
    // Built from the complete planted descriptor so the agent's delegation
    // reference participates in every target authorization the backend makes.
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(kernel),
        desc,
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

    // Kernel authorization and a user's tool approval are separate checks.
    // The session driver supplies permission and question callbacks at each turn.
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
        permission_mode: PermissionMode::Prompt,
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
    Ok(PreparedManagedAgent {
        host,
        session,
        config,
        abort,
        lease,
    })
}
