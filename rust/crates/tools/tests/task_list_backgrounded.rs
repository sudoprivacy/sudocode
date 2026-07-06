//! Integration tests for `TaskList` reporting live sub-agents.
//!
//! Plan §4.6 Commit 15: instead of a ratatui-based Background Agent
//! Selector (forbidden by memory `no_alternate_screen_tui_sudocode`),
//! we downgrade to exposing the same data via the existing
//! `TaskList` LLM tool. Coordinators can then enumerate live
//! background sub-agents via `TaskList(backgrounded_only=true)` and
//! pick one to continue via SendMessage / TaskOutput.
//!
//! ## What this locks in (long-workflow, data-flow chained)
//!
//! Sets up a fake agent store with 3 sub-agent manifests in
//! different statuses:
//!   - `agent-alpha`   → status "running"
//!   - `agent-beta`    → status "backgrounded"
//!   - `agent-gamma`   → status "completed"
//!
//! Then verifies:
//!
//! 1. `TaskList()` (no filter) returns ALL 3 under `background_agents`
//!    plus a `background_agent_count == 3`.
//! 2. `TaskList(backgrounded_only=true)` returns exactly
//!    `agent-alpha` and `agent-beta` — the running + backgrounded
//!    pair — and drops the completed one.
//! 3. Each snapshot carries `agent_id`, `status`, `color`,
//!    `subagent_type`, and `created_at` — the fields a switcher UI
//!    needs to render its labels.
//! 4. Order is created_at DESC — the most recent spawn comes first,
//!    matching the "you probably want the one you just launched"
//!    heuristic.

use std::path::PathBuf;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn unique_store(label: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "sudocode-tasklist-bg-{label}-{nanos}-{}",
        std::process::id()
    ))
}

fn seed_manifest(dir: &std::path::Path, agent_id: &str, status: &str, created_at: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    let path = dir.join(format!("{agent_id}.json"));
    let value = serde_json::json!({
        "agentId": agent_id,
        "name": format!("test-{agent_id}"),
        "description": format!("desc for {agent_id}"),
        "subagentType": "general-purpose",
        "model": "test-model",
        "status": status,
        "outputFile": format!("{}.md", dir.join(agent_id).display()),
        "manifestFile": path.display().to_string(),
        "createdAt": created_at,
        "derivedState": "working",
        "color": "cyan",
    });
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).expect("seed");
}

#[test]
fn tasklist_no_filter_returns_every_backgrounded_agent() {
    let _g = env_lock();
    let store = unique_store("no-filter");
    seed_manifest(&store, "agent-alpha", "running", "3000");
    seed_manifest(&store, "agent-beta", "backgrounded", "2000");
    seed_manifest(&store, "agent-gamma", "completed", "1000");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    let out = tools::execute_tool("TaskList", &serde_json::json!({})).expect("TaskList Ok");
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid json");

    let agents = json["background_agents"].as_array().expect("array");
    assert_eq!(json["background_agent_count"].as_u64(), Some(3));
    let ids: Vec<&str> = agents
        .iter()
        .map(|a| a["agent_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"agent-alpha"));
    assert!(ids.contains(&"agent-beta"));
    assert!(ids.contains(&"agent-gamma"));

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn tasklist_backgrounded_only_narrows_to_running_and_backgrounded() {
    let _g = env_lock();
    let store = unique_store("bg-only");
    seed_manifest(&store, "agent-alpha", "running", "3000");
    seed_manifest(&store, "agent-beta", "backgrounded", "2000");
    seed_manifest(&store, "agent-gamma", "completed", "1000");
    seed_manifest(&store, "agent-delta", "failed", "500");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    let out = tools::execute_tool(
        "TaskList",
        &serde_json::json!({ "backgrounded_only": true }),
    )
    .expect("TaskList Ok");
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid json");
    let agents = json["background_agents"].as_array().expect("array");

    assert_eq!(
        json["background_agent_count"].as_u64(),
        Some(2),
        "backgrounded_only must drop completed+failed"
    );
    let ids: Vec<&str> = agents
        .iter()
        .map(|a| a["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["agent-alpha", "agent-beta"]);

    // Order proof: newest first (created_at 3000 > 2000).
    assert_eq!(agents[0]["agent_id"].as_str(), Some("agent-alpha"));

    // Snapshot shape carries the fields a switcher UI needs.
    let alpha = &agents[0];
    assert_eq!(alpha["status"].as_str(), Some("running"));
    assert_eq!(alpha["subagent_type"].as_str(), Some("general-purpose"));
    assert_eq!(alpha["color"].as_str(), Some("cyan"));
    assert_eq!(alpha["created_at"].as_str(), Some("3000"));

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn tasklist_survives_missing_store_directory() {
    // Fresh session that hasn't spawned an agent yet -> store dir
    // may not exist. TaskList must still Ok with an empty list, not
    // error out.
    let _g = env_lock();
    let store = unique_store("empty");
    // Deliberately do NOT create the directory.
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    let out = tools::execute_tool("TaskList", &serde_json::json!({})).expect("TaskList Ok");
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid json");
    assert_eq!(json["background_agent_count"].as_u64(), Some(0));
    let agents = json["background_agents"].as_array().expect("array");
    assert!(agents.is_empty());

    std::env::remove_var("SUDOCODE_AGENT_STORE");
}

#[test]
fn tasklist_survives_corrupt_manifest_file() {
    let _g = env_lock();
    let store = unique_store("corrupt");
    seed_manifest(&store, "agent-good", "running", "3000");
    // A corrupt file next to a good one MUST NOT wipe the good list.
    std::fs::write(store.join("agent-broken.json"), "this is not JSON at all")
        .expect("seed corrupt");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    let out = tools::execute_tool("TaskList", &serde_json::json!({})).expect("TaskList Ok");
    let json: serde_json::Value = serde_json::from_str(&out).expect("valid json");
    let agents = json["background_agents"].as_array().expect("array");
    let ids: Vec<&str> = agents
        .iter()
        .map(|a| a["agent_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["agent-good"]);

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn tasklist_schema_advertises_backgrounded_only_field() {
    let specs = tools::mvp_tool_specs();
    let task_list = specs
        .into_iter()
        .find(|s| s.name == "TaskList")
        .expect("TaskList exists");
    let bg = &task_list.input_schema["properties"]["backgrounded_only"];
    assert_eq!(bg["type"].as_str(), Some("boolean"));
}
