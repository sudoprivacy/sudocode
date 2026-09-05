//! The one live session's engine, below the seam.
//!
//! `SessionEngine` is the `engine_core::EngineDelegate` (turns) **and**
//! `SessionLifecycle` (non-turn ops: model/auth/permission switch, reset,
//! resume, fork, compaction, reads) impl for a single live session. It owns the
//! session's build → run-turn (+ auto-compact) → model-switch → persist
//! lifecycle and returns the seam's neutral report data — nothing renders here.
//! `AcpCliSession` is the per-session state it locks; `ModelSwitchReport` is the
//! report DATA a model switch returns (each renderer formats it its own way).

use std::path::{Path, PathBuf};
use std::time::Instant;

use engine_core::AuthMode;
use plugins::PluginLoadOutcome;
use runtime::{
    estimate_block_tokens, estimate_session_tokens, CompactionConfig, PermissionMode, SystemPrompt,
};
use serde_json::{Map, Value};

use crate::config::{
    default_permission_mode, load_sudocode_config_for_current_dir, load_sudocode_config_for_cwd,
    require_sudocode_config_for_cwd, resolve_auth_mode, resolve_model_alias_with_config,
    resolve_model_switch_auth_mode, resolve_repl_model, AllowedToolSet,
};
use crate::prompt::build_acp_system_prompt;
use crate::runtime_build::{build_engine_runtime, BuiltRuntime, RuntimeConfig};
use crate::session::{
    canonical_session_cwd, context_overflow_user_message, create_managed_session_handle_for,
    load_session_reference, new_cli_session_for, SessionHandle,
};

// === moved from rusty-sudocode-cli/src/main.rs (CORE cluster extraction) ===

pub struct AcpCliSession {
    pub cwd: PathBuf,
    pub handle: SessionHandle,
    pub runtime: BuiltRuntime,
    pub abort_signal: runtime::HookAbortSignal,
    /// Session start time for duration tracking.
    pub started_at: Instant,
    /// per-session injected MCP servers (from session/new or session/load),
    /// reused when the runtime is rebuilt (e.g. model switch) so they
    /// survive across the session's lifetime.
    pub session_mcp_servers: std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
    /// Caller-supplied system-prompt adjustments (`_meta.sudocode.systemPrompt`
    /// / `appendSystemPrompt` on session/new or session/load). Kept on the
    /// session so a runtime rebuild (model switch) re-applies them.
    pub prompt_overrides: runtime::SystemPromptOverrides,
}

/// The CLI's implementation of the seam's [`engine_core::EngineDelegate`] — one
/// live session's runtime, driven the same way for every renderer (the REPL via
/// `EngineSession`, ACP over stdio/ws). It owns one session's build →
/// run-turn (+ auto-compact) → model-switch → persist lifecycle, single-session,
/// returning the seam's neutral `TurnComplete`. Every renderer shares this one
/// core — nothing renders here.
///
/// The active model is the session's own (`session.model`); this type keeps no
/// separate copy.
/// Outcome of a model switch, returned by [`SessionEngine::set_model_impl`] so
/// each seam consumer formats it its own way: the REPL renders a
/// `format_model_switch_report` / `format_model_report`; the pump/ACP path takes
/// `(resolved, available)`. Report DATA only — no formatting crosses the seam.
pub struct ModelSwitchReport {
    /// The model in effect before the switch.
    pub previous: String,
    /// The resolved target model (equal to `previous` when it was a no-op).
    pub resolved: String,
    /// Session message count at switch time (for the report lines).
    pub message_count: usize,
    /// Usage turns at switch time (for the no-op "current model" report).
    pub turns: u32,
    /// `false` when the target equalled the current model — no rebuild happened.
    pub changed: bool,
    /// Config keys ∪ discovery, current model pinned first (for `ModelChanged`).
    pub available: Vec<String>,
}

pub struct SessionEngine {
    session: std::sync::Mutex<AcpCliSession>,
    tokio_runtime: tokio::runtime::Runtime,
    allowed_tools: Option<AllowedToolSet>,
    /// Effective permission mode — the engine's SSOT (the renderer no longer
    /// keeps a copy). Resolved once at build from the CLI override / default;
    /// `/permissions` mutates it in place and every runtime rebuild reads it.
    permission_mode: std::sync::Mutex<PermissionMode>,
    /// Reasoning effort — immutable for now (no `/effort` verb yet).
    reasoning_effort: Option<String>,
    /// Auth-mode override — the engine's SSOT. `None` = auto-resolve from the
    /// model + config; `/auth` pins a concrete mode. Every rebuild reads it.
    auth_mode: std::sync::Mutex<Option<AuthMode>>,
}

impl SessionEngine {
    /// Build a single-session engine for `cwd`. Ports `AcpCliAgent::build_session`.
    ///
    /// `system_prompt` is supplied by the caller (the REPL passes its own
    /// system prompt; the ACP path passes the ACP one) so this one engine core
    /// serves every renderer without baking in a prompt policy.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        cwd: &Path,
        mcp_servers: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
        system_prompt: SystemPrompt,
        model: String,
        model_flag_raw: Option<String>,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode_override: Option<PermissionMode>,
        reasoning_effort: Option<String>,
        auth_mode: Option<AuthMode>,
    ) -> Result<Self, String> {
        let cwd = canonical_session_cwd(cwd)?;
        let _scope = runtime::WorkspaceRootScope::enter(&cwd);
        let resolved_model = if model_flag_raw.is_some() {
            model.clone()
        } else {
            resolve_repl_model(model.clone())
        };
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        let session_state =
            new_cli_session_for(&cwd).map_err(|e| format!("failed to create session: {e}"))?;
        let handle = create_managed_session_handle_for(&cwd, &session_state.session_id)
            .map_err(|e| format!("failed to create session handle: {e}"))?;
        let sudocode_config = require_sudocode_config_for_cwd(&cwd)?;
        let resolved_auth = resolve_auth_mode(&resolved_model, auth_mode, &sudocode_config)
            .map_err(|e| format!("failed to resolve auth mode: {e}"))?;
        let abort_signal = runtime::HookAbortSignal::new();
        let runtime = build_engine_runtime(
            &cwd,
            session_state.with_persistence_path(handle.path.clone()),
            &handle.id,
            RuntimeConfig {
                model: resolved_model.clone(),
                system_prompt,
                enable_tools: true,
                allowed_tools: allowed_tools.clone(),
                permission_mode,
                auth_mode: resolved_auth,
                sudocode_config,
            },
            mcp_servers,
            abort_signal.clone(),
            reasoning_effort.clone(),
        )
        .map_err(|e| format!("failed to build runtime: {e}"))?;
        runtime
            .session()
            .save_to_path(&handle.path)
            .map_err(|e| format!("failed to persist session: {e}"))?;

        let session = AcpCliSession {
            cwd,
            handle,
            runtime,
            abort_signal,
            started_at: Instant::now(),
            session_mcp_servers: mcp_servers.clone(),
            prompt_overrides,
        };
        Ok(Self {
            session: std::sync::Mutex::new(session),
            tokio_runtime: tokio::runtime::Runtime::new()
                .map_err(|e| format!("failed to create engine tokio runtime: {e}"))?,
            allowed_tools,
            permission_mode: std::sync::Mutex::new(permission_mode),
            reasoning_effort,
            auth_mode: std::sync::Mutex::new(auth_mode),
        })
    }

    fn lock_session(&self) -> std::sync::MutexGuard<'_, AcpCliSession> {
        self.session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// THE one runtime-rebuild primitive behind every session swap (model / auth
    /// / permission switch, `/clear`, `/resume`, `/session switch|fork`,
    /// compaction, plugin reload). Rebuilds the locked session's runtime for
    /// `new_session` under `handle`, reading the effective config straight from
    /// the engine's SSOT (`self.permission_mode` / `self.auth_mode` /
    /// `self.reasoning_effort`) and the model from `new_session.model` — the
    /// caller sets that field to the intended effective model, or leaves it
    /// `None` to inherit the current one. Abort signal + prompt overrides come
    /// from the live session. No model/permission/auth params: the engine, not
    /// the renderer, owns that config (audit finding B — one SSOT).
    fn rebuild_locked(
        &self,
        session: &mut AcpCliSession,
        mut new_session: runtime::Session,
        handle: SessionHandle,
    ) -> Result<(), String> {
        let cwd = session.cwd.clone();
        let _scope = runtime::WorkspaceRootScope::enter(&cwd);
        if new_session.model.is_none() {
            new_session.model = session.runtime.session().model.clone();
        }
        let model = new_session.model.clone().unwrap_or_default();
        let permission_mode = self.locked_permission_mode();
        let auth_override = self.auth_override();
        let sudocode_config = load_sudocode_config_for_cwd(&cwd);
        let auth_mode = resolve_model_switch_auth_mode(&model, auth_override, &sudocode_config)
            .map_err(|e| format!("failed to resolve auth mode: {e}"))?;
        let system_prompt = build_acp_system_prompt(&cwd, &session.prompt_overrides)?;
        let runtime = build_engine_runtime(
            &cwd,
            new_session,
            &handle.id,
            RuntimeConfig {
                model,
                system_prompt,
                enable_tools: true,
                allowed_tools: self.allowed_tools.clone(),
                permission_mode,
                auth_mode,
                sudocode_config,
            },
            &session.session_mcp_servers,
            session.abort_signal.clone(),
            self.reasoning_effort.clone(),
        )
        .map_err(|e| e.to_string())?;
        session.runtime = runtime;
        session.handle = handle;
        Ok(())
    }

    /// Effective permission mode (engine SSOT). Private helper behind the
    /// `SessionLifecycle::current_permission_mode` read + every rebuild.
    fn locked_permission_mode(&self) -> PermissionMode {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The raw auth-mode override (`None` = auto-resolve). Internal — callers
    /// wanting the resolved mode use [`Self::resolved_auth_mode`].
    fn auth_override(&self) -> Option<AuthMode> {
        *self
            .auth_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The model in effect (engine SSOT = the session's own model).
    fn session_model(&self) -> String {
        self.lock_session()
            .runtime
            .session()
            .model
            .clone()
            .unwrap_or_default()
    }

    /// The resolved auth mode in effect: the pinned override, else the mode the
    /// current model + config auto-resolve to (what the runtime actually uses).
    fn resolved_auth_mode(&self) -> AuthMode {
        let model = self.session_model();
        let config = load_sudocode_config_for_current_dir();
        resolve_auth_mode(&model, self.auth_override(), &config).unwrap_or(AuthMode::ApiKey)
    }

    /// Config keys ∪ discovery ids, `current` pinned first — the model list the
    /// seam's `ModelChanged` carries and `/model` (no arg) shows.
    fn available_models(&self, current: &str) -> Vec<String> {
        let config = load_sudocode_config_for_current_dir();
        let config_keys: Vec<String> = config.models.keys().cloned().collect();
        let mut available = runtime::model_capabilities::merge_discovery_ids(&config_keys);
        if !available.iter().any(|m| m.eq_ignore_ascii_case(current)) {
            available.insert(0, current.to_string());
        }
        available
    }

    /// The one model-switch implementation, shared by the turn seam
    /// ([`engine_core::EngineDelegate::set_model`]) and the REPL lifecycle
    /// ([`SessionLifecycle::set_model`]) — audit finding A (was 4 copies).
    /// Resolves the alias, and when it differs from the current model rebuilds
    /// the runtime via [`Self::rebuild_locked`] (keeping `session.model` in sync
    /// so auto-compaction reads the right context window). Returns report DATA;
    /// neither caller formats here.
    fn set_model_impl(&self, new_model: &str) -> Result<ModelSwitchReport, String> {
        let resolved = resolve_model_alias_with_config(new_model);
        let (previous, message_count, turns, changed) = {
            let mut session = self.lock_session();
            let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
            let previous = session.runtime.session().model.clone().unwrap_or_default();
            let message_count = session.runtime.session().messages.len();
            let turns = session.runtime.usage().turns();
            if resolved == previous {
                (previous, message_count, turns, false)
            } else {
                let mut new_session = session.runtime.session().clone();
                new_session.model = Some(resolved.clone());
                let handle = session.handle.clone();
                self.rebuild_locked(&mut session, new_session, handle)?;
                (previous, message_count, turns, true)
            }
        };
        let available = self.available_models(&resolved);
        Ok(ModelSwitchReport {
            previous,
            resolved,
            message_count,
            turns,
            changed,
            available,
        })
    }

    /// The one permission-mode implementation, shared by the seam and the REPL:
    /// update the engine SSOT, then flip the live policy's active mode in place
    /// (the lightweight mechanism the ACP path already trusts — no full rebuild).
    fn set_permission_mode_impl(&self, mode: PermissionMode) {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
        let mut session = self.lock_session();
        if let Some(rt) = session.runtime.runtime_mut() {
            rt.permission_policy_mut().set_active_mode(mode);
        }
    }
}

impl engine_core::EngineDelegate for SessionEngine {
    fn run_turn(
        &self,
        blocks: Vec<runtime::ContentBlock>,
        observer: &mut dyn runtime::RuntimeObserver,
        prompter: &mut dyn runtime::PermissionPrompter,
    ) -> Result<engine_core::TurnComplete, String> {
        let mut session = self.lock_session();
        session.abort_signal.reset();
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);

        // Pre-send auto-compaction, budgeted the way the API preflight is
        // (context window minus max output, the fixed per-request overhead, and
        // the autocompact buffer) so a too-large request never reaches the
        // provider — the #545 wedge fix, on the engine turn path. The session's
        // own model is the SSOT for the context-window lookup (build + set_model
        // keep it current). Compacts through the LLM path and rewrites the
        // persisted transcript; the runtime also compacts-and-resends once
        // reactively inside run_turn if the provider still rejects.
        let model = session.runtime.session().model.clone().unwrap_or_default();
        let context_limit = runtime::model_capabilities::context_window_or_default(&model) as usize;
        let max_output_tokens = engine_core::max_tokens_for_model(&model) as usize;
        let overhead_tokens = session
            .runtime
            .api_client()
            .fixed_request_overhead_tokens(session.runtime.system_prompt());
        let buffer_tokens = runtime::autocompact_buffer_tokens(&model) as usize;
        let history_budget =
            context_limit.saturating_sub(max_output_tokens + overhead_tokens + buffer_tokens);
        let prompt_tokens: usize = blocks.iter().map(estimate_block_tokens).sum();
        let estimated_tokens = estimate_session_tokens(session.runtime.session());
        let mut pre_send_compaction = None;
        if estimated_tokens + prompt_tokens > history_budget {
            if let Some(tracer) = session.runtime.session_tracer() {
                tracer.record("auto_compact_check", {
                    let mut attrs = Map::new();
                    attrs.insert(
                        "estimated_tokens".to_string(),
                        Value::Number(estimated_tokens.into()),
                    );
                    attrs.insert(
                        "prompt_tokens".to_string(),
                        Value::Number(prompt_tokens.into()),
                    );
                    attrs.insert(
                        "history_budget".to_string(),
                        Value::Number(history_budget.into()),
                    );
                    attrs.insert(
                        "context_limit".to_string(),
                        Value::Number(context_limit.into()),
                    );
                    attrs
                });
            }
            pre_send_compaction = self
                .tokio_runtime
                .block_on(session.runtime.compact_in_place(CompactionConfig {
                    max_estimated_tokens: 0, // force compaction
                    ..CompactionConfig::default()
                }));
            // Re-estimate against the hard limit the preflight enforces. Still
            // over → classified error instead of a request that will be rejected.
            let new_estimated_tokens = estimate_session_tokens(session.runtime.session());
            if new_estimated_tokens + prompt_tokens + overhead_tokens + max_output_tokens
                > context_limit
            {
                return Err(context_overflow_user_message(
                    session.runtime.session(),
                    new_estimated_tokens + prompt_tokens + overhead_tokens + max_output_tokens,
                    context_limit,
                ));
            }
        }

        let turn_summary = self
            .tokio_runtime
            .block_on(
                session
                    .runtime
                    .run_turn_with_blocks(blocks, Some(prompter), Some(observer)),
            )
            .map_err(|e| e.to_string())?;

        let path = session.handle.path.clone();
        session
            .runtime
            .session()
            .save_to_path(&path)
            .map_err(|e| format!("failed to persist session: {e}"))?;

        Ok(engine_core::TurnComplete {
            iterations: turn_summary.iterations,
            turn_usage: turn_summary.turn_usage,
            session_usage: turn_summary.session_usage,
            cancelled: turn_summary.cancelled,
            response_model: turn_summary.response_model,
            // Prefer the pre-send compaction event; else the runtime's in-turn one.
            auto_compaction: pre_send_compaction.or(turn_summary.auto_compaction),
        })
    }

    fn set_question_prompter(&self, prompter: Box<dyn runtime::QuestionPrompter>) {
        let mut session = self.lock_session();
        if let Some(rt) = session.runtime.runtime_mut() {
            rt.tool_executor_mut().set_question_prompter(prompter);
        }
    }

    fn abort_signal(&self) -> runtime::HookAbortSignal {
        self.lock_session().abort_signal.clone()
    }

    fn set_model(&self, new_model: &str) -> Result<(String, Vec<String>), String> {
        let report = self.set_model_impl(new_model)?;
        Ok((report.resolved, report.available))
    }

    fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), String> {
        self.set_permission_mode_impl(mode);
        Ok(())
    }

    fn handle_slash_command(&self, line: &str) -> Result<String, String> {
        // `/model <name>` switches the model; other slash commands are handled by
        // the renderer locally (the REPL intercepts them before they reach the
        // engine). Kept minimal here; the seam only needs the model verb.
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("/model") {
            let arg = rest.trim();
            if arg.is_empty() {
                let current = self
                    .lock_session()
                    .runtime
                    .session()
                    .model
                    .clone()
                    .unwrap_or_default();
                return Ok(format!("current model: {current}"));
            }
            let resolved = self.set_model_impl(arg)?.resolved;
            return Ok(format!("switched model to {resolved}"));
        }
        Ok(String::new())
    }

    fn close(&self) {
        let session = self.lock_session();
        let path = session.handle.path.clone();
        let _ = session.runtime.session().save_to_path(&path);
    }
}

/// The non-turn session-lifecycle contract the composition root (`LiveCli`) uses
/// to manage a live engine session **without being able to drive turns** — turns
/// go only through `EngineHandle`. Split from [`engine_core::EngineDelegate`]
/// (turn ops) so a renderer physically cannot bypass the seam: holding an
/// `Arc<dyn SessionLifecycle>` gives no access to `run_turn`. SRP: turn-driving
/// and session-management are two orthogonal responsibilities.
///
/// Methods are dyn-safe (no generics) and return owned snapshots — these are
/// rare, non-hot-path management ops (export / status / slash / undo), so the
/// clones cost nothing on the critical path.
pub trait SessionLifecycle: Send + Sync + 'static {
    /// A clone of the current session (for export / status / read inspection).
    fn session_snapshot(&self) -> runtime::Session;
    /// The active session's handle (id + persistence path). Owned clone so a
    /// slash-command handler can name the session without touching the runtime
    /// mid-turn.
    fn session_handle(&self) -> SessionHandle;
    /// Persist the session to its backing path.
    fn persist(&self) -> Result<(), String>;
    /// Mutate the session in place (undo, fork prep, …).
    fn with_session_mut(&self, f: &mut dyn FnMut(&mut runtime::Session));
    /// Snapshot of the runtime's cumulative/turn usage tracker (for
    /// `/status`, `/cost`, `/stats`, resume/model reports). Cloned, not
    /// borrowed, so it never pins the session lock.
    fn usage_snapshot(&self) -> runtime::UsageTracker;
    /// Estimated token footprint of the current session (for `/status`).
    fn estimated_tokens(&self) -> usize;
    /// A clone of the session tracer, if telemetry is active. Returned owned
    /// (the tracer is `Arc`-backed and cheap to clone) so callers record events
    /// without holding the session lock.
    fn session_tracer(&self) -> Option<telemetry::SessionTracer>;
    /// A snapshot of the plugin load outcome (for `/skills` resolution).
    fn plugin_load_outcome(&self) -> PluginLoadOutcome;
    /// Run an `/mcp reconnect|enable|disable <server>` action against the live
    /// MCP state. `None` when no MCP servers are running in this session; else
    /// the action's `Ok(message)` / `Err(message)`.
    fn mcp_command(&self, action: &str, server: &str) -> Option<Result<String, String>>;

    // --- config reads (engine SSOT) ------------------------------------------
    /// The model in effect.
    fn current_model(&self) -> String;
    /// The effective permission mode.
    fn current_permission_mode(&self) -> PermissionMode;
    /// The resolved auth mode in effect.
    fn current_auth_mode(&self) -> AuthMode;

    // --- semantic session ops (engine owns the rebuild; renderer only formats)-
    /// Switch the model. Returns report DATA (`previous` / `resolved` /
    /// `changed` / counts / `available`); the renderer formats it. Shares the
    /// one impl with [`engine_core::EngineDelegate::set_model`].
    fn set_model(&self, new_model: &str) -> Result<ModelSwitchReport, String>;
    /// Pin the auth mode (engine SSOT) and rebuild so it takes effect.
    fn set_auth(&self, mode: AuthMode) -> Result<(), String>;
    /// Switch the active permission mode (engine SSOT) in place.
    fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), String>;
    /// Start a fresh session (`/clear`), preserving the current model. Returns
    /// the new session handle for the renderer to adopt + report.
    fn reset_session(&self) -> Result<SessionHandle, String>;
    /// Resume a session by reference (`/resume`, `/session switch`): the engine
    /// loads it and swaps it in, keeping the current effective model. Returns
    /// the new handle + its message count.
    fn resume_session(&self, reference: &str) -> Result<(SessionHandle, usize), String>;
    /// Fork the current session (`/session fork`). Returns the new handle, its
    /// message count, and the branch name (if any).
    fn fork_session(
        &self,
        branch: Option<String>,
    ) -> Result<(SessionHandle, usize, Option<String>), String>;
    /// Rebuild the runtime to pick up reloaded plugin / feature state, then
    /// persist (`/plugins` reload).
    fn reload_features(&self) -> Result<(), String>;
    /// Run LLM-based history compaction and swap the compacted session in.
    /// Returns `(removed, kept, skipped)` for the renderer's report.
    fn run_compaction(
        &self,
    ) -> Result<(usize, usize, bool, runtime::CompactionSummarySource), String>;
}

impl SessionLifecycle for SessionEngine {
    fn session_snapshot(&self) -> runtime::Session {
        self.lock_session().runtime.session().clone()
    }

    fn session_handle(&self) -> SessionHandle {
        self.lock_session().handle.clone()
    }

    fn persist(&self) -> Result<(), String> {
        let session = self.lock_session();
        let path = session.handle.path.clone();
        session
            .runtime
            .session()
            .save_to_path(&path)
            .map_err(|e| e.to_string())
    }

    fn with_session_mut(&self, f: &mut dyn FnMut(&mut runtime::Session)) {
        let mut session = self.lock_session();
        f(session.runtime.session_mut());
    }

    fn usage_snapshot(&self) -> runtime::UsageTracker {
        self.lock_session().runtime.usage().clone()
    }

    fn estimated_tokens(&self) -> usize {
        self.lock_session().runtime.estimated_tokens()
    }

    fn session_tracer(&self) -> Option<telemetry::SessionTracer> {
        self.lock_session().runtime.session_tracer().cloned()
    }

    fn plugin_load_outcome(&self) -> PluginLoadOutcome {
        self.lock_session().runtime.plugin_load_outcome().clone()
    }

    fn mcp_command(&self, action: &str, server: &str) -> Option<Result<String, String>> {
        let session = self.lock_session();
        let mcp_state = session.runtime.mcp_state()?;
        let mut mcp = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = match action {
            "reconnect" => mcp.reconnect_server(server),
            "enable" => mcp.enable_server(server),
            "disable" => mcp.disable_server(server),
            other => return Some(Err(format!("unknown /mcp action: {other}"))),
        };
        Some(result.map_err(|e| e.to_string()))
    }

    fn current_model(&self) -> String {
        self.session_model()
    }

    fn current_permission_mode(&self) -> PermissionMode {
        self.locked_permission_mode()
    }

    fn current_auth_mode(&self) -> AuthMode {
        self.resolved_auth_mode()
    }

    fn set_model(&self, new_model: &str) -> Result<ModelSwitchReport, String> {
        self.set_model_impl(new_model)
    }

    fn set_auth(&self, mode: AuthMode) -> Result<(), String> {
        *self
            .auth_mode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(mode);
        let mut session = self.lock_session();
        let new_session = session.runtime.session().clone();
        let handle = session.handle.clone();
        self.rebuild_locked(&mut session, new_session, handle)
    }

    fn set_permission_mode(&self, mode: PermissionMode) -> Result<(), String> {
        self.set_permission_mode_impl(mode);
        Ok(())
    }

    fn reset_session(&self) -> Result<SessionHandle, String> {
        let mut session = self.lock_session();
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
        let current_model = session.runtime.session().model.clone();
        let session_state = new_cli_session_for(&session.cwd).map_err(|e| e.to_string())?;
        let handle = create_managed_session_handle_for(&session.cwd, &session_state.session_id)
            .map_err(|e| e.to_string())?;
        let mut fresh = session_state.with_persistence_path(handle.path.clone());
        fresh.model = current_model;
        self.rebuild_locked(&mut session, fresh, handle.clone())?;
        Ok(handle)
    }

    fn resume_session(&self, reference: &str) -> Result<(SessionHandle, usize), String> {
        let mut session = self.lock_session();
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
        let (handle, mut loaded) = load_session_reference(reference).map_err(|e| e.to_string())?;
        let message_count = loaded.messages.len();
        // Keep the current effective model (REPL parity: the runtime model is
        // config-driven, not adopted from the resumed session).
        loaded.model = session.runtime.session().model.clone();
        self.rebuild_locked(&mut session, loaded, handle.clone())?;
        Ok((handle, message_count))
    }

    fn fork_session(
        &self,
        branch: Option<String>,
    ) -> Result<(SessionHandle, usize, Option<String>), String> {
        let mut session = self.lock_session();
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
        let forked = session.runtime.fork_session(branch);
        let handle = create_managed_session_handle_for(&session.cwd, &forked.session_id)
            .map_err(|e| e.to_string())?;
        let branch_name = forked
            .fork
            .as_ref()
            .and_then(|fork| fork.branch_name.clone());
        let forked = forked.with_persistence_path(handle.path.clone());
        let message_count = forked.messages.len();
        forked
            .save_to_path(&handle.path)
            .map_err(|e| e.to_string())?;
        self.rebuild_locked(&mut session, forked, handle.clone())?;
        Ok((handle, message_count, branch_name))
    }

    fn reload_features(&self) -> Result<(), String> {
        let mut session = self.lock_session();
        let new_session = session.runtime.session().clone();
        let handle = session.handle.clone();
        self.rebuild_locked(&mut session, new_session, handle)?;
        let path = session.handle.path.clone();
        session
            .runtime
            .session()
            .save_to_path(&path)
            .map_err(|e| e.to_string())
    }

    fn run_compaction(
        &self,
    ) -> Result<(usize, usize, bool, runtime::CompactionSummarySource), String> {
        let mut session = self.lock_session();
        let cwd = session.cwd.clone();
        let _scope = runtime::WorkspaceRootScope::enter(&cwd);
        let result = self
            .tokio_runtime
            .block_on(session.runtime.compact(CompactionConfig::default(), None));
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        // Surface the summary provenance to the renderer's `/compact` report
        // (LLM vs heuristic) — main's compaction hardening added this column.
        let summary_source = result.summary_source;
        let handle = session.handle.clone();
        self.rebuild_locked(&mut session, result.compacted_session, handle)?;
        let path = session.handle.path.clone();
        session
            .runtime
            .session()
            .save_to_path(&path)
            .map_err(|e| e.to_string())?;
        Ok((removed, kept, skipped, summary_source))
    }
}
