//! Wire-schema lock for `TaskOutput`'s JSON projection of an
//! `AgentOutput` manifest.
//!
//! The coordinator prompt + our PTY tests (`pty_agent_summary` in
//! particular) train the LLM to look for specific snake_case field
//! names in the TaskOutput response. Any drift in the wire shape
//! silently breaks that contract — the model can't find fields the
//! coordinator prompt tells it exist, and downstream tests fail
//! obliquely with "field not found" chains.
//!
//! This file locks in:
//!
//! 1. **snake_case naming** — LLMs are prompt-trained on it.
//! 2. **Base fields always present** — `agent_id`, `status`,
//!    `retrieval_status`, `output_file`, `manifest_file`.
//! 3. **Optionals absent when unset** — no `null` clutter for the
//!    coordinator to burn tokens dissecting.
//! 4. **Optionals present when set** — this is exactly the bug that
//!    slipped through 4 commits before session 4's live-PTY run
//!    caught it: `result_full_path` stored on disk but not surfaced
//!    to TaskOutput. Regression guard.
//!
//! The compile-time guard against forgotten fields lives in
//! `build_agent_output_view`'s exhaustive destructure; this file
//! guards the runtime wire.

use tools::testing::{
    persist_terminal_with_telemetry_for_test, record_full_result_path_for_test,
    seed_agent_manifest_for_test, AgentRunTelemetryView,
};

fn unique_workspace(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "sudocode-wire-schema-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir");
    path
}

fn task_output_for(agent_id: &str) -> serde_json::Value {
    let out =
        tools::execute_tool("TaskOutput", &serde_json::json!({ "agent_id": agent_id })).unwrap();
    serde_json::from_str(&out).expect("valid json")
}

#[test]
fn wire_schema_has_snake_case_base_fields_after_terminal_state() {
    let ws = unique_workspace("base");
    let store_dir = ws.join("store");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store_dir);

    let manifest_path = seed_agent_manifest_for_test(&store_dir, "agent-base");
    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "completed",
        Some("all good"),
        None,
        None,
    )
    .expect("persist ok");

    let value = task_output_for("agent-base");
    // Base fields MUST use snake_case — the coordinator prompt is
    // trained on it. A rename to camelCase would silently break the
    // model's field lookups.
    for key in [
        "agent_id",
        "status",
        "retrieval_status",
        "output_file",
        "manifest_file",
    ] {
        assert!(
            value.get(key).is_some(),
            "TaskOutput wire schema MUST expose base field `{key}` in snake_case"
        );
    }
    // Explicitly reject camelCase — a serde annotation drift would
    // silently ship both, doubling every response's size.
    assert!(value.get("agentId").is_none(), "no camelCase in TaskOutput");
    assert!(
        value.get("outputFile").is_none(),
        "no camelCase in TaskOutput"
    );

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn wire_schema_omits_optional_fields_when_none() {
    let ws = unique_workspace("optional-absent");
    let store_dir = ws.join("store");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store_dir);

    let manifest_path = seed_agent_manifest_for_test(&store_dir, "agent-min");
    persist_terminal_with_telemetry_for_test(&manifest_path, "completed", Some("ok"), None, None)
        .expect("persist ok");

    let value = task_output_for("agent-min");
    for key in [
        "result_full_path",
        "color",
        "total_tokens",
        "tool_uses",
        "error",
    ] {
        assert!(
            value.get(key).is_none(),
            "unset optional `{key}` MUST NOT appear in the response — got: {value}"
        );
    }

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn wire_schema_surfaces_result_full_path_when_summarizer_recorded_it() {
    // This is the regression that took 4 commits + a live PTY run
    // to catch: manifest.result_full_path was populated on disk,
    // but agent_output_json's hard-coded field list didn't include
    // it. Locking down the fix here so it can never regress.
    let ws = unique_workspace("resultfullpath");
    let store_dir = ws.join("store");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store_dir);

    let manifest_path = seed_agent_manifest_for_test(&store_dir, "agent-summarized");
    // AgentSummary would write a sidecar; simulate.
    let sidecar = store_dir.join("agent-summarized.full.md");
    std::fs::write(&sidecar, "long full text").unwrap();
    record_full_result_path_for_test(&manifest_path, &sidecar).expect("record path");

    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "completed",
        Some("short summary"),
        None,
        Some(AgentRunTelemetryView {
            total_tokens: 9999,
            tool_uses: 42,
        }),
    )
    .expect("persist ok");

    let value = task_output_for("agent-summarized");
    assert_eq!(
        value["result_full_path"].as_str(),
        Some(sidecar.display().to_string().as_str()),
        "result_full_path MUST surface in TaskOutput"
    );
    assert_eq!(
        value["total_tokens"].as_u64(),
        Some(9999),
        "total_tokens MUST surface"
    );
    assert_eq!(
        value["tool_uses"].as_u64(),
        Some(42),
        "tool_uses MUST surface"
    );
    assert_eq!(
        value["result"].as_str(),
        Some("short summary"),
        "result is the parent-facing text (summary if summarized)"
    );

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&ws);
}
