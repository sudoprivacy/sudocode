//! PTY tests for CLI subcommands that run without an API key.
//!
//! These test the "human runs scode <subcommand>, sees output" journey.
//! No mock or live API needed — these subcommands produce output from
//! local state only. They exercise the CLI surface, output formatting,
//! and structured JSON output.
//!
//! ```bash
//! cargo test --test pty_subcommands
//! ```

mod common;

use common::spawn_scode;

// ──────────────────────────────────────────────────────────────────────
// 1. scode status — model, permissions, session info
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode status --output-format json`, sees structured
/// status with model, permission mode, and session info.
///
/// Steps:
/// 1. Spawn scode status --output-format json.
/// 2. Expect JSON with "model" key — proves status renders.
/// 3. Expect "permission" — proves permission mode is shown.
/// 4. Exit 0.
#[test]
fn status_renders_json() {
    let mut sess = spawn_scode(&["status", "--output-format", "json"]).expect("spawn scode status");

    sess.expect("model").expect("status should contain model");
    sess.expect("permission")
        .expect("status should contain permission info");

    let exit = sess.expect_eof().expect("scode status should exit");
    assert_eq!(exit, 0, "scode status should exit 0; got {exit}");
}

/// Human runs `scode status` (plain text), sees readable output.
#[test]
fn status_renders_text() {
    let mut sess = spawn_scode(&["status"]).expect("spawn scode status");

    // Plain text status should mention the model name.
    sess.expect("(?i)(model|auto|sonnet|opus)")
        .expect("text status should mention model");

    let exit = sess.expect_eof().expect("scode status should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 2. scode doctor — diagnostic checks
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode doctor --output-format json`, sees diagnostic
/// checks for API keys and environment.
///
/// Steps:
/// 1. Spawn scode doctor --output-format json.
/// 2. Expect "checks" — proves doctor ran.
/// 3. Expect "api_key" — proves it checked for API keys.
/// 4. Exit 0.
#[test]
fn doctor_renders_json() {
    let mut sess = spawn_scode(&["doctor", "--output-format", "json"]).expect("spawn scode doctor");

    sess.expect("checks")
        .expect("doctor should contain checks array");
    sess.expect("api_key")
        .expect("doctor should check for api key");

    let exit = sess.expect_eof().expect("scode doctor should exit");
    assert_eq!(exit, 0, "scode doctor should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 3. scode version — build info
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode version --output-format json`, sees build info.
///
/// Steps:
/// 1. Spawn scode version --output-format json.
/// 2. Expect "git_sha" — proves build info renders.
/// 3. Expect "build_date" — proves date is included.
/// 4. Exit 0.
#[test]
fn version_renders_json() {
    let mut sess =
        spawn_scode(&["version", "--output-format", "json"]).expect("spawn scode version");

    sess.expect("build_date")
        .expect("version should contain build date");
    sess.expect("git_sha")
        .expect("version should contain git sha");

    let exit = sess.expect_eof().expect("scode version should exit");
    assert_eq!(exit, 0, "scode version should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 4. scode config — configuration report
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode config --output-format json`, sees config files
/// and their load status.
///
/// Steps:
/// 1. Spawn scode config --output-format json.
/// 2. Expect "files" — proves config file listing renders.
/// 3. Expect "cwd" — proves working directory is shown.
/// 4. Exit 0.
#[test]
fn config_renders_json() {
    let mut sess = spawn_scode(&["config", "--output-format", "json"]).expect("spawn scode config");

    sess.expect("cwd")
        .expect("config should show working directory");
    sess.expect("files")
        .expect("config should list config files");

    let exit = sess.expect_eof().expect("scode config should exit");
    assert_eq!(exit, 0, "scode config should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 5. scode sandbox — isolation status
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode sandbox --output-format json`, sees sandbox info.
#[test]
fn sandbox_renders_json() {
    let mut sess =
        spawn_scode(&["sandbox", "--output-format", "json"]).expect("spawn scode sandbox");

    sess.expect("active")
        .expect("sandbox should show active status");

    let exit = sess.expect_eof().expect("scode sandbox should exit");
    assert_eq!(exit, 0, "scode sandbox should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 6. scode system-prompt — rendered system prompt
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode system-prompt`, sees the full system prompt
/// that would be sent to the model.
///
/// Steps:
/// 1. Spawn scode system-prompt.
/// 2. Expect "Sudo Code" — proves the preamble renders.
/// 3. Expect "tool" — proves tool descriptions are included.
/// 4. Exit 0.
#[test]
fn system_prompt_renders() {
    let mut sess = spawn_scode(&["system-prompt"]).expect("spawn scode system-prompt");

    sess.expect("(?i)(sudo.code|coding|assistant)")
        .expect("system prompt should contain agent identity");
    sess.expect("(?i)(tool|bash|read_file)")
        .expect("system prompt should reference tools");

    let exit = sess.expect_eof().expect("scode system-prompt should exit");
    assert_eq!(exit, 0, "scode system-prompt should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 7. scode --help — usage info
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode --help`, sees usage info with available commands.
#[test]
fn help_shows_usage_and_subcommands() {
    let mut sess = spawn_scode(&["--help"]).expect("spawn scode --help");

    sess.expect("Usage:").expect("help should show Usage:");
    sess.expect("(?i)(status|doctor|config)")
        .expect("help should list subcommands");

    let exit = sess.expect_eof().expect("scode --help should exit");
    assert_eq!(exit, 0, "scode --help should exit 0; got {exit}");
}

// ──────────────────────────────────────────────────────────────────────
// 8. --output-format json works across subcommands (contract test)
// ──────────────────────────────────────────────────────────────────────

/// Verify that --output-format json produces valid JSON structure
/// across multiple subcommands. This is a contract test — the exact
/// content varies, but every subcommand must produce a JSON object
/// with a "kind" field.
#[test]
fn output_format_json_contract() {
    for subcmd in &["status", "doctor", "version", "sandbox"] {
        let mut sess = spawn_scode(&[subcmd, "--output-format", "json"])
            .unwrap_or_else(|_| panic!("spawn scode {subcmd}"));

        // Every JSON subcommand output starts with { and contains "kind".
        sess.expect("kind")
            .unwrap_or_else(|_| panic!("{subcmd} JSON should contain 'kind' field"));

        let exit = sess
            .expect_eof()
            .unwrap_or_else(|_| panic!("{subcmd} should exit"));
        assert_eq!(exit, 0, "{subcmd} --output-format json should exit 0");
    }
}
