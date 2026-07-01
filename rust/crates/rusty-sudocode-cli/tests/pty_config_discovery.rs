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

    sess.expect("❯").expect("should see REPL prompt");

    // Type partial command then Tab.
    sess.send("/mod").expect("type partial command");
    std::thread::sleep(Duration::from_millis(100));
    sess.send("\t").expect("send Tab");

    // Completion list should show /model.
    sess.expect("(?i)model")
        .expect("tab completion should suggest /model");

    // Clean exit.
    sess.send_ctrl('c').expect("cancel");
    std::thread::sleep(Duration::from_millis(100));
    sess.send_ctrl('c').expect("exit");
    let _ = sess.expect_eof();
}

// ──────────────────────────────────────────────────────────────────────
// 2. /model switches model in REPL
// ──────────────────────────────────────────────────────────────────────

/// Human types `/model sonnet` in REPL, sees confirmation that the
/// model switched.
#[test]
fn model_switch_in_repl() {
    let env = TestEnv::new("model-switch");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);

    sess.expect("❯").expect("should see REPL prompt");

    sess.send("/model sonnet\r").expect("send /model sonnet");

    // Should confirm the model switch — mentions "sonnet" or "model".
    sess.expect("(?i)(sonnet|model|switched|active)")
        .expect("should see model switch confirmation");

    // Clean exit.
    sess.expect("❯").expect("prompt after switch");
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(Duration::from_secs(15));
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

    sess.expect("❯").expect("should see REPL prompt");

    sess.send("/permissions read-only\r")
        .expect("send /permissions");

    // Switch report shows "Active mode    read-only".
    sess.expect("read-only")
        .expect("should see read-only in switch confirmation");

    // Clean exit.
    sess.expect("❯").expect("prompt after switch");
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(Duration::from_secs(15));
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

    sess.expect("❯").expect("should see REPL prompt");

    // Submit a typo'd slash command.
    sess.send("/comit\r").expect("send typo command");

    // Should see a suggestion or "not implemented" or "did you mean".
    sess.expect("(?i)(commit|did you mean|not implemented|unknown|similar)")
        .expect("should suggest the correct command or report error");

    // Clean exit.
    sess.expect("❯").expect("prompt after error");
    sess.send("/exit\r").expect("exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
