//! Config / model / permission resolution — the engine-side SSOT.
//!
//! These helpers resolve *what the engine will do* from the environment, the
//! `.scode.json` / `.nexus/sudocode/settings.json` config, and the compiled-in
//! defaults: which model to talk to (alias expansion + provenance) and which
//! permission mode to enforce. They live below the seam because the answer is an
//! engine input, not a rendering concern — both the REPL and `engine-acp` build a
//! session from them. Renderer crates re-import from here (`engine_host::config`)
//! rather than owning a second copy.

use std::collections::BTreeSet;
use std::env;
use std::path::Path;

use runtime::{ConfigLoader, PermissionMode, ResolvedPermissionMode};

pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

/// #148: Model provenance for `scode status` JSON/text output. Records where
/// the resolved model string came from so consumers don't have to re-read argv
/// to audit whether their `--model` flag was honored vs falling back to env
/// or config or default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSource {
    /// Explicit `--model` / `--model=` CLI flag.
    Flag,
    /// ANTHROPIC_MODEL environment variable (when no flag was passed).
    Env,
    /// `model` key in `.scode.json` / `.nexus/sudocode/settings.json` (when neither
    /// flag nor env set it).
    Config,
    /// Compiled-in DEFAULT_MODEL fallback.
    Default,
}

impl ModelSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelSource::Flag => "flag",
            ModelSource::Env => "env",
            ModelSource::Config => "config",
            ModelSource::Default => "default",
        }
    }
}

/// Single source of truth for the env-or-config default model lookup. Returns
/// `(resolved, raw, source)` when env or config wins, `None` to defer to the
/// compiled-in default.
pub fn lookup_default_model() -> Option<(String, String, ModelSource)> {
    if let Some(env_model) = env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Some((
            resolve_model_alias_with_config(&env_model),
            env_model,
            ModelSource::Env,
        ));
    }
    if let Some(config_model) = config_model_for_current_dir() {
        return Some((
            resolve_model_alias_with_config(&config_model),
            config_model,
            ModelSource::Config,
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Model / alias resolution
// ---------------------------------------------------------------------------

pub fn resolve_model_alias(model: &str) -> &str {
    match model {
        "auto" | "claude-sonnet" | "sonnet" => "claude-sonnet-4-6",
        "claude-opus" | "opus" => "claude-opus-4-6",
        "claude-haiku" | "haiku" => "claude-haiku-4-5-20251213",
        _ => model,
    }
}

pub fn resolve_model_alias_with_config(model: &str) -> String {
    let trimmed = model.trim();
    let config = load_sudocode_config_for_current_dir();
    if let Some(alias) = resolve_config_model_alias(trimmed, &config) {
        return alias;
    }
    if let Some(resolved) = config_alias_for_current_dir(trimmed) {
        return resolve_model_alias(&resolved).to_string();
    }
    resolve_model_alias(trimmed).to_string()
}

pub fn resolve_config_model_alias(
    model: &str,
    config: &engine_core::SudoCodeConfig,
) -> Option<String> {
    let trimmed = model.trim();
    let entry = config.models.get(&trimmed.to_ascii_lowercase())?;
    if entry.alias.trim().is_empty() {
        Some(trimmed.to_string())
    } else {
        Some(entry.alias.clone())
    }
}

fn config_alias_for_current_dir(alias: &str) -> Option<String> {
    if alias.is_empty() {
        return None;
    }
    let cwd = runtime::current_workspace_root().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    let config = loader.load().ok()?;
    config.aliases().get(alias).cloned()
}

// ---------------------------------------------------------------------------
// Config loaders
// ---------------------------------------------------------------------------

pub fn load_sudocode_config_for_current_dir() -> engine_core::SudoCodeConfig {
    let Ok(cwd) = runtime::current_workspace_root() else {
        return engine_core::SudoCodeConfig::default();
    };
    load_sudocode_config_for_cwd(&cwd)
}

pub fn load_sudocode_config_for_cwd(cwd: &Path) -> engine_core::SudoCodeConfig {
    let loader = ConfigLoader::default_for(cwd);
    let config = loader.load_sudocode_config().unwrap_or_default();
    runtime::model_capabilities::apply_config_limits(&config);
    config
}

pub fn require_sudocode_config_for_cwd(cwd: &Path) -> Result<engine_core::SudoCodeConfig, String> {
    let loader = ConfigLoader::default_for(cwd);
    let config = loader.load_sudocode_config().map_err(|e| e.to_string())?;
    // Seed the capability overrides here rather than at each startup site:
    // every entry point (REPL, --print, acp, subagents) reaches the config
    // through this pair, and a forgotten seeding call is invisible until a
    // provider rejects `max_tokens` at request time.
    runtime::model_capabilities::apply_config_limits(&config);
    Ok(config)
}

// ---------------------------------------------------------------------------
// Allowed tools
// ---------------------------------------------------------------------------

pub type AllowedToolSet = BTreeSet<String>;

// ---------------------------------------------------------------------------
// Permission mode helpers
// ---------------------------------------------------------------------------

pub fn permission_mode_from_label(mode: &str) -> PermissionMode {
    match mode {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

fn permission_mode_from_resolved(mode: ResolvedPermissionMode) -> PermissionMode {
    match mode {
        ResolvedPermissionMode::ReadOnly => PermissionMode::ReadOnly,
        ResolvedPermissionMode::WorkspaceWrite => PermissionMode::WorkspaceWrite,
        ResolvedPermissionMode::DangerFullAccess => PermissionMode::DangerFullAccess,
    }
}

pub fn default_permission_mode() -> PermissionMode {
    env::var("SUDO_CODE_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(normalize_permission_mode)
        .map(permission_mode_from_label)
        .or_else(config_permission_mode_for_current_dir)
        .unwrap_or(PermissionMode::DangerFullAccess)
}

fn config_permission_mode_for_current_dir() -> Option<PermissionMode> {
    let cwd = runtime::current_workspace_root().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader
        .load()
        .ok()?
        .permission_mode()
        .map(permission_mode_from_resolved)
}

pub fn config_model_for_current_dir() -> Option<String> {
    let cwd = runtime::current_workspace_root().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader.load().ok()?.model().map(ToOwned::to_owned)
}

pub fn resolve_repl_model(cli_model: String) -> String {
    if cli_model != DEFAULT_MODEL {
        return cli_model;
    }
    lookup_default_model()
        .map(|(resolved, _, _)| resolved)
        .unwrap_or(cli_model)
}

/// Canonicalize a user-supplied permission-mode label to its wire form, or
/// `None` if it is not one of the three supported modes.
pub fn normalize_permission_mode(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        "read-only" => Some("read-only"),
        "workspace-write" => Some("workspace-write"),
        "danger-full-access" => Some("danger-full-access"),
        _ => None,
    }
}
