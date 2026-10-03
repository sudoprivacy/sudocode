//! `send`'s dead-pid guard (design D1: a dead pid errors, no fallback).
//!
//! A `send` to an exited worker must error rather than silently provisioning a
//! phantom inbox and reporting success — the "reported delivered but never
//! arrived" failure a2a exists to prevent. A plain agent-name, by contrast, is
//! offline-capable and must pass the guard: its inbox fills until it next runs.
//!
//! Exercises the guard directly (via a test seam) so it is hermetic and does not
//! perform a real send. Own process so `SUDOCODE_AGENT_STORE` isolation cannot
//! race another test in the same binary.

use tools::testing::reject_dead_pid_for_test;

fn temp_store(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("send-dead-pid-{label}-{nanos}"))
}

/// A terminal manifest as `run_agent_job_returning_text` persists it — the
/// camelCase keys `AgentOutput` deserialises, `status: completed` = terminal.
fn write_terminal_manifest(store: &std::path::Path, agent_id: &str) {
    std::fs::create_dir_all(store).unwrap();
    let manifest = serde_json::json!({
        "agentId": agent_id,
        "name": agent_id,
        "description": "a finished worker",
        "status": "completed",
        "outputFile": store.join(format!("{agent_id}.md")).to_string_lossy(),
        "manifestFile": store.join(format!("{agent_id}.json")).to_string_lossy(),
        "createdAt": "2026-09-25T00:00:00Z",
        "derivedState": "completed",
    });
    std::fs::write(
        store.join(format!("{agent_id}.json")),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn dead_pid_is_rejected_but_unknown_name_passes() {
    let store = temp_store("guard");
    write_terminal_manifest(&store, "agent-9001");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    // A known-terminal pid → refused (its message could never be read).
    let err =
        reject_dead_pid_for_test("agent-9001").expect_err("a known-terminal pid must be rejected");
    assert!(
        err.contains("exited") || err.contains("dead"),
        "error should explain the pid is dead: {err}"
    );

    // A plain agent-name with no manifest is not a dead pid — offline-capable,
    // must pass so its durable inbox can fill until it next runs.
    assert!(
        reject_dead_pid_for_test("some-teammate").is_ok(),
        "an agent-name must pass the dead-pid guard"
    );

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&store);
}
