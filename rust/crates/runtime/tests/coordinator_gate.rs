//! Integration tests for the coordinator hard tool-allowlist gate.
//!
//! Env var `SUDOCODE_COORDINATOR_MODE` gates the whole feature — when
//! off, `is_tool_allowed_in_coordinator_mode` is a pass-through (fast
//! path, zero cost per tool call). When on, only the tools returned by
//! `coordinator_allowed_tools` may be dispatched. Tests here lock in
//! that shape at the runtime crate level; the tools crate has its own
//! dispatch-guard regression test in `tools/tests/`.
//!
//! Tests are serialised via a process-wide mutex because
//! `SUDOCODE_COORDINATOR_MODE` is read from the OS env — parallel
//! tests writing distinct values would race each other's assertions.

use std::sync::{Mutex, MutexGuard, OnceLock};

use runtime::coordinator_mode::{
    coordinator_allowed_tools, is_coordinator_mode, is_tool_allowed_in_coordinator_mode,
    COORDINATOR_ENV_VAR,
};

/// Serialises tests that mutate `SUDOCODE_COORDINATOR_MODE`.
fn env_mutex() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// RAII helper that sets the env var and clears it on drop so tests
/// don't leak state to each other.
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

// ── coordinator_allowed_tools SSOT ───────────────────────────────

#[test]
fn allowlist_contains_delegation_surface() {
    let allowed = coordinator_allowed_tools();
    for name in [
        "Agent",
        "SendMessage",
        "TaskStop",
        "TaskGet",
        "TaskList",
        "TaskOutput",
    ] {
        assert!(
            allowed.contains(name),
            "coordinator allowlist MUST include delegation-surface tool `{name}`"
        );
    }
}

#[test]
fn allowlist_excludes_write_side_tools() {
    let allowed = coordinator_allowed_tools();
    // Every write-side tool that would let the coordinator itself
    // modify the workspace MUST be excluded — the whole point of
    // coordinator mode is to force delegation.
    for name in [
        "bash",
        "write_file",
        "edit_file",
        "PowerShell",
        "REPL",
        "NotebookEdit",
        "EnterPlanMode",
        "ExitPlanMode",
    ] {
        assert!(
            !allowed.contains(name),
            "write-side tool `{name}` MUST NOT be in coordinator allowlist"
        );
    }
}

#[test]
fn allowlist_includes_read_only_lookups() {
    let allowed = coordinator_allowed_tools();
    // Read-only tools stay so the coordinator can peek without
    // spawning a worker for trivial lookups.
    for name in [
        "read_file",
        "glob_search",
        "grep_search",
        "WebSearch",
        "WebFetch",
        "Skill",
    ] {
        assert!(
            allowed.contains(name),
            "read-only tool `{name}` MUST remain available in coordinator mode"
        );
    }
}

// ── is_tool_allowed_in_coordinator_mode (dispatch predicate) ─────

#[test]
fn predicate_returns_true_for_every_tool_when_env_off() {
    let _guard = env_mutex();
    let _clear = EnvGuard::clear(COORDINATOR_ENV_VAR);
    assert!(!is_coordinator_mode(), "sanity: env var must be off");

    // Even write-side tools return true — fast path is a
    // pass-through when coordinator mode is off.
    for name in [
        "bash",
        "write_file",
        "edit_file",
        "Agent",
        "PowerShell",
        "Sleep",
    ] {
        assert!(
            is_tool_allowed_in_coordinator_mode(name),
            "with coordinator mode off, every tool MUST pass through (got false for `{name}`)"
        );
    }
}

#[test]
fn predicate_blocks_write_tools_when_env_on() {
    let _guard = env_mutex();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");
    assert!(is_coordinator_mode(), "sanity: env var must be on");

    for name in [
        "bash",
        "write_file",
        "edit_file",
        "PowerShell",
        "REPL",
        "NotebookEdit",
    ] {
        assert!(
            !is_tool_allowed_in_coordinator_mode(name),
            "coordinator mode MUST block write-side tool `{name}` at dispatch"
        );
    }
}

#[test]
fn predicate_allows_delegation_tools_when_env_on() {
    let _guard = env_mutex();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");

    for name in [
        "Agent",
        "SendMessage",
        "TaskStop",
        "TaskGet",
        "TaskOutput",
        "read_file",
    ] {
        assert!(
            is_tool_allowed_in_coordinator_mode(name),
            "coordinator mode MUST allow delegation/read-only tool `{name}`"
        );
    }
}

#[test]
fn predicate_blocks_unknown_tool_when_env_on() {
    // Sanity: a completely made-up tool name is NOT allowed in
    // coordinator mode. If it were, a non-compliant model could
    // hallucinate write-side actions under bogus names.
    let _guard = env_mutex();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");

    assert!(!is_tool_allowed_in_coordinator_mode("MadeUpToolName"));
    assert!(!is_tool_allowed_in_coordinator_mode(""));
}

#[test]
fn env_off_returns_all_writes_allowed() {
    // Belt-and-suspenders — same as
    // `predicate_returns_true_for_every_tool_when_env_off` but
    // explicit about what "empty" and "false" values do (mirrors
    // CC-fork's `isEnvTruthy` treatment of these cases).
    let _guard = env_mutex();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "0");
    assert!(!is_coordinator_mode());
    assert!(is_tool_allowed_in_coordinator_mode("bash"));

    let _env2 = EnvGuard::set(COORDINATOR_ENV_VAR, "");
    assert!(!is_coordinator_mode());
    assert!(is_tool_allowed_in_coordinator_mode("bash"));

    let _env3 = EnvGuard::set(COORDINATOR_ENV_VAR, "false");
    assert!(!is_coordinator_mode());
    assert!(is_tool_allowed_in_coordinator_mode("bash"));
}
