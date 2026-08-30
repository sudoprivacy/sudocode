//! The config-sourced branch of `runtime::experiments`, isolated in
//! its own test binary: `init_config_flags` fills a process-global
//! OnceLock, so installing a truthy flag would leak into the
//! default-off assertions in `experiments_flags.rs` if they shared a
//! process. Here the install is the only test, so first-call-wins is
//! guaranteed to be THIS call.

use std::collections::BTreeMap;

use runtime::experiments::{init_config_flags, is_enabled, Experiment};

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

#[test]
fn config_flags_apply_when_env_is_silent_and_env_still_overrides() {
    let _a = EnvGuard::clear("SUDOCODE_ENABLE_MCP");
    let _b = EnvGuard::clear("SUDOCODE_EXPERIMENT_MCP_CONFIG_SERVERS");
    let _c = EnvGuard::clear("SUDOCODE_COORDINATOR_MODE");
    let _d = EnvGuard::clear("SUDOCODE_EXPERIMENT_COORDINATOR_MODE");

    let installed = init_config_flags(BTreeMap::from([(String::from("mcpConfigServers"), true)]));
    assert!(installed, "only test in this binary — first call must win");

    // Config truthy enables when env is silent.
    assert!(is_enabled(Experiment::McpConfigServers));
    // A flag absent from the installed map stays off.
    assert!(!is_enabled(Experiment::CoordinatorMode));
    // Env falsy beats config truthy.
    let _off = EnvGuard::set("SUDOCODE_ENABLE_MCP", "0");
    assert!(!is_enabled(Experiment::McpConfigServers));
}
