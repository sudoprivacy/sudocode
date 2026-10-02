//! Model compatibility PTY tests — verifies that arbitrary models
//! served by sudorouter can be used via proxy passthrough.
//!
//! The model list is read from the `SCODE_COMPAT_MODELS` environment
//! variable (comma-separated model IDs). When the variable is unset or
//! empty, a small built-in default set (`DEFAULT_COMPAT_MODELS`) is used
//! so a bare live run still exercises real passthrough instead of passing
//! vacuously. CI's `model-compat.yml` overrides the default with the full
//! model list fetched from sudorouter's `/v1/models` endpoint in bounded batches.
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{spawn_scode_in_dir_with_env, HarnessWorkspace, TestEnv};
use runtime::{ContentBlock, MessageRole, Session};

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
    Refused,
    Unsupported,
    Fail,
}

impl std::fmt::Display for ModelStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Skip => write!(f, "SKIP"),
            Self::Refused => write!(f, "REFUSED"),
            Self::Unsupported => write!(f, "UNSUPPORTED"),
            Self::Fail => write!(f, "FAIL"),
        }
    }
}

/// Check if the PTY screen contains patterns indicating the model is
/// unavailable (as opposed to genuinely incompatible). These are
/// transient/group-membership errors that should be skipped, not failed.
fn is_availability_error(screen: &str) -> bool {
    common::model_unavailable_in_screen(screen)
        // Also reproduced with a minimal direct gateway request, independently
        // of scode's prompt and tools: the dependent provider rejects the route.
        || screen.contains("Bad request for dependent service.")
        // A connection failure before HTTP, also seen in a direct gateway probe.
        || screen.contains("client error (Connect): tls handshake eof")
}

/// Read only the last completed HTTP attempt. A later successful HTTP response
/// must invalidate an earlier outage, even if its stream then hangs or is empty.
/// The full trace can contain request data; reports retain only the error.
fn latest_request_failure(path: &Path) -> Option<String> {
    let log = std::fs::read_to_string(path).ok()?;
    for line in log.lines().rev() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event["event"].as_str() {
            Some("request_succeeded") => return None,
            Some("request_failed") => {
                return event["attributes"]["error"].as_str().map(str::to_string);
            }
            _ => {}
        }
    }
    None
}

/// Only a protocol refusal from the current HTTP attempt can explain this exit.
fn latest_provider_refusal(path: &Path) -> Option<serde_json::Value> {
    let log = std::fs::read_to_string(path).ok()?;
    for line in log.lines().rev() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event["event"].as_str() {
            Some("provider_refusal") => return Some(event["attributes"].clone()),
            Some("request_succeeded" | "request_failed" | "request_debug") => return None,
            _ => {}
        }
    }
    None
}

/// Only an endpoint-catalog preflight rejection with no inference attempt can
/// establish unsupported capability. Model output cannot opt out of this test.
fn unsupported_tool_capability(path: &Path, model: &str) -> bool {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let mut rejected = false;
    for line in log.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event["event"].as_str() {
            Some("request_debug" | "request_succeeded" | "request_failed") => return false,
            Some("model_capability_rejected") => {
                let attributes = &event["attributes"];
                rejected |= attributes["model"] == model
                    && attributes["capability"] == "tool_calling"
                    && attributes["source"] == "endpoint_catalog";
            }
            _ => {}
        }
    }
    rejected
}

/// Keep request-shape diagnostics without prompts, tool definitions or headers.
fn request_diagnostics(path: &Path) -> serde_json::Value {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let events: Vec<_> = log
        .lines()
        .filter_map(|line| {
            let event: serde_json::Value = serde_json::from_str(line).ok()?;
            let attributes = &event["attributes"];
            match event["event"].as_str()? {
                "request_debug" => {
                    let body = &attributes["body"];
                    Some(serde_json::json!({"request": {
                        "model": body["model"], "max_tokens": body["max_tokens"],
                        "thinking": body["thinking"], "reasoning_effort": body["reasoning_effort"],
                        "stream": body["stream"], "tool_choice": body["tool_choice"],
                        "tool_count": body["tools"].as_array().map(Vec::len)
                    }}))
                }
                "request_succeeded" | "request_failed" => Some(serde_json::json!({
                    "event": event["event"], "path": attributes["path"],
                    "status": attributes["status"], "error": attributes["error"]
                })),
                "provider_refusal" => Some(serde_json::json!({
                    "event": event["event"], "model": attributes["model"],
                    "category": attributes["category"], "explanation": attributes["explanation"]
                })),
                "model_capability_rejected" => Some(serde_json::json!({
                    "event": event["event"], "model": attributes["model"],
                    "capability": attributes["capability"], "source": attributes["source"]
                })),
                _ => None,
            }
        })
        .collect();
    serde_json::json!(events)
}

/// Run a single model through a "What is 2+2?" smoke test.
///
/// Returns `Pass` only for a persisted assistant answer of "4" and exit 0.
/// Explicit provider availability failures are `Skip`; process timeouts,
/// incorrect answers and unexplained nonzero exits are `Fail`.
#[allow(clippy::redundant_closure_for_method_calls)]
fn test_one_model(env: &TestEnv, model: &str) -> ModelResult {
    let workspace = HarnessWorkspace::new(&format!("compat-{model}"));
    let request_log = workspace.root.join("model-requests.jsonl");
    let spawn_result = spawn_scode_in_dir_with_env(
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
        &[
            ("SUDO_CODE_CONFIG_HOME", env.config_home()),
            ("SCODE_LOG_PATH", &request_log),
        ],
    );

    let mut sess = match spawn_result {
        Ok(sess) => sess,
        Err(e) => {
            return ModelResult {
                model: model.to_string(),
                status: ModelStatus::Fail,
                detail: format!("spawn failed: {e}"),
            };
        }
    };

    sess.set_default_timeout(MODEL_TIMEOUT);

    // One deadline for the entire process. A model name or request ID containing
    // "4" is not an answer, and a timeout after that match is not success.
    let exit = sess.expect_eof();
    let screen = sess.render(|s| s.contents());
    let request_error = latest_request_failure(&request_log);
    let refusal = latest_provider_refusal(&request_log);
    let unsupported = unsupported_tool_capability(&request_log, model);
    let (status, mut detail) = match exit {
        Ok(0) => match assistant_answer(&workspace.root.join(".scode")) {
            Ok(answer) if answer.trim() == "4" => (
                ModelStatus::Pass,
                "assistant answered 4, exit 0".to_string(),
            ),
            Ok(answer) => (
                ModelStatus::Fail,
                format!("unexpected assistant answer: {answer:?}"),
            ),
            Err(error) => (ModelStatus::Fail, error),
        },
        Ok(code) if unsupported => (
            ModelStatus::Unsupported,
            format!(
                "endpoint declares tool calling unsupported; no inference attempted, exit {code}"
            ),
        ),
        Ok(code) if refusal.is_some() => (
            ModelStatus::Refused,
            format!(
                "provider explicitly refused the request, exit {code}: {}",
                refusal.unwrap()
            ),
        ),
        Ok(code)
            if is_availability_error(&screen)
                || request_error.as_deref().is_some_and(is_availability_error) =>
        {
            (
                ModelStatus::Skip,
                format!(
                    "upstream unavailable, exit {code}: {screen}; HTTP error: {request_error:?}"
                ),
            )
        }
        Ok(code) => (ModelStatus::Fail, format!("exit {code}: {screen}")),
        // Retry backoff can outlast the PTY deadline. A concrete provider error
        // explains that timeout; a generic "still waiting" notice never does.
        Err(_) if request_error.as_deref().is_some_and(is_availability_error) => (
            ModelStatus::Skip,
            format!(
                "upstream unavailable while CLI retries: {}",
                request_error.as_deref().unwrap_or_default()
            ),
        ),
        Err(error) => (
            ModelStatus::Fail,
            format!(
                "process did not finish: {error}; last HTTP error: {request_error:?}\n{screen}"
            ),
        ),
    };
    if status != ModelStatus::Skip {
        use std::fmt::Write as _;
        let _ = write!(
            detail,
            "\nHTTP trace: {}",
            request_diagnostics(&request_log)
        );
    }
    ModelResult {
        model: model.to_string(),
        status,
        detail,
    }
}

fn assistant_answer(root: &Path) -> Result<String, String> {
    fn find(dir: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                find(&path, paths)?;
            } else if path
                .file_name()
                .is_some_and(|name| name == "transcript.jsonl")
            {
                paths.push(path);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    find(root, &mut paths).map_err(|e| format!("missing conversation: {e}"))?;
    if paths.len() != 1 {
        return Err(format!("expected one conversation, found {}", paths.len()));
    }
    let session = Session::load_from_path(&paths[0]).map_err(|e| e.to_string())?;
    let message = session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .ok_or_else(|| "conversation has no assistant answer".to_string())?;
    Ok(message
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>())
}

/// Model IDs exercised when `SCODE_COMPAT_MODELS` is unset or empty, so a
/// bare `SCODE_TEST_BACKEND=live cargo test --test pty_model_compat` run
/// verifies real passthrough instead of passing vacuously. Both are
/// unconfigured-in-`sudocode.json` IDs that sudorouter serves (spanning two
/// model families), so `--auth proxy` genuinely goes down the passthrough
/// path. CI's `model-compat.yml` overrides this with the full endpoint list.
const DEFAULT_COMPAT_MODELS: &[&str] = &["o3-mini", "doubao-seed-1-6-251015"];

/// Parse the `SCODE_COMPAT_MODELS` env var into a list of model IDs,
/// falling back to [`DEFAULT_COMPAT_MODELS`] when it is unset or empty.
fn compat_models() -> Vec<String> {
    let from_env: Vec<String> = std::env::var("SCODE_COMPAT_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    if from_env.is_empty() {
        DEFAULT_COMPAT_MODELS
            .iter()
            .map(ToString::to_string)
            .collect()
    } else {
        from_env
    }
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
/// `Skip` (upstream unavailable), `Refused`, and `Unsupported` (catalog-declared
/// missing tool support) do not count as failure or as
/// verified compatibility. CI's aggregate requires at least one actual pass.
#[test]
fn model_compat_sweep() {
    let env = TestEnv::new("model-compat");
    if env.is_mock() {
        // No mock scenario for arbitrary models — pass vacuously.
        eprintln!("model_compat_sweep: mock mode, skipping");
        return;
    }

    // Never empty: falls back to DEFAULT_COMPAT_MODELS when the env is unset,
    // so a live run always exercises at least the built-in default set.
    let models = compat_models();

    eprintln!(
        "model_compat_sweep: testing {} model(s): {}",
        models.len(),
        models.join(", ")
    );

    // CI puts the report outside the disposable workspace. Write before starting
    // and after each model so a killed sweep still leaves useful evidence.
    let report_path = std::env::var_os("SCODE_COMPAT_REPORT").map_or_else(
        || env.workspace_root().join("model-compat-report.json"),
        PathBuf::from,
    );
    let mut results = Vec::new();
    write_report(&report_path, models.len(), &results);
    for (index, model) in models.iter().enumerate() {
        eprintln!("[{}/{}] starting {model}", index + 1, models.len());
        let result = test_one_model(&env, model);
        eprintln!("{model}: {} {}", result.status, result.detail);
        results.push(result);
        write_report(&report_path, models.len(), &results);
    }

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
    let refused_count = results
        .iter()
        .filter(|r| r.status == ModelStatus::Refused)
        .count();
    let unsupported_count = results
        .iter()
        .filter(|r| r.status == ModelStatus::Unsupported)
        .count();

    eprintln!("\nSummary: {pass_count} pass, {skip_count} skip, {refused_count} refused, {unsupported_count} unsupported, {fail_count} fail");

    assert_eq!(
        fail_count, 0,
        "{fail_count} model(s) failed compatibility check"
    );
}

fn write_report(path: &Path, expected: usize, results: &[ModelResult]) {
    let report = serde_json::json!({
        "total": expected,
        "completed": results.len(),
        "pass": results.iter().filter(|r| r.status == ModelStatus::Pass).count(),
        "skip": results.iter().filter(|r| r.status == ModelStatus::Skip).count(),
        "refused": results.iter().filter(|r| r.status == ModelStatus::Refused).count(),
        "unsupported": results.iter().filter(|r| r.status == ModelStatus::Unsupported).count(),
        "fail": results.iter().filter(|r| r.status == ModelStatus::Fail).count(),
        "models": results.iter().map(|r| serde_json::json!({
            "model": r.model,
            "status": r.status.to_string(),
            "detail": r.detail,
        })).collect::<Vec<_>>(),
    });

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).expect("create report directory");
    }
    std::fs::write(path, serde_json::to_string_pretty(&report).unwrap())
        .expect("write compatibility report");
}
