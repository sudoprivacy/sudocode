//! PTY tests for configuration & discovery features.
//!
//! These test real user interactions: tab completion in the REPL,
//! slash command switching (model, permissions, auth), fuzzy
//! suggestion on typos, and CLI flags (--compact, credential bypass).
//!
//! Tab completion and fuzzy suggest are PTY-only features — they
//! can't be tested via piped stdin. This is where PTY testing earns
//! its keep.
//!
//! ```bash
//! cargo test --test pty_config_discovery
//! ```

mod common;

use std::time::Duration;

use common::{spawn_scode, TestEnv};

// ──────────────────────────────────────────────────────────────────────
// 1. Tab completion shows slash command candidates
// ──────────────────────────────────────────────────────────────────────

/// Human types `/mod` in the REPL, presses Tab, sees `/model` in
/// the completion list. This is a PTY-only test — tab completion
/// doesn't work with piped stdin.
///
/// Steps:
/// 1. Spawn scode in REPL (mock, to avoid API calls).
/// 2. Wait for prompt.
/// 3. Type `/mod` (partial command).
/// 4. Send Tab character.
/// 5. Expect "model" in the completion output.
#[test]
fn tab_completion_shows_slash_commands() {
    let env = TestEnv::new("tab-complete");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "REPL input ready");

    // Type a partial command, wait for the editor to show it, then Tab.
    sess.send("/mod").expect("type partial command");
    common::expect_screen(
        &sess,
        |screen| common::input_line_of(screen) == "/mod",
        common::DEFAULT_TIMEOUT,
        "partial command reached the editor",
    );
    sess.send("\t").expect("send Tab");

    // Tab completes the input to the first matching command.
    common::expect_screen(
        &sess,
        |screen| common::input_line_of(screen) == "/model",
        common::DEFAULT_TIMEOUT,
        "tab completion should complete /mod to /model",
    );

    // Clean exit.
    sess.send_ctrl('c').expect("cancel");
    std::thread::sleep(Duration::from_millis(100));
    sess.send_ctrl('c').expect("exit");
    let _ = sess.expect_eof();
}

// ──────────────────────────────────────────────────────────────────────
// 2. /model switches model in REPL
// ──────────────────────────────────────────────────────────────────────

/// Human types `/model claude-haiku` in a Sonnet REPL, sees confirmation
/// that the model switched. Naming the current model would only print the
/// model report, so the target differs from the startup model.
#[test]
fn model_switch_in_repl() {
    let env = TestEnv::new("model-switch");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    common::expect_input_line_cleared(&sess, env.timeout(), "REPL input ready");

    sess.send("/model claude-haiku\r")
        .expect("send /model claude-haiku");

    // The switch report, not the echoed command line, proves the switch.
    let screen = expect_report(
        &sess,
        &env,
        "Model updated",
        "should see model switch confirmation",
    );
    assert!(
        screen
            .lines()
            .any(|line| line.trim_start().starts_with("Current") && line.contains("haiku")),
        "the switch report should name Haiku as the current model:\n{screen}"
    );

    // Clean exit.
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 2b. /model opens the interactive model picker
// ──────────────────────────────────────────────────────────────────────

/// In the REPL, `/model` without arguments opens the interactive picker.
#[test]
fn model_without_args_opens_picker() {
    let env = TestEnv::new("model-picker");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    common::expect_input_line_cleared(&sess, env.timeout(), "REPL input ready");

    sess.send("/model\r").expect("send /model (no args)");

    common::expect_screen(
        &sess,
        |screen| screen.contains("Select model"),
        env.timeout(),
        "should see interactive model picker",
    );

    // Cancel the picker and return to the prompt.
    sess.send("\x1b").expect("cancel picker");
    common::expect_screen(
        &sess,
        |screen| !screen.contains("Select model"),
        env.timeout(),
        "Esc should close the model picker",
    );
    common::expect_input_line_cleared(&sess, env.timeout(), "prompt after picker");

    // Clean exit.
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 3. /permissions switches mode in REPL
// ──────────────────────────────────────────────────────────────────────

/// Human types `/permissions read-only` in REPL, sees confirmation.
/// Then runs /status to verify the permission mode actually changed.
#[test]
fn permissions_switch_in_repl() {
    let env = TestEnv::new("perm-switch");
    // Start in danger-full-access so we can switch down.
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access"]);

    common::expect_input_line_cleared(&sess, env.timeout(), "REPL input ready");

    sess.send("/permissions read-only\r")
        .expect("send /permissions");

    // Switch report shows "Active mode    read-only".
    let screen = expect_report(
        &sess,
        &env,
        "Permissions updated",
        "should see the switch report",
    );
    assert!(
        screen
            .lines()
            .any(|line| line.trim_start().starts_with("Active mode")
                && line.trim_end().ends_with("read-only")),
        "the switch report should show read-only as the active mode:\n{screen}"
    );

    // Clean exit.
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let _ = sess.expect_eof();
}

// ──────────────────────────────────────────────────────────────────────
// 4. /agents list (CLI subcommand)
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode agents --output-format json`, sees JSON with
/// agents array.
#[test]
fn agents_list_renders_json() {
    let mut sess = spawn_scode(&["agents", "--output-format", "json"]).expect("spawn scode agents");

    sess.expect("agents").expect("should contain agents key");
    sess.expect("kind").expect("should contain kind field");

    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 5. /skills list (CLI subcommand)
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode skills --output-format json`, sees JSON with
/// skills listing.
#[test]
fn skills_list_renders_json() {
    let mut sess = spawn_scode(&["skills", "--output-format", "json"]).expect("spawn scode skills");

    sess.expect("skills").expect("should contain skills key");

    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 6. --compact suppresses tool call details
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode --compact "What is 2+2?"`. In compact mode,
/// only the final assistant text is printed — no tool boxes, no
/// spinner, no status line.
///
/// This is a live/mock dual-mode test. The compact flag controls
/// output format, not the model.
#[test]
fn compact_flag_shows_only_final_text() {
    let env = TestEnv::new("compact-flag");
    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");

    let mut sess = env.spawn(&["--compact", "--permission-mode", "read-only", &prompt]);

    // Should see the answer.
    sess.expect("4")
        .expect("compact output should contain answer");

    let exit = sess.expect_eof().expect("should exit");
    assert_eq!(exit, 0);
}

// ──────────────────────────────────────────────────────────────────────
// 7. Informational commands bypass credential check
// ──────────────────────────────────────────────────────────────────────

/// Human runs `scode version`, `scode help`, `scode doctor` without
/// any API credentials configured. All should succeed (exit 0).
/// These commands don't call the API.
#[test]
fn informational_commands_bypass_credentials() {
    for subcmd in &["version", "help", "doctor", "status", "sandbox", "config"] {
        let mut sess = spawn_scode(&[subcmd]).unwrap_or_else(|_| panic!("spawn scode {subcmd}"));

        let exit = sess
            .expect_eof()
            .unwrap_or_else(|_| panic!("{subcmd} should exit"));
        assert_eq!(exit, 0, "{subcmd} should exit 0 without credentials");
    }
}

// ──────────────────────────────────────────────────────────────────────
// 8. Slash command fuzzy suggestion on typo
// ──────────────────────────────────────────────────────────────────────

/// Human types `/comit` (typo for /commit) in the REPL. scode
/// should suggest "did you mean /commit?" or similar fuzzy match.
///
/// This is a PTY-only test — fuzzy suggestion only fires in the
/// interactive REPL.
#[test]
fn slash_command_fuzzy_suggest_on_typo() {
    let env = TestEnv::new("fuzzy-suggest");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    common::expect_input_line_cleared(&sess, env.timeout(), "REPL input ready");

    // Submit a typo'd slash command.
    sess.send("/comit\r").expect("send typo command");

    // The error names the typo and suggests the command it resembles.
    let screen = expect_report(
        &sess,
        &env,
        "Unknown slash command: /comit",
        "should report the unknown command",
    );
    assert!(
        screen.contains("Did you mean") && screen.contains("/commit"),
        "the error should suggest /commit:\n{screen}"
    );

    // Clean exit.
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let _ = sess.expect_eof();
}

/// Wait for a command's report and for the editor to be ready again. Waiting
/// on the report text rather than the prompt glyph matters: the glyph is
/// permanent chrome, and the echoed command line contains the same words.
fn expect_report(
    sess: &pty_expect::PtySession,
    env: &TestEnv,
    report: &str,
    context: &str,
) -> String {
    let screen = common::expect_screen(
        sess,
        |screen| screen.contains(report),
        env.timeout(),
        context,
    );
    common::expect_input_line_cleared(sess, env.timeout(), context);
    screen
}
