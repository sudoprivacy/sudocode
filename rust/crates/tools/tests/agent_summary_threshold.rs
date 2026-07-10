//! Integration tests for the AgentSummary auto-summarize path.
//!
//! ## What this locks in (long-workflow, data-flow chained)
//!
//! 1. **Under-threshold path** — a short result passes through
//!    verbatim; no summarizer sub-turn spawns; no `.full.md`
//!    sibling gets written; manifest.result_full_path stays `None`.
//! 2. **Over-threshold path** — a result whose char-count exceeds
//!    the configured threshold triggers the sidecar-write step BEFORE
//!    the summarizer LLM call. Assertions on the sidecar file's
//!    contents + the on-disk manifest's `resultFullPath` field prove
//!    the pipeline plumbed the full text correctly, regardless of
//!    what the summarizer LLM returns.
//! 3. **Env-disable (`0`)** — even a 100 000-char result yields
//!    no sidecar and no summary attempt.
//! 4. **Env override (small threshold)** — a tiny threshold correctly
//!    fires summarization for medium-sized text.
//!
//! Live LLM is NOT required — the test seam `maybe_summarize_for_test`
//! stubs the summarizer with a placeholder string, exercising all
//! non-LLM steps (threshold decision, sidecar write, manifest update).

use std::path::PathBuf;

use tools::testing::maybe_summarize_for_test;

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
        "sudocode-agent-summary-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir");
    path
}

/// Write a minimal on-disk manifest for the fake agent so
/// `maybe_summarize_for_test` has something to read + update.
/// Returns (manifest_path, output_md_path).
fn seed_fake_agent(dir: &std::path::Path, agent_id: &str) -> (PathBuf, PathBuf) {
    let output_md = dir.join(format!("{agent_id}.md"));
    let manifest = dir.join(format!("{agent_id}.json"));
    std::fs::write(&output_md, "# Agent Task\n\n(prompt goes here)\n").expect("seed output_md");
    let manifest_json = serde_json::json!({
        "agentId": agent_id,
        "name": "test-agent",
        "description": "seed",
        "subagentType": "general-purpose",
        "model": "test-model",
        "status": "running",
        "outputFile": output_md.display().to_string(),
        "manifestFile": manifest.display().to_string(),
        "createdAt": "0",
        "derivedState": "working",
    });
    std::fs::write(
        &manifest,
        serde_json::to_string_pretty(&manifest_json).unwrap(),
    )
    .expect("seed manifest");
    (manifest, output_md)
}

#[test]
fn under_threshold_short_result_passes_through_verbatim() {
    let _g = env_lock();
    std::env::remove_var(tools::AGENT_SUMMARY_THRESHOLD_ENV);

    let ws = unique_workspace("under");
    let (manifest, output_md) = seed_fake_agent(&ws, "agent-under-threshold");
    let short_text = "short result — well below any reasonable threshold";

    let (parent, full_path) = maybe_summarize_for_test(&manifest, short_text, "SHOULD-NOT-BE-USED")
        .expect("gate returns Ok");

    assert_eq!(parent, short_text, "under-threshold text stays verbatim");
    assert!(full_path.is_none(), "no sidecar for under-threshold");

    // Sidecar file MUST NOT exist.
    let sidecar = output_md.with_extension("full.md");
    assert!(
        !sidecar.exists(),
        "no `.full.md` sidecar for under-threshold"
    );

    // Manifest MUST NOT gain a resultFullPath field.
    let raw = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        !raw.contains("resultFullPath"),
        "manifest resultFullPath stays absent under threshold"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn over_threshold_writes_full_sidecar_and_updates_manifest() {
    let _g = env_lock();
    std::env::remove_var(tools::AGENT_SUMMARY_THRESHOLD_ENV);

    let ws = unique_workspace("over");
    let (manifest, output_md) = seed_fake_agent(&ws, "agent-over-threshold");
    // 10 KB > 8 KB default threshold.
    let full_text: String = "L".repeat(10_000);
    let placeholder = "PLACEHOLDER_SUMMARY_QWERTY";

    let (parent, full_path) =
        maybe_summarize_for_test(&manifest, &full_text, placeholder).expect("gate returns Ok");

    assert_eq!(
        parent, placeholder,
        "over-threshold path MUST route through the summarizer"
    );
    let sidecar = full_path.expect("full_path returned");
    assert_eq!(
        sidecar,
        output_md.with_extension("full.md"),
        "sidecar path derived from output_file with .full.md extension"
    );

    // Sidecar file exists AND contains the full text verbatim.
    let sidecar_contents = std::fs::read_to_string(&sidecar).expect("sidecar readable");
    assert!(
        sidecar_contents.contains(&full_text),
        "sidecar MUST embed the FULL unabridged text"
    );
    assert!(
        sidecar_contents.contains("agent_id: agent-over-threshold"),
        "sidecar header carries the agent_id"
    );

    // Manifest resultFullPath field points at the sidecar.
    let raw = std::fs::read_to_string(&manifest).unwrap();
    let manifest_json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        manifest_json["resultFullPath"].as_str(),
        Some(sidecar.display().to_string().as_str())
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn env_zero_disables_summarization_entirely() {
    let _g = env_lock();
    std::env::set_var(tools::AGENT_SUMMARY_THRESHOLD_ENV, "0");

    let ws = unique_workspace("disabled");
    let (manifest, output_md) = seed_fake_agent(&ws, "agent-summary-off");
    let huge_text = "X".repeat(100_000);

    let (parent, full_path) =
        maybe_summarize_for_test(&manifest, &huge_text, "PLACEHOLDER").expect("gate returns Ok");

    assert_eq!(parent.len(), huge_text.len(), "disabled -> verbatim");
    assert!(full_path.is_none());
    assert!(!output_md.with_extension("full.md").exists());

    std::env::remove_var(tools::AGENT_SUMMARY_THRESHOLD_ENV);
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn small_env_threshold_triggers_summarization_for_medium_text() {
    let _g = env_lock();
    std::env::set_var(tools::AGENT_SUMMARY_THRESHOLD_ENV, "100");

    let ws = unique_workspace("tiny-threshold");
    let (manifest, output_md) = seed_fake_agent(&ws, "agent-tiny-threshold");
    // 500 chars > 100 threshold.
    let text: String = "M".repeat(500);
    let placeholder = "PLACEHOLDER";

    let (parent, full_path) =
        maybe_summarize_for_test(&manifest, &text, placeholder).expect("gate returns Ok");

    assert_eq!(parent, placeholder, "tiny threshold catches medium text");
    let sidecar = full_path.expect("full_path set");
    assert_eq!(sidecar, output_md.with_extension("full.md"));
    assert!(sidecar.exists());
    let sidecar_contents = std::fs::read_to_string(&sidecar).unwrap();
    assert!(sidecar_contents.contains(&text));

    std::env::remove_var(tools::AGENT_SUMMARY_THRESHOLD_ENV);
    let _ = std::fs::remove_dir_all(&ws);
}
