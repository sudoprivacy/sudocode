//! Integration tests for the `runtime::experiments` feature-flag
//! registry: env precedence (legacy > generic), default-off, settings
//! `experimental` parsing (unknown key / non-bool = load error), and
//! the registry naming invariants. The config-install branch lives in
//! `experiments_config_install.rs` — its process-global OnceLock must
//! not be populated in this binary or the default-off test races it.
//!
//! Tests are serialised via a process-wide mutex because flags read
//! the OS env live — parallel tests writing distinct values would
//! race each other's assertions.

use std::sync::{Mutex, MutexGuard, OnceLock};

use runtime::experiments::{is_enabled, Experiment};

fn env_mutex() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

struct EnvGuard(&'static str, Option<String>);
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self(key, prior)
    }
    fn clear(key: &'static str) -> Self {
        let prior = std::env::var(key).ok();
        std::env::remove_var(key);
        Self(key, prior)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

const LEGACY: &str = "SUDOCODE_COORDINATOR_MODE";
const GENERIC: &str = "SUDOCODE_EXPERIMENT_COORDINATOR_MODE";

#[test]
fn disabled_by_default_when_nothing_set() {
    let _guard = env_mutex();
    let _a = EnvGuard::clear(LEGACY);
    let _b = EnvGuard::clear(GENERIC);
    let _c = EnvGuard::clear("SUDOCODE_ENABLE_MCP");
    let _d = EnvGuard::clear("SUDOCODE_EXPERIMENT_MCP_CONFIG_SERVERS");
    // No test in this binary installs config flags (see
    // experiments_config_install.rs), so this exercises the true
    // nothing-set default.
    assert!(!is_enabled(Experiment::CoordinatorMode));
    assert!(!is_enabled(Experiment::McpConfigServers));
}

#[test]
fn generic_env_var_enables_and_explicit_falsy_disables() {
    let _guard = env_mutex();
    let _a = EnvGuard::clear(LEGACY);
    {
        let _on = EnvGuard::set(GENERIC, "1");
        assert!(is_enabled(Experiment::CoordinatorMode));
    }
    {
        let _off = EnvGuard::set(GENERIC, "0");
        assert!(!is_enabled(Experiment::CoordinatorMode));
    }
    {
        // Unrecognized values fall through to the next level (config,
        // which is either uninstalled or has no truthy entry here).
        let _junk = EnvGuard::set(GENERIC, "banana");
        assert!(!is_enabled(Experiment::CoordinatorMode));
    }
}

#[test]
fn legacy_env_var_wins_over_generic() {
    let _guard = env_mutex();
    let _legacy = EnvGuard::set(LEGACY, "0");
    let _generic = EnvGuard::set(GENERIC, "1");
    assert!(
        !is_enabled(Experiment::CoordinatorMode),
        "legacy env var is the highest-precedence source"
    );
}

// The config-install branch (`init_config_flags` with a truthy flag)
// lives in its own test binary, `experiments_config_install.rs`: the
// installed map is a process-global OnceLock, so a truthy install here
// would leak into `disabled_by_default_when_nothing_set` in whichever
// order the harness runs them.

// ── registry ↔ settings-schema mirror ─────────────────────────────

#[test]
fn every_experiment_key_is_unique_and_camel_case() {
    let mut seen = std::collections::BTreeSet::new();
    for experiment in Experiment::ALL {
        let key = experiment.key();
        assert!(seen.insert(key), "duplicate experiment key: {key}");
        assert!(
            key.chars().next().is_some_and(char::is_lowercase)
                && key.chars().all(char::is_alphanumeric),
            "experiment key must be camelCase like other settings keys: {key}"
        );
        assert!(
            experiment
                .env_suffix()
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_'),
            "env suffix must be UPPER_SNAKE: {}",
            experiment.env_suffix()
        );
    }
}

// ── settings `experimental` section parsing ───────────────────────

/// Load a config whose project-scope settings.json is `settings_json`,
/// with `SUDO_CODE_CONFIG_HOME` pointed at the same temp dir so the
/// developer's real user-scope settings can't leak into the merge.
/// Callers must hold `env_mutex()`.
fn load_config_with_settings(settings_json: &str) -> Result<runtime::RuntimeConfig, String> {
    let dir = std::env::temp_dir().join(format!(
        "scode-experiments-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let project_settings = dir.join(".nexus").join("sudocode");
    std::fs::create_dir_all(&project_settings).expect("create project settings dir");
    std::fs::write(project_settings.join("settings.json"), settings_json)
        .expect("write settings.json");
    let config_home = dir.join("config-home");
    std::fs::create_dir_all(&config_home).expect("create config home");
    let _home = EnvGuard::set(
        "SUDO_CODE_CONFIG_HOME",
        config_home.to_str().expect("utf-8 temp path"),
    );
    let result = runtime::ConfigLoader::default_for(&dir)
        .load()
        .map_err(|error| error.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[test]
fn settings_experimental_section_parses_known_flags() {
    let _guard = env_mutex();
    let config = load_config_with_settings(r#"{"experimental": {"coordinatorMode": true}}"#)
        .expect("known flag should load");
    assert_eq!(
        config.experiments().get("coordinatorMode"),
        Some(&true),
        "parsed experimental section should surface the flag"
    );
}

#[test]
fn settings_experimental_unknown_key_fails_loud() {
    let _guard = env_mutex();
    let error = load_config_with_settings(r#"{"experimental": {"coordinatorMoed": true}}"#)
        .expect_err("typo'd flag must be a load error, not silently off");
    assert!(
        error.contains("coordinatorMoed"),
        "error should name the offending key: {error}"
    );
}

#[test]
fn settings_experimental_non_bool_value_fails_loud() {
    let _guard = env_mutex();
    let error = load_config_with_settings(r#"{"experimental": {"coordinatorMode": "yes"}}"#)
        .expect_err("non-bool flag value must be a load error");
    assert!(
        error.contains("boolean"),
        "error should say a boolean is required: {error}"
    );
}
