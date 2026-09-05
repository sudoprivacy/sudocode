//! Runtime construction below the seam.
//!
//! Home of the `ConversationRuntime` wrapper (`BuiltRuntime`), the non-session
//! `RuntimeConfig` / `RuntimePluginState` inputs threaded through construction,
//! and the `build_runtime*` chain that assembles plugins, MCP, permission
//! policy, the system prompt, and the `EngineApiClient`. Both renderers build a
//! session through here. Live plugin-hook progress rides the engine↔renderer
//! seam as `EngineEvent::HookProgress` (installed per-turn from the observer),
//! so no renderer type is named below the seam.

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Mutex};

use commands::render_skills_prompt_section;
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
    cwd: &Path,
    session: Session,
    handle_id: &str,
    config: RuntimeConfig,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
    abort_signal: runtime::HookAbortSignal,
    reasoning_effort: Option<String>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let mut runtime = build_runtime_for_cwd(cwd, session, handle_id, config, session_mcp)?;
    runtime = runtime.with_hook_abort_signal(abort_signal);
    if let Some(rt) = runtime.runtime.as_mut() {
        rt.api_client_mut().set_reasoning_effort(reasoning_effort);
        let thinking = ConfigLoader::default_for(cwd)
            .load()
            .map_or(true, |cfg| cfg.thinking());
        rt.api_client_mut().set_thinking_enabled(thinking);
    }
    Ok(runtime)
}

pub fn build_runtime_for_cwd(
    cwd: &Path,
    session: Session,
    session_id: &str,
    config: RuntimeConfig,
    session_mcp: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let loader = ConfigLoader::default_for(cwd);
    let file_config = loader.load()?;
    let runtime_plugin_state =
        build_runtime_plugin_state_with_loader(cwd, &loader, &file_config, session_mcp)?;
    build_runtime_with_plugin_state(
        cwd,
        session,
        session_id,
        config,
        runtime_plugin_state,
        session_mcp,
    )
}

pub(crate) fn build_runtime_with_plugin_state(
    cwd: &Path,
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
    // already advertised). Its handler is the CliToolExecutor intercept wired
    // below; the co-host advertises the same tool the same way.
    if a2a.is_some() {
        if let Some(allowed) = config.allowed_tools.as_mut() {
            allowed.extend(["send_message".to_string()]);
        }
    }
    let policy =
        match permission_policy(config.permission_mode, &feature_config, &tool_registry, cwd) {
            Ok(policy) => policy,
            Err(error) => {
                shutdown_mcp_state_best_effort(&mcp_state);
                return Err(Box::new(std::io::Error::other(error)));
            }
        };
    let mut system_prompt = config.system_prompt.clone();
    // Skills are listed so the model can name and load one without the user
    // having to know it exists. Plugin-provided skill roots are included via
    // `plugin_load_outcome`, so a plugin can inject skills that the prompt then
    // advertises. This runs for the REPL, `--print`, and ACP sessions alike:
    // they all land in this function via `build_runtime_for_cwd`.
    if let Some(section) = render_skills_prompt_section(cwd, Some(&plugin_load_outcome)) {
        system_prompt.dynamic_sections.push(section);
    }
    // nexus A2A: teach the model its A2A identity + how to reach peers, so the
    // standalone loop knows it can `send_message` to a named peer.
    if let Some(session) = a2a {
        system_prompt
            .dynamic_sections
            .push(session.peer_system_prompt());
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
    let mut runtime = ConversationRuntime::new_with_features(
        session,
        client,
        CliToolExecutor::new(
            config.allowed_tools,
            tool_registry.clone(),
            mcp_state.clone(),
        ),
        policy,
        system_prompt,
        &feature_config,
    )
    .with_session_known_date(runtime::today_local());
    // nexus A2A: give the CLI executor the send half so `send_message` routes
    // to the peer's replicated DT_STREAM inbox (the shared handler the co-host
    // uses). Set only when configured; absent it the tool is never advertised.
    if let Some(session) = a2a {
        runtime
            .tool_executor_mut()
            .set_mailbox_sender(session.sender());
    }
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
