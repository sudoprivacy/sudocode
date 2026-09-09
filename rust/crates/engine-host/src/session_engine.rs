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
use std::time::{Duration, Instant};

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

/// Resolve which model a resumed session runs on — the Claude Code rule, where
/// the config is the single source of truth for the model.
///
/// - An explicit `--model` flag always wins (`model_flag_raw` is `Some`).
/// - Otherwise the current config default is used (`resolve_repl_model`), so
///   changing the global model applies to resumed sessions too.
///
/// The persisted transcript's `session.model` is deliberately NOT consulted: it
/// is only a descriptive record of the model the transcript last ran on (shown
/// in the resume list / `/status`), never the selector. `/model` switches within
/// a session are runtime-only and do not survive resume. Auto-compaction sizes
/// the context window from the runtime's active model (this same config SSOT),
/// so it stays correct regardless of what the transcript records.
///
/// `already_resolved_default` is the model the caller already computed for the
/// no-flag case (the config default); when a flag was passed it is the resolved
/// flag value. Kept as one helper so every resume path resolves identically.
fn resume_model_from_config_ssot(
    model_flag_raw: Option<&str>,
    already_resolved_default: &str,
) -> String {
    match model_flag_raw {
        Some(_) => already_resolved_default.to_string(),
        None => resolve_repl_model(already_resolved_default.to_string()),
    }
}

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
    /// Whether THIS session uses memory (`_meta.sudocode.memory` on
    /// session/new or session/load). One process serves many sessions, so
    /// the mode lives here rather than anywhere process-wide; kept on the
    /// session for the same reason as `prompt_overrides` — a runtime rebuild
    /// (model switch, compaction, fork) must re-apply it.
    pub memory: runtime::memory::MemoryMode,
}

/// The CLI's implementation of the seam's [`engine_core::EngineDelegate`] — one
/// live session's runtime, driven the same way for every renderer (the REPL via
/// `EngineSession`, ACP over stdio/ws). It owns one session's build →
/// run-turn (+ auto-compact) → model-switch → persist lifecycle, single-session,
/// returning the seam's neutral `TurnComplete`. Every renderer shares this one
/// core — nothing renders here.
///
/// The active model is the model the runtime was built with from the config
/// SSOT (see [`resume_model_from_config_ssot`] and `ConversationRuntime`'s
/// `running_model`); `session.model` is only a descriptive record.
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

/// Report DATA from a cancellable `/compact` ([`SessionEngine::compact_cancellable`],
/// the ACP path). The renderer formats it (e.g. `format_acp_compact_report`);
/// no formatting crosses the seam.
pub struct CompactionOutcome {
    /// `true` when a `session/cancel` aborted the compaction mid-round-trip (or
    /// landed after it): the transcript in memory and on disk is untouched.
    pub cancelled: bool,
    /// Estimated session tokens before compaction.
    pub before_tokens: usize,
    /// Estimated session tokens after (equal to `before_tokens` when skipped).
    pub after_tokens: usize,
    /// Messages removed (0 when skipped or cancelled).
    pub removed: usize,
    /// Messages kept.
    pub kept: usize,
    /// `Some((method, summary_source))` when messages were actually removed;
    /// `None` when skipped or cancelled — matching the `method` argument shape
    /// of `commands::reports::format_acp_compact_report`.
    pub method: Option<(runtime::CompactionMethod, runtime::CompactionSummarySource)>,
}

pub struct SessionEngine {
    session: std::sync::Mutex<AcpCliSession>,
    /// The session's blocking runtime for turn work. `Option` so `Drop` can take
    /// it and `shutdown_background()` it: the seam pump owns this delegate and
    /// drops it from inside its own async context (`rt.block_on(drive)`), and a
    /// plain `Runtime` drop there panics ("cannot drop a runtime in an async
    /// context"). `shutdown_background` tears down without blocking.
    tokio_runtime: Option<tokio::runtime::Runtime>,
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
        memory: runtime::memory::MemoryMode,
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
                memory,
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
            memory,
        };
        Ok(Self {
            session: std::sync::Mutex::new(session),
            tokio_runtime: Some(
                tokio::runtime::Runtime::new()
                    .map_err(|e| format!("failed to create engine tokio runtime: {e}"))?,
            ),
            allowed_tools,
            permission_mode: std::sync::Mutex::new(permission_mode),
            reasoning_effort,
            auth_mode: std::sync::Mutex::new(auth_mode),
        })
    }

    /// Build a single-session engine wrapping an **already-persisted**
    /// transcript (`session` under `handle`) — the ACP `session/load` and
    /// `session/new` fork paths, where the transcript already exists on disk and
    /// must be adopted as-is (not recreated, not re-saved here). Mirrors
    /// [`Self::build`]'s model / permission / auth resolution; the caller
    /// supplies the resolved `system_prompt` (the ACP one) exactly as for
    /// `build`. Ports `AcpSdkDelegate::open_persisted_session`.
    #[allow(clippy::too_many_arguments)]
    pub fn open_persisted(
        cwd: &Path,
        handle: SessionHandle,
        session: runtime::Session,
        mcp_servers: &std::collections::BTreeMap<String, runtime::ScopedMcpServerConfig>,
        prompt_overrides: runtime::SystemPromptOverrides,
        memory: runtime::memory::MemoryMode,
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
        // Model resolution on resume follows Claude Code: the config is the
        // single source of truth. See [`resume_model_from_config_ssot`].
        let resolved_model = resume_model_from_config_ssot(model_flag_raw.as_deref(), &model);
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        let sudocode_config = require_sudocode_config_for_cwd(&cwd)?;
        let resolved_auth = resolve_auth_mode(&resolved_model, auth_mode, &sudocode_config)
            .map_err(|e| format!("failed to resolve auth mode: {e}"))?;
        let abort_signal = runtime::HookAbortSignal::new();
        // Adopt the persisted transcript verbatim — no `new_cli_session_for`, no
        // `save_to_path` (it is already on disk at `handle.path`; re-saving here
        // would rewrite a transcript the turn loop has not touched yet).
        let runtime = build_engine_runtime(
            &cwd,
            session,
            &handle.id,
            RuntimeConfig {
                model: resolved_model,
                system_prompt,
                enable_tools: true,
                allowed_tools: allowed_tools.clone(),
                permission_mode,
                auth_mode: resolved_auth,
                sudocode_config,
                memory,
            },
            mcp_servers,
            abort_signal.clone(),
            reasoning_effort.clone(),
        )
        .map_err(|e| format!("failed to build runtime: {e}"))?;

        let session = AcpCliSession {
            cwd,
            handle,
            runtime,
            abort_signal,
            started_at: Instant::now(),
            session_mcp_servers: mcp_servers.clone(),
            prompt_overrides,
            memory,
        };
        Ok(Self {
            session: std::sync::Mutex::new(session),
            tokio_runtime: Some(
                tokio::runtime::Runtime::new()
                    .map_err(|e| format!("failed to create engine tokio runtime: {e}"))?,
            ),
            allowed_tools,
            permission_mode: std::sync::Mutex::new(permission_mode),
            reasoning_effort,
            auth_mode: std::sync::Mutex::new(auth_mode),
        })
    }

    /// The session's blocking runtime, present for the delegate's whole life
    /// (only `Drop` takes it).
    fn rt(&self) -> &tokio::runtime::Runtime {
        self.tokio_runtime
            .as_ref()
            .expect("session engine tokio runtime present until drop")
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
        // The rebuilt runtime is stamped with today's date. Carry the outgoing
        // one's date forward instead: a session that started yesterday and is
        // rebuilt today (a `/model` switch, a fork, a compaction) would
        // otherwise have its known date silently advanced, suppressing the
        // date-rollover reminder (#128, issue #135). The *model* is deliberately
        // not carried — a rebuild is where it legitimately changes.
        let inherited_known_date = session.runtime.prompt_known_date().map(str::to_string);
        if new_session.model.is_none() {
            new_session.model = session.runtime.session().model.clone();
        }
        let model = new_session.model.clone().unwrap_or_default();
        let permission_mode = self.locked_permission_mode();
        let auth_override = self.auth_override();
        let sudocode_config = load_sudocode_config_for_cwd(&cwd);
        let auth_mode = resolve_model_switch_auth_mode(&model, auth_override, &sudocode_config)
            .map_err(|e| format!("failed to resolve auth mode: {e}"))?;
        let system_prompt =
            build_acp_system_prompt(&cwd, &session.prompt_overrides, session.memory)?;
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
                memory: session.memory,
            },
            &session.session_mcp_servers,
            session.abort_signal.clone(),
            self.reasoning_effort.clone(),
        )
        .map_err(|e| e.to_string())?;
        let runtime = match inherited_known_date {
            Some(known) => runtime.with_session_known_date(known),
            None => runtime,
        };
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

    /// Run an explicit `/compact` that a `session/cancel` can abort mid-flight
    /// (the ACP path; the REPL's [`SessionLifecycle::run_compaction`] is not
    /// cancellable, since the REPL has no concurrent cancel channel). Compacts
    /// through the LLM path with a local-heuristic fallback, installs + persists
    /// the compacted transcript when anything was removed, and records the same
    /// telemetry the ACP `/compact` always did. A cancel during the model
    /// round-trip — or one that lands right after it — leaves the transcript
    /// untouched and returns `cancelled`. Returns report DATA; the renderer
    /// formats it. Ports `AcpCliAgent::handle_acp_compact`.
    pub fn compact_cancellable(&self) -> Result<CompactionOutcome, String> {
        let mut session = self.lock_session();
        let _scope = runtime::WorkspaceRootScope::enter(&session.cwd);
        // Fresh turn: a cancel left over from an earlier turn must not abort this
        // one (mirrors `run_turn`).
        session.abort_signal.reset();
        let abort_signal = session.abort_signal.clone();
        let before_tokens = estimate_session_tokens(session.runtime.session());
        let config = CompactionConfig {
            max_estimated_tokens: 0,
            ..CompactionConfig::default()
        };
        let compaction = self.rt().block_on(async {
            tokio::select! {
                result = session.runtime.compact_with_method(config, None) => Some(result),
                () = wait_for_abort(&abort_signal) => None,
            }
        });
        let Some((result, method)) = compaction.filter(|_| !abort_signal.is_aborted()) else {
            if let Some(tracer) = session.runtime.session_tracer() {
                tracer.record("slash_compact_cancelled", Map::new());
            }
            return Ok(CompactionOutcome {
                cancelled: true,
                before_tokens,
                after_tokens: before_tokens,
                removed: 0,
                kept: session.runtime.session().messages.len(),
                method: None,
            });
        };
        let removed = result.removed_message_count;
        let summary_source = result.summary_source;
        if removed > 0 {
            *session.runtime.session_mut() = result.compacted_session;
            let path = session.handle.path.clone();
            session
                .runtime
                .session()
                .save_to_path(&path)
                .map_err(|e| format!("failed to persist compacted session: {e}"))?;
        }
        let kept = session.runtime.session().messages.len();
        let after_tokens = estimate_session_tokens(session.runtime.session());
        if let Some(tracer) = session.runtime.session_tracer() {
            tracer.record("slash_compact", {
                let mut attrs = Map::new();
                attrs.insert(
                    "method".to_string(),
                    Value::String(method.as_str().to_string()),
                );
                attrs.insert(
                    "removed_messages".to_string(),
                    Value::Number(removed.into()),
                );
                attrs.insert(
                    "tokens_before".to_string(),
                    Value::Number(before_tokens.into()),
                );
                attrs.insert(
                    "tokens_after".to_string(),
                    Value::Number(after_tokens.into()),
                );
                attrs
            });
        }
        Ok(CompactionOutcome {
            cancelled: false,
            before_tokens,
            after_tokens,
            removed,
            kept,
            method: (removed > 0).then_some((method, summary_source)),
        })
    }

    /// Set the trace id the next turn's requests carry (ACP `_meta.traceId`).
    pub fn set_trace_id(&self, trace_id: &str) {
        self.lock_session()
            .runtime
            .set_trace_id(trace_id.to_string());
    }

    /// Push each block as its own `User` message onto the live session before a
    /// turn runs — the ACP image path pre-loads native / VLM-described image
    /// blocks this way (the renderer prepares the blocks; the engine only
    /// appends them). Ports the message-push half of `AcpSdkDelegate::push_images`.
    pub fn push_user_blocks(&self, blocks: Vec<runtime::ContentBlock>) -> Result<(), String> {
        let mut session = self.lock_session();
        for block in blocks {
            let msg = runtime::ConversationMessage {
                role: runtime::MessageRole::User,
                blocks: vec![block],
                usage: None,
                model: None,
            };
            session
                .runtime
                .session_mut()
                .push_message(msg)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// Resolve once `signal` is aborted. Polls as well as awaiting the signal's
/// notification, so an `abort()` that races the subscription is still seen
/// promptly. Ported from the ACP `/compact` path (`main.rs::wait_for_abort`).
async fn wait_for_abort(signal: &runtime::HookAbortSignal) {
    loop {
        if signal.is_aborted() {
            return;
        }
        tokio::select! {
            () = signal.cancelled() => {}
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

impl Drop for SessionEngine {
    fn drop(&mut self) {
        // The seam pump (`EngineSession::spawn`) owns this delegate and, on a
        // one-shot exit, drops the last `Arc` from inside its own async context
        // (`rt.block_on(drive)`). Dropping a `tokio::runtime::Runtime` there
        // panics ("Cannot drop a runtime in a context where blocking is not
        // allowed"). `shutdown_background` tears the runtime down without
        // blocking, so it is safe in an async context; on the normal (main-
        // thread) drop no turn tasks remain in flight, so it is equivalent.
        if let Some(rt) = self.tokio_runtime.take() {
            rt.shutdown_background();
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
            pre_send_compaction =
                self.rt()
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
            .rt()
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

/// Who a session's requests are billed to.
///
/// A session spends someone's money, and until this crossed the seam nothing on
/// screen said whose. The account is resolved per request from layered config,
/// so it can be one the user never chose — an `auth_profile` inherited from a
/// parent directory, or simply the first account in the file. That is fine
/// right up until the bill arrives on the wrong account, which is why the rule
/// that chose it travels with the name rather than being left to be inferred.
#[derive(Debug, Clone)]
pub enum BillingAccount {
    /// A named account under `auth_modes.proxy`, and the rule that chose it.
    Proxy {
        name: String,
        source: engine_core::AccountSource,
    },
    /// The session is not on a proxy account — its auth mode bills no named
    /// account, so there is nothing to show.
    NotProxied,
    /// The selector refused to resolve one. Carries its message: this is the
    /// case worth showing, because the same refusal is what a request will hit.
    Unresolved(String),
}

impl BillingAccount {
    /// The account name, when one resolved — for the per-turn status line,
    /// which has room for the answer but not the reasoning.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Proxy { name, .. } => Some(name.as_str()),
            Self::NotProxied | Self::Unresolved(_) => None,
        }
    }

    /// A stable token for the deciding rule, for machine-readable reports.
    #[must_use]
    pub fn source_label(&self) -> &'static str {
        match self {
            Self::Proxy { source, .. } => source.describe(),
            Self::NotProxied => "not proxied",
            Self::Unresolved(_) => "unresolved",
        }
    }

    /// One line for `/status`: who pays, and what made it them.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Proxy { name, source } => format!("{name} (chosen by {})", source.describe()),
            Self::NotProxied => "none (this auth mode bills no named account)".to_string(),
            Self::Unresolved(err) => format!("unresolved — {err}"),
        }
    }
}

/// Every account configured under `auth_modes.proxy`, in config order.
#[must_use]
pub fn configured_proxy_accounts(config: &engine_core::SudoCodeConfig) -> Vec<String> {
    config
        .auth_modes
        .get("proxy")
        .map(|accounts| accounts.keys().cloned().collect())
        .unwrap_or_default()
}

/// Who a request for `model` from the current directory would be billed to.
///
/// The engine-side entry point for the question, so `/status` in a live REPL,
/// `scode status` with no engine at all, and the ACP `/status` all get the
/// answer from the one selector the request path uses. A report that re-derives
/// the decision can disagree with the code that spends the money — which is
/// exactly how an unhonored `auth_profile` stayed invisible while every request
/// billed a different account.
#[must_use]
pub fn billing_account_for_model(model: &str, auth_override: Option<AuthMode>) -> BillingAccount {
    let config = load_sudocode_config_for_current_dir();
    if resolve_auth_mode(model, auth_override, &config).unwrap_or(AuthMode::ApiKey)
        != AuthMode::Proxy
    {
        return BillingAccount::NotProxied;
    }
    match engine_core::proxy_account_for_model(&config, model) {
        Ok(selected) => BillingAccount::Proxy {
            name: selected.name.to_string(),
            source: selected.source,
        },
        Err(err) => BillingAccount::Unresolved(err.to_string()),
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
    /// Who this session's requests are billed to, and why that account.
    fn current_billing_account(&self) -> BillingAccount;
    /// Every account configured under `auth_modes.proxy`, in config order —
    /// the set `set_billing_account` will accept.
    fn billing_accounts(&self) -> Vec<String>;
    /// Point this project at a named proxy account: persist the selection and
    /// rebuild so the live session bills it from the next turn on. Returns the
    /// account now in effect.
    ///
    /// The rebuild is the point. Persisting alone would leave the session
    /// spending the old account while every report named the new one — the
    /// same invisible divergence between what is reported and what is billed
    /// that [`BillingAccount`] exists to close.
    fn set_billing_account(&self, name: &str) -> Result<BillingAccount, String>;

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

    fn current_billing_account(&self) -> BillingAccount {
        billing_account_for_model(&self.session_model(), self.auth_override())
    }

    fn billing_accounts(&self) -> Vec<String> {
        configured_proxy_accounts(&load_sudocode_config_for_current_dir())
    }

    fn set_billing_account(&self, name: &str) -> Result<BillingAccount, String> {
        let name = name.trim();
        // Refuse an account that is not configured rather than writing it and
        // letting the next request fail: the selector treats an unhonored
        // selection as fatal precisely so it never quietly bills another
        // account, and a `/account` that accepted a typo would just move that
        // failure to a place the user has stopped looking.
        let available = configured_proxy_accounts(&load_sudocode_config_for_current_dir());
        if !available.iter().any(|candidate| candidate == name) {
            return Err(if available.is_empty() {
                "no accounts are configured under auth_modes.proxy in sudocode.json".to_string()
            } else {
                format!(
                    "no account named '{name}' under auth_modes.proxy (configured: {})",
                    available.join(", ")
                )
            });
        }

        // The one scope-aware config writer `/config set` uses, so the
        // selection lands in the project's `settings.local.json` exactly as it
        // would by hand.
        tools::set_config_setting("auth_profile", name)?;

        {
            let mut session = self.lock_session();
            let new_session = session.runtime.session().clone();
            let handle = session.handle.clone();
            self.rebuild_locked(&mut session, new_session, handle)?;
        }
        Ok(self.current_billing_account())
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
            .rt()
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

#[cfg(test)]
mod tests {
    use super::resume_model_from_config_ssot;

    #[test]
    fn resume_model_flag_wins_and_is_used_verbatim() {
        // An explicit --model flag (already alias-resolved by the caller) is
        // used as-is on resume, regardless of what the transcript recorded.
        let resolved = resume_model_from_config_ssot(Some("claude-opus-4-8"), "claude-opus-4-8");
        assert_eq!(resolved, "claude-opus-4-8");
    }

    #[test]
    fn resume_never_consults_the_session_pin() {
        // Structural guarantee: the resolver takes only the flag and the
        // config-resolved default — it has no parameter for the persisted
        // `session.model`, so a stale transcript model can never drive
        // selection. This test documents that contract at the type level; if
        // someone re-adds a `session.model` argument, it will fail to compile
        // against this call and force a review of the SSOT rule.
        let with_flag = resume_model_from_config_ssot(Some("m"), "m");
        assert_eq!(with_flag, "m");
    }
}
