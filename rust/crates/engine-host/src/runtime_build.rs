//! Runtime construction below the seam.
//!
//! Home of the `ConversationRuntime` wrapper (`BuiltRuntime`), the non-session
//! `RuntimeConfig` / `RuntimePluginState` inputs threaded through construction,
//! and the `build_runtime*` chain that assembles plugins, MCP, permission
//! policy, the system prompt, and the `EngineApiClient`. Both renderers build a
//! session through here. Live plugin-hook progress rides the engine↔renderer
//! seam as `EngineEvent::HookProgress` (installed per-turn from the observer),
//! so no renderer type is named below the seam.

use std::io;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use commands::cwd_prompt_sections;
use engine_core::{AuthMode, EngineApiClient};
use plugins::{PluginLoadOutcome, PluginManager, PluginRegistry};
use runtime::{ConfigLoader, ConversationRuntime, PermissionMode, Session, SystemPrompt};
use tools::GlobalToolRegistry;

use crate::config::AllowedToolSet;
use crate::mcp::{
    build_runtime_mcp_state, session_mcp_tool_names, shutdown_mcp_state_best_effort,
    RuntimeMcpState,
};
use crate::tool_executor::{permission_policy, CliToolExecutor};

/// What the HOST supplies to the engine, in place of a working directory.
///
/// Two hosts run this engine: the `scode` CLI on a developer's machine, and a
/// co-hosted agent inside `nexusd-cluster`. They differ in where the agent's
/// files live and what the agent is called — not in any engine behaviour — so
/// those are values a host passes, not facts the engine infers from a path.
///
/// A co-host has no meaningful current directory. Deriving the workspace, the
/// agent's identity and the skills listing from one would force it to invent a
/// plausible path and hope all four derivations agreed with each other.
pub struct HostContext {
    /// Filesystem the file tools read and write through, and the SSOT for the
    /// root relative tool paths resolve against ([`runtime::FsBackend::working_root`]).
    ///
    /// Kernel-backed for both hosts: a co-hosted agent reaches the daemon's
    /// kernel in-process, a CLI session its own
    /// ([`crate::local_kernel::boot_session_fs`]). Which kernel is the host's
    /// choice; that there is one is not, because a hook, a permission gate and
    /// an audit row that exist for one host have to exist for the other.
    pub fs: Arc<dyn runtime::FsBackend>,
    /// Where the host reads its own files from: configuration, the skills and
    /// agent-type listings, and the directory a session's agent name is derived
    /// from when nothing configures one.
    ///
    /// Stays on the HOST disk for both hosts, deliberately: a daemon has to
    /// read its configuration before it can serve the VFS that configuration
    /// describes, so this is the one root that does NOT follow `fs`. It is also
    /// why there is no second "workspace root" beside it — a host root that
    /// answered *some* of these questions differently from `fs` is exactly how
    /// a session ends up announcing one identity and writing as another.
    pub config_root: PathBuf,
    /// The agent's name when the host knows it. `None` means "derive it",
    /// which is what a CLI session does from its directory and config.
    pub agent_name: Option<String>,
    /// The HOST directory this session's host-side execution runs in: `bash`,
    /// `git`, and every hook that resolves `current_workspace_root()`.
    ///
    /// A CLI session's shell root IS its workspace — one directory, reached two
    /// ways. A co-hosted agent's is not and cannot be: its workspace is a VFS
    /// path (`/proc/<pid>/workspace`, a per-agent view of DT_LINKs onto the
    /// repos it was given), and no shell can `cd` into that. So it gets a
    /// directory of its own on the daemon's host, and the model is told the
    /// difference (`cohost_shell_prompt_section`).
    ///
    /// The value matters because the alternative is not "no scope" but "the
    /// daemon's scope": `current_workspace_root()` falls through to the process
    /// working directory, which every co-hosted agent on that daemon shares and
    /// which is the daemon's own checkout.
    pub shell_root: PathBuf,
    /// Where this agent's messages are sent and received, when the host owns
    /// that. `None` means "resolve it", which is what a CLI session does.
    ///
    /// A co-hosted agent's mailbox IS its identity, so it is supplied rather
    /// than derived: a second one resolved here would give `send` a different
    /// address than the one the agent receives on, and neither side can see
    /// the other's answer.
    pub mailbox: Option<Arc<runtime::mailbox::Mailbox>>,
}

impl HostContext {
    /// The CLI's shape: a session at `cwd`, on a kernel of its own.
    ///
    /// Booting a kernel here rather than handing the engine the host disk is
    /// what makes the two hosts one system: the same tools, over the same
    /// `FsBackend`, against the same VFS semantics, differing only in which
    /// kernel answers.
    ///
    /// What the session may reach is `cwd` plus the configured
    /// `additionalDirectories`, and is decided HERE rather than passed in:
    /// every path outside the mounts is refused, so a second caller with a
    /// second list would be a second answer to what a session is allowed to
    /// touch.
    pub fn for_cli_session(cwd: impl Into<PathBuf>) -> io::Result<Self> {
        let cwd = cwd.into();
        let extra_roots = additional_directories(&cwd)?;
        // The name has to be known before the kernel is built: it is the
        // identity every syscall from this session carries, and resolving it
        // afterwards would leave the kernel's view of who is writing and the
        // prompt's claim about who this is free to disagree.
        let agent_name = agent_name_under(&cwd);
        let fs = crate::local_kernel::boot_session_fs(&cwd, &extra_roots, &agent_name)?;
        Ok(Self {
            fs,
            // The CLI's three roots are one directory: what it reads its config
            // from, what its tools address, and where its shell runs.
            shell_root: cwd.clone(),
            config_root: cwd,
            agent_name: Some(agent_name),
            mailbox: None,
        })
    }

    /// The co-host's shape: an agent inside `nexusd-cluster`, on the daemon's
    /// kernel.
    ///
    /// Deliberately adjacent to [`Self::for_cli_session`]: the difference between
    /// the two hosts is the difference between these two functions, and a reader
    /// who has to open two files to find it will not find it. Everything either
    /// host supplies is a VALUE here — no engine behaviour branches on which one
    /// is running.
    ///
    /// * `fs` — the daemon's kernel, so the agent's writes pass the hooks, the
    ///   audit trail and the permission checks that co-hosting exists for.
    /// * `config_root` — the daemon's own directory. A daemon reads its
    ///   configuration before it can serve the VFS that configuration describes.
    /// * `agent_name` / `mailbox` — from the descriptor, which is the SSOT for
    ///   who this agent is. Deriving either here would let `send` address one
    ///   identity while the agent receives on another.
    /// * `shell_root` — created if missing, one per agent under the config home
    ///   so it survives a respawn. See the field's docstring for why it cannot be
    ///   the workspace.
    pub fn for_cohost_agent(
        fs: Arc<dyn runtime::FsBackend>,
        agent_name: &str,
        mailbox: Arc<runtime::mailbox::Mailbox>,
    ) -> io::Result<Self> {
        let shell_root = cohost_shell_root(agent_name);
        std::fs::create_dir_all(&shell_root)?;
        Ok(Self {
            fs,
            shell_root,
            config_root: std::env::current_dir()?,
            agent_name: Some(agent_name.to_string()),
            mailbox: Some(mailbox),
        })
    }

    /// The name this agent answers to.
    ///
    /// One definition rather than the two identical blocks this replaces: the
    /// name feeds both the A2A prompt section and `send` routing, and a
    /// disagreement between them is an agent that describes itself as one peer
    /// and delivers as another.
    #[must_use]
    pub fn resolved_agent_name(&self) -> String {
        match &self.agent_name {
            Some(name) => name.clone(),
            None => agent_name_under(&self.config_root),
        }
    }
}

/// `<config home>/agents/<name>/shell` — a co-hosted agent's own host-side
/// directory.
///
/// Under the config home rather than a temp directory because it is the agent's,
/// not the run's: an agent respawned onto the same identity comes back to the
/// files it left. Per AGENT, not per pid, for the same reason.
fn cohost_shell_root(agent_name: &str) -> PathBuf {
    runtime::default_config_home()
        .join("agents")
        .join(agent_name)
        .join("shell")
}

/// Directories a session under `root` may reach beyond `root` itself, from the
/// `additionalDirectories` configuration key.
///
/// A malformed value is an error rather than an empty list: the key exists to
/// widen what a session can read, so silently ignoring it would present the
/// containment refusal as if the directory were forbidden — with the setting
/// that was meant to allow it sitting right there, apparently applied.
fn additional_directories(root: &Path) -> io::Result<Vec<PathBuf>> {
    // Configuration that will not parse is an error here, not an empty list:
    // what a session may reach comes out of this file, and a session that
    // quietly falls back to "the workspace only" because of a typo elsewhere in
    // it would refuse a directory the config plainly grants.
    let config = ConfigLoader::default_for(root)
        .load()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let Some(value) = config.get("additionalDirectories").cloned() else {
        return Ok(Vec::new());
    };
    let malformed = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "additionalDirectories must be a list of directory paths",
        )
    };
    value
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .map(|entry| entry.as_str().map(PathBuf::from).ok_or_else(malformed))
        .collect()
}

/// The agent name a session under `root` answers to: what the configuration
/// there says, else what [`runtime::mailbox::local_agent_name`] derives from
/// the directory itself.
fn agent_name_under(root: &Path) -> String {
    let configured = ConfigLoader::default_for(root).load().ok().and_then(|rc| {
        rc.get("agentName")
            .and_then(|v| v.as_str().map(str::to_string))
    });
    runtime::mailbox::local_agent_name(configured.as_deref(), root)
}

// === moved from rusty-sudocode-cli/src/main.rs (CORE cluster extraction) ===

pub struct RuntimePluginState {
    pub feature_config: runtime::RuntimeFeatureConfig,
    pub tool_registry: GlobalToolRegistry,
    pub plugin_registry: PluginRegistry,
    pub plugin_load_outcome: PluginLoadOutcome,
    pub mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

/// Groups the non-session parameters threaded through the `build_runtime*`
/// call chain so that adding a new knob only touches one struct instead of
/// 3-4 function signatures and 10+ call sites.
#[derive(Clone)]
pub struct RuntimeConfig {
    pub model: String,
    pub system_prompt: SystemPrompt,
    pub enable_tools: bool,
    pub allowed_tools: Option<AllowedToolSet>,
    pub permission_mode: PermissionMode,
    pub auth_mode: AuthMode,
    pub sudocode_config: engine_core::SudoCodeConfig,
    /// Whether this session uses memory at all. Set from
    /// `_meta.sudocode.memory` on an ACP session; `MemoryMode::Enabled`
    /// (the default) everywhere else. It reaches the permission policy here;
    /// the prompt side is applied by the caller's `system_prompt`.
    pub memory: runtime::memory::MemoryMode,
}

pub struct BuiltRuntime {
    runtime: Option<ConversationRuntime<EngineApiClient, CliToolExecutor>>,
    plugin_registry: PluginRegistry,
    plugin_load_outcome: PluginLoadOutcome,
    plugins_active: bool,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    mcp_active: bool,
}

impl BuiltRuntime {
    /// Take the engine out, leaving the shell behind.
    ///
    /// For a host that drives the runtime itself rather than borrowing it per
    /// turn — the co-host hands it to the mailbox loop. The SHELL must outlive
    /// that loop: its `Drop` shuts down MCP servers and plugins, so dropping it
    /// at spawn time would tear those down under a still-running agent.
    pub fn take_runtime(
        &mut self,
    ) -> Option<ConversationRuntime<EngineApiClient, CliToolExecutor>> {
        self.runtime.take()
    }

    pub fn new(
        runtime: ConversationRuntime<EngineApiClient, CliToolExecutor>,
        plugin_registry: PluginRegistry,
        plugin_load_outcome: PluginLoadOutcome,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            plugin_registry,
            plugin_load_outcome,
            plugins_active: true,
            mcp_state,
            mcp_active: true,
        }
    }

    pub fn with_hook_abort_signal(mut self, hook_abort_signal: runtime::HookAbortSignal) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing hook abort signal");
        self.runtime = Some(runtime.with_hook_abort_signal(hook_abort_signal));
        self
    }

    pub fn with_session_known_date(mut self, date: impl Into<String>) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before overriding session known date");
        self.runtime = Some(runtime.with_session_known_date(date));
        self
    }

    pub fn with_session_known_model(mut self, model: impl Into<String>) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before overriding session known model");
        self.runtime = Some(runtime.with_session_known_model(model));
        self
    }

    /// Set the trace ID for the next request.
    pub fn set_trace_id(&mut self, trace_id: impl Into<String>) {
        if let Some(ref mut runtime) = self.runtime {
            runtime.set_trace_id(trace_id);
        }
    }

    pub fn plugin_load_outcome(&self) -> &PluginLoadOutcome {
        &self.plugin_load_outcome
    }

    /// Mutable access to the wrapped `ConversationRuntime` (the `runtime` field
    /// stays private so `Drop` remains the sole owner of teardown). `None` only
    /// in the impossible window before construction / after a `take`.
    pub fn runtime_mut(
        &mut self,
    ) -> Option<&mut ConversationRuntime<EngineApiClient, CliToolExecutor>> {
        self.runtime.as_mut()
    }

    /// Shared access to the wrapped `ConversationRuntime`.
    pub fn runtime_ref(&self) -> Option<&ConversationRuntime<EngineApiClient, CliToolExecutor>> {
        self.runtime.as_ref()
    }

    /// The engine-side MCP state, if any MCP servers are running for this
    /// session. Backs the seam's `/mcp` action dispatch.
    pub fn mcp_state(&self) -> Option<&Arc<Mutex<RuntimeMcpState>>> {
        self.mcp_state.as_ref()
    }

    fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.plugins_active {
            self.plugin_registry.shutdown()?;
            self.plugins_active = false;
        }
        Ok(())
    }

    fn shutdown_mcp(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp_active {
            if let Some(mcp_state) = &self.mcp_state {
                mcp_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shutdown()?;
            }
            self.mcp_active = false;
        }
        Ok(())
    }

    /// Returns a reference to the session tracer, if available.
    pub fn session_tracer(&self) -> Option<&telemetry::SessionTracer> {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
            .api_client()
            .session_tracer()
    }
}

impl Deref for BuiltRuntime {
    type Target = ConversationRuntime<EngineApiClient, CliToolExecutor>;

    fn deref(&self) -> &Self::Target {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl DerefMut for BuiltRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.runtime
            .as_mut()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl Drop for BuiltRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_mcp();
        let _ = self.shutdown_plugins();
    }
}

pub fn plugin_load_outcome_for_cwd(
    cwd: &Path,
) -> Result<PluginLoadOutcome, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let runtime_config = loader.load()?;
    let plugin_manager = build_plugin_manager(cwd, &loader, &runtime_config);
    Ok(plugin_manager.plugin_registry_report()?.load_outcome())
}

pub fn build_runtime_plugin_state_with_loader(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
) -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    // Surface the settings `experimental` section to the process-global
    // experiments registry BEFORE anything consults a flag (the MCP gate
    // below is one consumer). First call wins; ACP session rebuilds
    // against the same config are no-ops.
    runtime::experiments::init_config_flags(runtime_config.experiments().clone());
    let plugin_manager = build_plugin_manager(cwd, loader, runtime_config);
    let plugin_registry_report = plugin_manager.plugin_registry_report()?;
    let plugin_load_outcome = plugin_registry_report.load_outcome();
    let plugin_registry = plugin_registry_report.into_registry()?;
    let plugin_hook_config =
        runtime_hook_config_from_plugin_hooks(plugin_registry.projected_hooks()?);
    let feature_config = runtime_config
        .feature_config()
        .clone()
        .with_hooks(runtime_config.hooks().merged(&plugin_hook_config));
    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?;
    let (mcp_state, runtime_tools) =
        build_runtime_mcp_state(runtime_config, &plugin_load_outcome, session_mcp)?;
    let tool_registry = match tool_registry.with_runtime_tools(runtime_tools) {
        Ok(tool_registry) => tool_registry,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(Box::new(std::io::Error::other(error)));
        }
    };
    Ok(RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        plugin_load_outcome,
        mcp_state,
    })
}

pub fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_config = runtime_config
        .plugins()
        .to_plugin_manager_config(cwd, loader.config_home());
    PluginManager::new(plugin_config)
}

pub(crate) fn runtime_hook_config_from_plugin_hooks(
    hooks: plugins::ProjectedPluginHooks,
) -> runtime::RuntimeHookConfig {
    runtime::RuntimeHookConfig::new_with_sources(
        hooks
            .pre_tool_use
            .into_iter()
            .map(|entry| (entry.command, entry.plugin_id))
            .collect(),
        hooks
            .post_tool_use
            .into_iter()
            .map(|entry| (entry.command, entry.plugin_id))
            .collect(),
        hooks
            .post_tool_use_failure
            .into_iter()
            .map(|entry| (entry.command, entry.plugin_id))
            .collect(),
    )
}

/// The one place the post-build tail every engine-side runtime (re)build shares
/// lives: build the runtime for `session` under `handle_id`, install the hook
/// abort signal, then apply reasoning-effort + `thinking` config. Callers own
/// the `RuntimeConfig` assembly (their config sources differ) and the
/// surrounding save/tracer logic; this collapses the identical ~15-line tail
/// that had been copied across `SessionEngine::build` / `rebuild_locked` /
/// `set_model_impl` and `AcpCliAgent::build_session` / `handle_acp_model_switch`.
/// The workspace-root scope must already be active for `cwd`.
#[allow(clippy::too_many_arguments)]
pub fn build_engine_runtime(
    host: &HostContext,
    session: Session,
    handle_id: &str,
    config: RuntimeConfig,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
    abort_signal: runtime::HookAbortSignal,
    reasoning_effort: Option<String>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let mut runtime = build_runtime_for_host(host, session, handle_id, config, session_mcp)?;
    runtime = runtime.with_hook_abort_signal(abort_signal);
    if let Some(rt) = runtime.runtime.as_mut() {
        rt.api_client_mut().set_reasoning_effort(reasoning_effort);
        let thinking = ConfigLoader::default_for(&host.config_root)
            .load()
            .map_or(true, |cfg| cfg.thinking());
        rt.api_client_mut().set_thinking_enabled(thinking);
    }
    Ok(runtime)
}

pub fn build_runtime_for_host(
    host: &HostContext,
    session: Session,
    session_id: &str,
    config: RuntimeConfig,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(&host.config_root);
    let file_config = loader.load()?;
    let runtime_plugin_state = build_runtime_plugin_state_with_loader(
        &host.config_root,
        &loader,
        &file_config,
        session_mcp,
    )?;
    build_runtime_with_plugin_state(
        host,
        session,
        session_id,
        config,
        runtime_plugin_state,
        session_mcp,
    )
}

pub(crate) fn build_runtime_with_plugin_state(
    host: &HostContext,
    mut session: Session,
    session_id: &str,
    mut config: RuntimeConfig,
    runtime_plugin_state: RuntimePluginState,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    // Persist the model in session metadata so resumed sessions can report it.
    if session.model.is_none() {
        session.model = Some(config.model.clone());
    }
    let RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        plugin_load_outcome,
        mcp_state,
    } = runtime_plugin_state;
    // Point the built-in file tools at the host's filesystem. Everything
    // above this line is identical for both hosts; this is the line that
    // decides whether a write lands on local disk or in the kernel.
    let tool_registry = tool_registry.with_fs(Arc::clone(&host.fs));
    // Resolve the standalone nexus-A2A session once (fail loud on a partial
    // config or a dial failure). `None` when A2A is off — the fast path that
    // leaves scode behaviour unchanged. Held as `Option<&'static Session>`
    // (Copy) and reused below to advertise, prompt, and wire the send half.
    let a2a = match crate::nexus_a2a::session() {
        Ok(a2a) => a2a,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(Box::new(std::io::Error::other(error)));
        }
    };
    // per-session injected MCP tools bypass the global --allowed-tools gate:
    // they are explicitly requested for this session and their names are only
    // known at runtime, so add their qualified names to the allow-list when
    // one is active. The prefix uses `runtime::mcp_tool_prefix`, which
    // normalizes server names the same way the tool index does (e.g.
    // `github.com` -> `mcp__github_com__`), so non-alphanumeric server names
    // are matched correctly.
    if let Some(allowed) = config.allowed_tools.as_mut() {
        if let Some(mcp_state) = &mcp_state {
            let tools = mcp_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tools_with_server();
            allowed.extend(session_mcp_tool_names(tools, session_mcp));
        }
    }
    // nexus A2A: when configured, keep the peer-reply tool available even under
    // an explicit --allowedTools restriction (absent a restriction it is
    // already advertised). Its network handler is the CliToolExecutor intercept
    // wired below; the co-host advertises the same tool the same way. The tool
    // exists either way — absent A2A it delivers to the workspace mailbox — so
    // this line controls reachability under a restriction, nothing more.
    if a2a.is_some() {
        if let Some(allowed) = config.allowed_tools.as_mut() {
            allowed.extend(["send".to_string()]);
        }
    }
    let policy = match permission_policy(
        config.permission_mode,
        &feature_config,
        &tool_registry,
        &host.config_root,
        config.memory,
    ) {
        Ok(policy) => policy,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(Box::new(std::io::Error::other(error)));
        }
    };
    let mut system_prompt = config.system_prompt.clone();
    // The cwd-derived sections — the skills listing (so the model can name and
    // load a skill without the user knowing it exists, plugin-provided roots
    // included via `plugin_load_outcome`) and the `<available-agent-types>`
    // catalog (so it knows what to pass as `agent_spawn`'s `agent`). Shared
    // with the `scode system-prompt` preview via `commands::cwd_prompt_sections`
    // so the two cannot drift.
    //
    // The catalog lives here and NOT in `agent_spawn`'s description because
    // that description sits in the cached tools block while this list changes
    // whenever a `.md` agent is added — see `runtime::agent_types` for the
    // cache measurement behind the split.
    //
    // This runs for the REPL, `--print`, and ACP sessions alike: they all land
    // in this function via `build_runtime_for_host`.
    //
    // Read from the host's own root, not through `fs`: a skill and an agent
    // type are host installations (they live beside the configuration that
    // declares them), so a co-hosted agent gets the daemon's, exactly as it
    // gets the daemon's auth mode.
    system_prompt.dynamic_sections.extend(cwd_prompt_sections(
        &host.config_root,
        Some(&plugin_load_outcome),
    ));
    // Deferred tools listing: inject `<available-deferred-tools>` so the
    // model knows which tools exist beyond the core set visible in the API
    // `tools` array. Discovery via ToolSearch, direct execution by name.
    let deferred_section = tool_registry.deferred_tools_prompt_section();
    if !deferred_section.is_empty() {
        system_prompt.dynamic_sections.push(deferred_section);
    }
    // A2A: teach the model its identity + how to reply, so a receiving loop
    // knows it can `send` back to a named peer and doesn't echo the message
    // framing. Every receive path renders the shared REPL section; nexus adds
    // its network note (via `peer_system_prompt`), standalone uses the same
    // local identity the `send` routing below resolves.
    if host.mailbox.is_some() {
        // A host that supplied its own mailbox also drives delivery, and the
        // prose describing that framing travels with the driver rather than
        // being guessed here — the co-host's messages arrive wrapped in
        // `[message from <sender>]`, which only its loop knows to emit.
    } else if let Some(session) = a2a {
        system_prompt
            .dynamic_sections
            .push(session.peer_system_prompt());
    } else {
        let self_name = host.resolved_agent_name();
        system_prompt
            .dynamic_sections
            .push(runtime::agent_mailbox::repl_a2a_prompt_section(
                &self_name,
                &[],
            ));
    }
    let client = match EngineApiClient::new(
        session_id,
        &config.sudocode_config,
        &config.model,
        config.auth_mode,
        tool_registry.clone(),
        config.enable_tools,
        config.allowed_tools.clone(),
    ) {
        Ok(client) => client,
        Err(error) => {
            shutdown_mcp_state_best_effort(&mcp_state);
            return Err(error);
        }
    };
    let mut tool_executor = CliToolExecutor::new(
        config.allowed_tools,
        tool_registry.clone(),
        mcp_state.clone(),
    );
    // Hand the dispatcher this session's mailbox: the ONE place local-versus-nexus
    // is decided. Everything that sends resolves the same handle, so a recipient
    // name means the same destination to the `send` tool, to a sub-agent reporting
    // back, and to the receiver tailing its own inbox.
    //
    // No A2A session means no nexus configured, and the resolver answers with
    // workspace-local JSONL — the same code path rather than a fallback branch,
    // which is what stops the two from drifting.
    if let Some(mailbox) = host.mailbox.clone() {
        tool_executor.set_mailbox(mailbox);
    } else if let Some(a2a_session) = a2a {
        tool_executor.set_mailbox(a2a_session.mailbox());
    } else {
        // Standalone (no nexus): route `send` to the shared same-machine pair
        // root under this process's resolved identity, so a peer scode started
        // in another folder receives it (its poller tails the same
        // `{pair_root}/agents/{name}/chat-with-me`). Without this the send would
        // fall back to workspace-local, which two different folders never share.
        // The SAME name the prompt section above announced. It resolved from
        // the host context while this read `current_dir()`, so a session whose
        // directory differed from the process's told the model it was one peer
        // and delivered as another — silently, since neither side can see the
        // other's answer.
        let self_name = host.resolved_agent_name();
        tool_executor.set_mailbox(std::sync::Arc::new(
            runtime::mailbox::Mailbox::workspace_local(
                &runtime::mailbox::local_pair_root(),
                self_name,
            ),
        ));
    }
    let runtime = ConversationRuntime::new_with_features(
        session,
        client,
        tool_executor,
        policy,
        system_prompt,
        &feature_config,
    )
    .with_session_known_date(runtime::today_local())
    .with_session_known_model(config.model.clone());
    // Live plugin-hook progress rides the seam: the observer (the seam's
    // `engine-core` adapter) installs its `HookProgressSink` as the runtime's
    // `hook_progress_reporter` at the start of each turn, so no reporter is
    // injected here.
    if let Err(error) = plugin_registry.initialize() {
        shutdown_mcp_state_best_effort(&mcp_state);
        return Err(Box::new(error));
    }
    Ok(BuiltRuntime::new(
        runtime,
        plugin_registry,
        plugin_load_outcome,
        mcp_state,
    ))
}
