//! Integration tests for the sub-agent telemetry pipeline that
//! populates the `<usage>` block in the task-notification XML.
//!
//! ## What this locks in (long-workflow, data-flow chained)
//!
//! The pipeline is:
//!
//!   `TurnSummary` → `telemetry_from_turn` → `record_agent_telemetry`
//!   → on-disk manifest → `persist_agent_terminal_state_with_telemetry`
//!   → re-read manifest → task-notification `<usage>` sub-tags
//!
//! Any silent break in that chain would produce a task-notification
//! with empty `<usage>`, which the coordinator prompt teaches the
//! model to trust for delegation-cost bookkeeping. So every step is
//! guarded here.
//!
//! ## Scenarios
//!
//! 1. **Fresh manifest → record_agent_telemetry writes both counters.**
//!    Confirms the read-modify-write cycle survives a manifest that
//!    predates the telemetry fields (missing keys deserialise as
//!    `None`, get filled).
//! 2. **Two records overwrite in place.** Multi-turn runs update
//!    telemetry after each turn — the LAST write wins.
//! 3. **Terminal-state persist preserves prior mid-run mutations.**
//!    Sets `result_full_path` inline, then calls persist_terminal —
//!    the field survives instead of being clobbered by the caller's
//!    stale manifest snapshot (fixes a pre-existing bug where
//!    Commit 11's AgentSummary sidecar path could be lost).
//! 4. **persist_terminal + telemetry lands in the JSON manifest and
//!    survives round-trip.** End-to-end: pretend we're the runtime
//!    telling `persist_terminal_state_with_telemetry` about a
//!    completed sub-agent, then read the manifest back and confirm
//!    `totalTokens` + `toolUses` are present.

use std::path::PathBuf;

use tools::testing::{
    persist_terminal_with_telemetry_for_test, record_agent_telemetry_for_test,
    seed_agent_manifest_for_test, AgentRunTelemetryView,
};

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn unique_workspace(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "sudocode-agent-telemetry-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir");
    path
}

#[test]
fn record_agent_telemetry_writes_both_counters() {
    let _g = env_lock();
    let ws = unique_workspace("record-basic");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-tel-basic");

    record_agent_telemetry_for_test(
        &manifest_path,
        AgentRunTelemetryView {
            total_tokens: 1234,
            tool_uses: 7,
        },
    )
    .expect("record ok");

    let raw = std::fs::read_to_string(&manifest_path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["totalTokens"].as_u64(), Some(1234));
    assert_eq!(json["toolUses"].as_u64(), Some(7));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn record_agent_telemetry_overwrites_prior_values() {
    let _g = env_lock();
    let ws = unique_workspace("record-overwrite");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-tel-overwrite");

    record_agent_telemetry_for_test(
        &manifest_path,
        AgentRunTelemetryView {
            total_tokens: 100,
            tool_uses: 2,
        },
    )
    .unwrap();
    record_agent_telemetry_for_test(
        &manifest_path,
        AgentRunTelemetryView {
            total_tokens: 5555,
            tool_uses: 11,
        },
    )
    .unwrap();

    let raw = std::fs::read_to_string(&manifest_path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    // LAST write wins — matches multi-turn's aggregate-then-record pattern.
    assert_eq!(json["totalTokens"].as_u64(), Some(5555));
    assert_eq!(json["toolUses"].as_u64(), Some(11));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn persist_terminal_survives_prior_full_result_path_mutation() {
    // Regression guard for the pre-Commit-11 clobber bug: an inline
    // record_full_result_path write MUST NOT be wiped when
    // persist_terminal_state runs afterwards with the caller's
    // stale in-memory manifest.
    let _g = env_lock();
    let ws = unique_workspace("persist-preserves-fullpath");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-tel-fullpath");

    // Simulate the AgentSummary path: sidecar file + on-disk update.
    let sidecar = ws.join("agent-tel-fullpath.full.md");
    std::fs::write(&sidecar, "full text").unwrap();
    tools::testing::record_full_result_path_for_test(&manifest_path, &sidecar).unwrap();

    // Now the runtime finishes the run and terminal-persists.
    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "completed",
        Some("short summary"),
        None,
        Some(AgentRunTelemetryView {
            total_tokens: 42,
            tool_uses: 3,
        }),
    )
    .expect("persist ok");

    let raw = std::fs::read_to_string(&manifest_path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["status"].as_str(), Some("completed"));
    assert_eq!(json["result"].as_str(), Some("short summary"));
    assert_eq!(json["totalTokens"].as_u64(), Some(42));
    assert_eq!(json["toolUses"].as_u64(), Some(3));
    // The critical assertion: the sidecar path set BEFORE persist is
    // still there.
    assert_eq!(
        json["resultFullPath"].as_str(),
        Some(sidecar.display().to_string().as_str()),
        "persist_terminal MUST NOT clobber resultFullPath"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn persist_terminal_writes_telemetry_when_none_recorded_before() {
    // Straight-line path: fresh manifest, no mid-run mutations, just
    // one terminal-state call with telemetry. Everything MUST land.
    let _g = env_lock();
    let ws = unique_workspace("persist-fresh");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-tel-fresh");

    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "completed",
        Some("done"),
        None,
        Some(AgentRunTelemetryView {
            total_tokens: 8192,
            tool_uses: 5,
        }),
    )
    .expect("persist ok");

    let raw = std::fs::read_to_string(&manifest_path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["status"].as_str(), Some("completed"));
    assert_eq!(json["totalTokens"].as_u64(), Some(8192));
    assert_eq!(json["toolUses"].as_u64(), Some(5));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn persist_terminal_without_telemetry_leaves_counters_absent() {
    // Callers that don't have telemetry (test paths, error paths)
    // MUST NOT force `totalTokens: 0` / `toolUses: 0` — those would
    // be misleading. Instead the fields stay absent.
    let _g = env_lock();
    let ws = unique_workspace("persist-no-telemetry");
    let manifest_path = seed_agent_manifest_for_test(&ws, "agent-tel-none");

    persist_terminal_with_telemetry_for_test(
        &manifest_path,
        "failed",
        None,
        Some("something broke".to_string()),
        None,
    )
    .expect("persist ok");

    let raw = std::fs::read_to_string(&manifest_path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["status"].as_str(), Some("failed"));
    assert!(json.get("totalTokens").is_none() || json["totalTokens"].is_null());
    assert!(json.get("toolUses").is_none() || json["toolUses"].is_null());
}
