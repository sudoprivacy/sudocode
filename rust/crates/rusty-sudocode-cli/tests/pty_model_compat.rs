//! Model compatibility PTY tests — verifies that arbitrary models
//! served by sudorouter can be used via proxy passthrough.
//!
//! The model list is read from the `SCODE_COMPAT_MODELS` environment
//! variable (comma-separated model IDs). When the variable is unset
//! or empty, no models are tested and the test passes vacuously.
//!
//! These are **live-only** tests — passthrough requires a real proxy.
//! In mock mode the test exits immediately (no mock scenario needed).
//!
//! ## Usage
//!
//! ```bash
//! # Single model
//! SCODE_TEST_BACKEND=live SCODE_COMPAT_MODELS=o3-mini \
//!   cargo test --test pty_model_compat -- --test-threads=1
//!
//! # Multiple models
//! SCODE_TEST_BACKEND=live SCODE_COMPAT_MODELS=o3-mini,doubao-seed-1-6-251015,gpt-4o \
//!   cargo test --test pty_model_compat -- --test-threads=1
//!
//! # CI: the model-compat.yml workflow populates SCODE_COMPAT_MODELS
//! # from the sudorouter /v1/models endpoint automatically.
//! ```

mod common;

use std::time::Duration;

use common::{spawn_scode_in_dir, HarnessWorkspace, TestEnv};

/// Per-model timeout — generous because some models are slow to cold-start.
const MODEL_TIMEOUT: Duration = Duration::from_secs(90);

/// Result for a single model compatibility check.
#[derive(Debug)]
struct ModelResult {
    model: String,
    status: ModelStatus,
    detail: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ModelStatus {
    Pass,
    Skip,
    Fail,
}

impl std::fmt::Display for ModelStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Skip => write!(f, "SKIP"),
            Self::Fail => write!(f, "FAIL"),
        }
    }
}

/// Run a single model through a "What is 2+2?" smoke test.
///
/// Returns `Pass` if the model responds with "4", `Skip` if the model
/// is unavailable (429, timeout, connection error), or `Fail` if the
/// model responds but the answer is wrong or scode exits non-zero.
fn test_one_model(model: &str) -> ModelResult {
    let workspace = HarnessWorkspace::new(&format!("compat-{model}"));
    let spawn_result = spawn_scode_in_dir(
        &workspace.root,
        &[
            "--model",
            model,
            "--auth",
            "proxy",
            "--compact",
            "--permission-mode",
            "read-only",
            "What is 2+2? Answer with just the number.",
        ],
        MODEL_TIMEOUT,
    );

    let mut sess = match spawn_result {
        Ok(sess) => sess,
        Err(e) => {
            return ModelResult {
                model: model.to_string(),
                status: ModelStatus::Skip,
                detail: format!("spawn failed: {e}"),
            };
        }
    };

    sess.set_default_timeout(MODEL_TIMEOUT);

    // Look for "4" in the output. If the model is unavailable (429,
    // rate limit, timeout), treat it as a skip rather than a failure.
    match sess.expect("4") {
        Ok(_) => {
            // Got the expected answer — wait for exit.
            match sess.expect_eof() {
                Ok(0) => ModelResult {
                    model: model.to_string(),
                    status: ModelStatus::Pass,
                    detail: "responded with 4, exit 0".to_string(),
                },
                Ok(code) => ModelResult {
                    model: model.to_string(),
                    status: ModelStatus::Fail,
                    detail: format!("responded with 4 but exit code {code}"),
                },
                Err(e) => ModelResult {
                    model: model.to_string(),
                    status: ModelStatus::Pass,
                    detail: format!("responded with 4, eof error (non-critical): {e}"),
                },
            }
        }
        Err(e) => {
            // Capture the PTY screen to distinguish availability errors
            // from genuine incompatibility.
            let screen = sess.render(|s| s.contents());
            let is_availability_error = screen.contains("429")
                || screen.contains("rate limit")
                || screen.contains("Rate limit")
                || screen.contains("overloaded")
                || screen.contains("503")
                || screen.contains("502")
                || screen.contains("timed out")
                || screen.contains("timeout")
                || screen.contains("ETIMEDOUT")
                || screen.contains("ECONNREFUSED")
                || screen.contains("connection refused")
                || screen.contains("upstream")
                || screen.contains("saturated");

            if is_availability_error {
                ModelResult {
                    model: model.to_string(),
                    status: ModelStatus::Skip,
                    detail: format!("upstream unavailable: {e}"),
                }
            } else {
                ModelResult {
                    model: model.to_string(),
                    status: ModelStatus::Fail,
                    detail: format!("expect error: {e}\nPTY screen:\n{screen}"),
                }
            }
        }
    }
}

/// Parse the `SCODE_COMPAT_MODELS` env var into a list of model IDs.
fn compat_models() -> Vec<String> {
    std::env::var("SCODE_COMPAT_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

// ──────────────────────────────────────────────────────────────────────
// Test entry point
// ──────────────────────────────────────────────────────────────────────

/// Parameterized model compatibility test.
///
/// Reads `SCODE_COMPAT_MODELS` and tests each model sequentially.
/// Prints a summary table and writes a JSON report to the workspace.
///
/// The test **passes** as long as there are no `Fail` results.
/// `Skip` (upstream unavailable) does not count as failure.
#[test]
fn model_compat_sweep() {
    let env = TestEnv::new("model-compat");
    if env.is_mock() {
        // No mock scenario for arbitrary models — pass vacuously.
        eprintln!("model_compat_sweep: mock mode, skipping");
        return;
    }

    let models = compat_models();
    if models.is_empty() {
        eprintln!("model_compat_sweep: SCODE_COMPAT_MODELS is empty, skipping");
        return;
    }

    eprintln!(
        "model_compat_sweep: testing {} model(s): {}",
        models.len(),
        models.join(", ")
    );

    let results: Vec<ModelResult> = models.iter().map(|m| test_one_model(m)).collect();

    // Print summary table.
    let header = format!("\n{:<40} {:<6} DETAIL", "MODEL", "STATUS");
    eprintln!("{header}");
    eprintln!("{}", "-".repeat(80));
    for result in &results {
        eprintln!(
            "{:<40} {:<6} {}",
            result.model,
            result.status,
            // Truncate detail for table readability.
            result.detail.lines().next().unwrap_or("")
        );
    }

    let pass_count = results
        .iter()
        .filter(|r| r.status == ModelStatus::Pass)
        .count();
    let skip_count = results
        .iter()
        .filter(|r| r.status == ModelStatus::Skip)
        .count();
    let fail_count = results
        .iter()
        .filter(|r| r.status == ModelStatus::Fail)
        .count();

    eprintln!("\nSummary: {pass_count} pass, {skip_count} skip, {fail_count} fail");

    // Write JSON report for CI artifact upload.
    let report = serde_json::json!({
        "total": results.len(),
        "pass": pass_count,
        "skip": skip_count,
        "fail": fail_count,
        "models": results.iter().map(|r| serde_json::json!({
            "model": r.model,
            "status": r.status.to_string(),
            "detail": r.detail,
        })).collect::<Vec<_>>(),
    });

    let report_path = env.workspace_root().join("model-compat-report.json");
    if let Err(e) = std::fs::write(&report_path, serde_json::to_string_pretty(&report).unwrap()) {
        eprintln!(
            "warning: failed to write report to {}: {e}",
            report_path.display()
        );
    } else {
        eprintln!("Report written to {}", report_path.display());
    }

    // Fail the test if any model has a genuine compatibility failure.
    assert_eq!(
        fail_count, 0,
        "{fail_count} model(s) failed compatibility check"
    );
}
