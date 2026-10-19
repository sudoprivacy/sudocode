//! Experimental feature flags — the standing pattern for gating
//! in-progress features.
//!
//! ## Standing rule
//!
//! Every experimental feature ships behind a flag registered in
//! [`Experiment`], OFF by default. A feature graduates by deleting its
//! flag (and the dead branch), not by flipping the default. See
//! CONTRIBUTING.md § "Experimental features".
//!
//! ## Resolution precedence (highest wins)
//!
//! 1. **Legacy dedicated env var** (back-compat for flags that predate
//!    this module, e.g. `SUDOCODE_COORDINATOR_MODE`).
//! 2. **Generic env var** — `SUDOCODE_EXPERIMENT_<UPPER_SNAKE>`:
//!    truthy (`1`/`true`/`on`/`yes`) enables, falsy
//!    (`0`/`false`/`off`/`no`) force-disables even when config enables.
//! 3. **Settings** — `"experimental": {"<camelCaseKey>": true}` in the
//!    deep-merged `settings.json` scopes, surfaced to this module via
//!    [`init_config_flags`] at process startup.
//! 4. Default: **disabled**.
//!
//! Env vars are read live on every check (never cached) so PTY/e2e
//! harnesses and `EnvGuard`-style tests can toggle them per spawn.
//! Config flags are process-global and fixed at startup.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Registry of every experimental feature flag. Adding an experiment
/// means adding a variant here plus its entry in [`Experiment::ALL`],
/// [`Experiment::key`], and [`Experiment::env_suffix`] — the
/// `config_schema` EXPERIMENTAL_CHILDREN table mirrors `key()` so
/// unknown keys in `settings.json` warn at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Experiment {
    /// Coordinator mode: role-prompt swap + hard tool allowlist so the
    /// main agent orchestrates workers instead of editing directly.
    CoordinatorMode,
    /// Spawn config/plugin-defined MCP servers and advertise their
    /// `mcp__*` tools to the model. (ACP session-injected MCP servers
    /// are an explicit per-session request and are NOT gated by this.)
    McpConfigServers,
}

impl Experiment {
    pub const ALL: &'static [Experiment] =
        &[Experiment::CoordinatorMode, Experiment::McpConfigServers];

    /// Key under `"experimental"` in `settings.json` (camelCase, like
    /// every other settings key).
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Experiment::CoordinatorMode => "coordinatorMode",
            Experiment::McpConfigServers => "mcpConfigServers",
        }
    }

    /// Suffix of the generic `SUDOCODE_EXPERIMENT_*` env var.
    #[must_use]
    pub fn env_suffix(self) -> &'static str {
        match self {
            Experiment::CoordinatorMode => "COORDINATOR_MODE",
            Experiment::McpConfigServers => "MCP_CONFIG_SERVERS",
        }
    }

    /// Pre-existing dedicated env var honored for back-compat (highest
    /// precedence). New experiments must NOT add one — the generic
    /// `SUDOCODE_EXPERIMENT_*` form is enough.
    #[must_use]
    pub fn legacy_env(self) -> Option<&'static str> {
        match self {
            Experiment::CoordinatorMode => Some("SUDOCODE_COORDINATOR_MODE"),
            Experiment::McpConfigServers => Some("SUDOCODE_ENABLE_MCP"),
        }
    }
}

/// Config-sourced flag values (`experimental` section of the merged
/// settings), installed once at process startup by the CLI.
static CONFIG_FLAGS: OnceLock<BTreeMap<String, bool>> = OnceLock::new();

/// Install the `experimental` settings section. First call wins; later
/// calls (e.g. per-ACP-session runtime rebuilds against the same config
/// files) are no-ops. Returns whether this call installed the flags.
pub fn init_config_flags(flags: BTreeMap<String, bool>) -> bool {
    CONFIG_FLAGS.set(flags).is_ok()
}

/// Whether `experiment` is enabled, per the precedence order in the
/// module docs.
#[must_use]
pub fn is_enabled(experiment: Experiment) -> bool {
    if let Some(var) = experiment.legacy_env() {
        if let Some(enabled) = env_flag(var) {
            return enabled;
        }
    }
    let generic = format!("SUDOCODE_EXPERIMENT_{}", experiment.env_suffix());
    if let Some(enabled) = env_flag(&generic) {
        return enabled;
    }
    CONFIG_FLAGS
        .get()
        .and_then(|flags| flags.get(experiment.key()).copied())
        .unwrap_or(false)
}

/// Parse an env var as a tri-state flag: `Some(true)` for truthy,
/// `Some(false)` for explicit falsy, `None` when unset/unrecognized
/// (falls through to the next precedence level).
fn env_flag(var: &str) -> Option<bool> {
    let value = std::env::var(var).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}
