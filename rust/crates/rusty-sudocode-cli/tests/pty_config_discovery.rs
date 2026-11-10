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

use common::TestEnv;

fn spawn_without_credentials(env: &TestEnv, args: &[&str]) -> pty_expect::PtySession {
    let config = env.workspace_root().join("empty-config");
    std::fs::create_dir_all(&config).expect("empty informational config");
    let config = config.to_string_lossy();
    env.spawn_binary_with_env(
        &env.binary_path(),
        args,
        &[
            ("SCODE_GLOBAL_CONFIG_DIR", config.as_ref()),
            ("SCODE_PROJECT_CONFIG_DIR", config.as_ref()),
            ("ANTHROPIC_API_KEY", ""),
            ("ANTHROPIC_AUTH_TOKEN", ""),
            ("PROXY_AUTH_TOKEN", ""),
            ("CLAUDE_CODE_OAUTH_TOKEN", ""),
        ],
    )
}

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

/// Tab completes a command's arguments from its usage hint and from runtime
/// values: a literal choice, a subcommand followed by a session id, a file in
/// the working directory, and a model.
/// Tab on a session id stays instant however slowly a stored transcript reads.
/// Completion runs on the input thread, so it lists ids from the store's names
/// alone. Here one stored transcript is a FIFO with no writer: opening it
/// would block forever, so reading any transcript would freeze the REPL at
/// this Tab, as a slow disk, a locked file or a network filesystem would.
#[cfg(unix)]
#[test]
fn session_id_completion_never_reads_a_transcript() {
    let env = TestEnv::new("tab-complete-unreadable-session");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "REPL input ready");

    let sessions = env.workspace_root().join(".scode/sessions");
    let store = std::fs::read_dir(&sessions)
        .expect("the running session created its store")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("one workspace store");
    let blocked = store.join("session-unreadable");
    std::fs::create_dir_all(&blocked).unwrap();
    let fifo = blocked.join("transcript.jsonl");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo {}", fifo.display());

    sess.send("/session switch ").expect("type the command");
    common::expect_screen(
        &sess,
        |screen| common::input_line_of(screen) == "/session switch",
        common::DEFAULT_TIMEOUT,
        "command reached the editor",
    );
    sess.send("\t").expect("send Tab");
    common::expect_screen(
        &sess,
        |screen| {
            common::input_line_of(screen)
                .strip_prefix("/session switch ")
                .is_some_and(|id| !id.is_empty() && !id.contains(' '))
        },
        common::DEFAULT_TIMEOUT,
        "a session id, without opening the unreadable transcript",
    );
    // Dropping the session kills the child, even one blocked opening the FIFO.
}

#[test]
fn tab_completion_completes_arguments() {
    let env = TestEnv::new("tab-complete-args");
    std::fs::write(env.workspace_root().join("notes-export.md"), "fixture")
        .expect("write completion fixture");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "REPL input ready");

    // The screen trims the input line, so a trailing space shows only through
    // what the next Tab completes.
    let complete = |sess: &mut pty_expect::PtySession,
                    typed: &str,
                    tabs: usize,
                    done: &dyn Fn(&str) -> bool,
                    what: &str| {
        sess.send(typed).expect("type partial command");
        common::expect_screen(
            sess,
            |screen| common::input_line_of(screen) == typed.trim(),
            common::DEFAULT_TIMEOUT,
            "partial command reached the editor",
        );
        for _ in 0..tabs {
            sess.send("\t").expect("send Tab");
        }
        common::expect_screen(
            sess,
            |screen| done(&common::input_line_of(screen)),
            common::DEFAULT_TIMEOUT,
            what,
        );
        sess.send_ctrl('u').expect("clear the input line");
        common::expect_screen(
            sess,
            |screen| common::input_line_of(screen).is_empty(),
            common::DEFAULT_TIMEOUT,
            "input line cleared",
        );
    };

    complete(
        &mut sess,
        "/permissions wo",
        1,
        &|line| line == "/permissions workspace-write",
        "a literal choice from the usage hint",
    );
    complete(
        &mut sess,
        "/session sw",
        2,
        &|line| {
            line.strip_prefix("/session switch ")
                .is_some_and(|id| !id.is_empty() && !id.contains(' '))
        },
        "the subcommand, then a session id from the session store",
    );
    complete(
        &mut sess,
        "/export notes-ex",
        1,
        &|line| line == "/export notes-export.md",
        "a file in the working directory",
    );
    complete(
        &mut sess,
        "/model ",
        1,
        &|line| {
            line.strip_prefix("/model ")
                .is_some_and(|model| !model.is_empty())
        },
        "a model the session can switch to",
    );

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
    let env = TestEnv::new_mock("agents-list-without-credentials");
    let mut sess = spawn_without_credentials(&env, &["agents", "--output-format", "json"]);

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
    let env = TestEnv::new_mock("skills-list-without-credentials");
    let mut sess = spawn_without_credentials(&env, &["skills", "--output-format", "json"]);

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
    let env = TestEnv::new_mock("informational-without-credentials");
    for subcmd in &["version", "help", "doctor", "status", "sandbox", "config"] {
        // Diagnose the command's credential-free behavior, independent of a
        // developer's saved credentials, invalid settings or active workspace.
        let mut sess = spawn_without_credentials(&env, &[subcmd]);
        if *subcmd == "doctor" {
            sess.expect("no supported auth env vars were found")
                .expect("doctor must observe the credential-free fixture");
        }

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
