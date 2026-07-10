//! Integration test for the coordinator dispatch-side guard in
//! `execute_tool_with_enforcer` — the "belt" that fires even if a
//! non-compliant LLM hallucinates a forbidden tool name past the
//! schema-side filter in `GlobalToolRegistry::definitions`.
//!
//! Together with `runtime/tests/coordinator_gate.rs` this locks in
//! both layers of coordinator mode's tool restriction.

use serde_json::json;

use runtime::coordinator_mode::COORDINATOR_ENV_VAR;
use tools::GlobalToolRegistry;

/// Serialise env-var-mutating tests — same reason as
/// `runtime/tests/coordinator_gate.rs`.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
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

// ── Dispatch guard fires on write tools when coord mode is on ────

#[test]
fn dispatch_rejects_bash_when_coordinator_mode_is_on() {
    let _guard = env_lock();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");

    let registry = GlobalToolRegistry::builtin();
    let input = json!({ "command": "echo hello" });
    let result = registry.execute("bash", &input);

    let err = result.expect_err("bash must be rejected by the coordinator dispatch guard");
    assert!(
        err.contains("not available in coordinator mode"),
        "error must name the coordinator gate (got: `{err}`)"
    );
    assert!(
        err.contains("Agent(") || err.contains("SendMessage"),
        "error must instruct the model to delegate via Agent (got: `{err}`)"
    );
}

#[test]
fn dispatch_rejects_write_file_when_coordinator_mode_is_on() {
    let _guard = env_lock();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");

    let registry = GlobalToolRegistry::builtin();
    let input = json!({ "path": "/tmp/anything.txt", "content": "x" });
    let result = registry.execute("write_file", &input);

    let err = result.expect_err("write_file must be rejected in coordinator mode");
    assert!(err.contains("not available in coordinator mode"));
}

// ── Dispatch guard is a no-op when coord mode is off ─────────────

#[test]
fn dispatch_allows_bash_when_coordinator_mode_is_off() {
    let _guard = env_lock();
    let _env = EnvGuard::clear(COORDINATOR_ENV_VAR);

    let registry = GlobalToolRegistry::builtin();
    let input = json!({ "command": "true" });
    // We don't care about bash's own result here — just that the
    // coordinator gate does NOT reject before the tool runs.
    // Any Err that comes back must NOT be the coordinator error.
    match registry.execute("bash", &input) {
        Ok(_) => {}
        Err(e) => {
            assert!(
                !e.contains("not available in coordinator mode"),
                "coordinator gate must be a pass-through when env is off (unexpected error: {e})"
            );
        }
    }
}

// ── Schema-side filter mirrors the dispatch guard ────────────────

#[test]
fn definitions_hides_write_tools_when_coordinator_mode_is_on() {
    let _guard = env_lock();
    let _env = EnvGuard::set(COORDINATOR_ENV_VAR, "1");

    let registry = GlobalToolRegistry::builtin();
    let defs = registry.definitions(None);
    let names: std::collections::HashSet<_> = defs.iter().map(|d| d.name.as_str()).collect();

    for hidden in [
        "bash",
        "write_file",
        "edit_file",
        "PowerShell",
        "REPL",
        "NotebookEdit",
    ] {
        assert!(
            !names.contains(hidden),
            "`{hidden}` MUST be hidden from LLM schema when coordinator mode is on"
        );
    }
    for shown in ["Agent", "SendMessage", "TaskStop", "read_file"] {
        assert!(
            names.contains(shown),
            "`{shown}` MUST remain visible in coordinator mode"
        );
    }
}

#[test]
fn definitions_shows_write_tools_when_coordinator_mode_is_off() {
    let _guard = env_lock();
    let _env = EnvGuard::clear(COORDINATOR_ENV_VAR);

    let registry = GlobalToolRegistry::builtin();
    let defs = registry.definitions(None);
    let names: std::collections::HashSet<_> = defs.iter().map(|d| d.name.as_str()).collect();

    for shown in ["bash", "write_file", "edit_file", "Agent"] {
        assert!(
            names.contains(shown),
            "with coordinator mode off, `{shown}` MUST be present in tool schema"
        );
    }
}
