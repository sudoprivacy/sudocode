//! PTY tests for `!<command>` bash mode — the human runs a shell command
//! from the REPL prompt without involving the model.
//!
//! ```bash
//! cargo test --test pty_bash_mode                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_bash_mode  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

/// A bare `!` follows the ordinary conversation path on every platform.
/// Keep the first completed turn in the next request and send the bare input
/// unchanged. This also keeps this target meaningful on Windows, where the
/// shell-specific cases below are excluded by their Unix configuration.
#[test]
fn bare_bang_is_a_model_message_with_conversation_history() {
    let env = TestEnv::new("bare-bang");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    let budget = if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        env.timeout()
    };
    // The protocol fixture selects its response from the conversation history,
    // so the second input remains exactly what the human typed.
    let first = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    common::run_turn(&mut sess, &first, budget, "the initial conversation turn");
    common::run_turn(&mut sess, "!", budget, "the bare bang conversation turn");

    if env.is_mock() {
        let bodies = env.captured_message_bodies();
        assert_eq!(
            bodies.len(),
            2,
            "each ordinary input should start one model turn"
        );
        let request: serde_json::Value = serde_json::from_str(&bodies[1]).unwrap();
        let messages = request["messages"].as_array().expect("request messages");
        assert!(
            messages
                .iter()
                .any(|message| message["role"] == "assistant"),
            "the second request should retain the first completed turn: {request}"
        );
        let latest_user = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .expect("the second user message");
        let blocks = latest_user["content"]
            .as_array()
            .expect("user content blocks");
        assert!(
            blocks.iter().any(|block| block["text"] == "!"),
            "the bare bang should reach the model unchanged: {latest_user}"
        );
        assert!(
            !latest_user.to_string().contains("<bash-input>"),
            "the ordinary message should retain its conversation shape: {latest_user}"
        );
    }

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    assert_eq!(sess.expect_eof().expect("REPL should exit cleanly"), 0);
}

/// Human types `! printf …` at the prompt. The command runs immediately, its
/// stdout appears under the echoed input, no model request is made, and the
/// exchange is carried into the next prompt's request as `<bash-input>` /
/// `<bash-stdout>` user messages.
///
/// Steps (causal data flow):
/// 1. Spawn the REPL, wait for the prompt.
/// 2. Send `! printf 'bash-mode-marker-%s' 42`.
/// 3. The `⎿` connector and `bash-mode-marker-42` appear — output rendered.
/// 4. Send a normal prompt; the model answers.
/// 5. Mock only: exactly one `/v1/messages` request was made (the `!` line
///    did not query), and its body carries the bash-input / bash-stdout
///    tags with the marker.
///
/// Catches: `!` forwarded to the model as prose, output not rendered,
/// transcript not recording the exchange, a stray turn started by `!`.
#[test]
#[cfg(unix)]
fn bang_runs_shell_command_without_model_turn() {
    let env = TestEnv::new("bash-mode");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&sess, env.timeout(), "REPL prompt should render");

    sess.send("! printf 'bash-mode-marker-%s' 42\r")
        .expect("send ! command");
    expect_output_rows(
        &sess,
        &["\u{23bf}  bash-mode-marker-42"],
        env.timeout(),
        "command stdout should render under the connector",
    );

    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    let budget = if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        env.timeout()
    };
    common::run_turn(&mut sess, &prompt, budget, "the follow-up prompt");

    if env.is_mock() {
        let bodies = env.captured_message_bodies();
        let screen = sess.render(common::screen_contents);
        assert_eq!(
            bodies.len(),
            1,
            "the ! line must not start a model turn; requests: {}\nPTY screen:\n{screen}",
            bodies.len()
        );
        let body = &bodies[0];
        assert!(
            body.contains("<bash-input>printf 'bash-mode-marker-%s' 42</bash-input>"),
            "request should carry the bash-input tag: {body}"
        );
        assert!(
            body.contains("<bash-stdout>bash-mode-marker-42</bash-stdout>"),
            "request should carry the bash-stdout tag: {body}"
        );
        assert!(
            body.contains("<local-command-caveat>"),
            "request should carry the local-command caveat: {body}"
        );
    }

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("REPL should exit cleanly after /exit; got {e:?}\nPTY screen:\n{screen}")
    });
    assert_eq!(exit, 0);
}

/// A failing command shows its stderr and exit code, and the REPL keeps
/// going: a later `!` still works. Guards the error branch and the
/// exit-status line.
#[test]
#[cfg(unix)]
fn bang_reports_stderr_and_exit_code() {
    let env = TestEnv::new("bash-mode-stderr");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&sess, env.timeout(), "REPL prompt should render");

    sess.send("! sh -c 'echo boom-on-stderr >&2; exit 3'\r")
        .expect("send failing ! command");
    expect_output_rows(
        &sess,
        &["\u{23bf}  boom-on-stderr", "exit 3"],
        env.timeout(),
        "stderr and the non-zero exit status should render",
    );

    sess.send("! printf 'still-alive'\r")
        .expect("send second ! command");
    expect_output_rows(
        &sess,
        &["\u{23bf}  still-alive"],
        env.timeout(),
        "REPL should still run ! commands after a failure",
    );

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("REPL should exit cleanly after /exit; got {e:?}\nPTY screen:\n{screen}")
    });
    assert_eq!(exit, 0);
}

/// `/history` lists what the human typed — `!` commands and prompts alike —
/// in the order they were typed, and the list survives `--resume`. Prompts are
/// recorded outside the turn that runs them, so this also guards that the
/// final save carries them.
#[test]
#[cfg(unix)]
fn history_lists_bang_commands_and_prompts_in_order() {
    let env = TestEnv::new("bash-mode-history");
    let budget = if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        env.timeout()
    };
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    common::expect_input_line_cleared(&sess, env.timeout(), "REPL prompt should render");
    sess.send("! printf 'history-first'\r")
        .expect("send ! command");
    expect_output_rows(
        &sess,
        &["\u{23bf}  history-first"],
        env.timeout(),
        "! output",
    );
    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    common::run_turn(&mut sess, &prompt, budget, "the prompt");
    expect_history(&mut sess, &env, "this session");
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    assert_eq!(sess.expect_eof().expect("REPL should exit"), 0);

    let mut resumed = env.spawn(&["--resume", "latest", "--permission-mode", "read-only"]);
    expect_history(&mut resumed, &env, "the resumed session");
    resumed.send("/exit\r").expect("send /exit");
    resumed.set_default_timeout(common::at_least(Duration::from_secs(30)));
    assert_eq!(resumed.expect_eof().expect("resumed REPL should exit"), 0);
}

/// `/history` shows exactly the `!` command, then the prompt.
#[cfg(unix)]
fn expect_history(sess: &mut pty_expect::PtySession, env: &TestEnv, context: &str) {
    common::expect_input_line_cleared(sess, env.timeout(), context);
    sess.send("/history\r").expect("send /history");
    let rows = |screen: &str| -> Vec<String> {
        let report = screen
            .rsplit_once("Prompt history")
            .map_or("", |(_, report)| report);
        report
            .lines()
            .map(str::trim)
            .filter(|line| {
                line.starts_with("1. ") || line.starts_with("2. ") || line.starts_with("3. ")
            })
            .map(str::to_owned)
            .collect()
    };
    let screen = common::expect_screen(
        sess,
        |screen| rows(screen).len() >= 2,
        env.timeout(),
        &format!("{context}: /history should list the typed lines"),
    );
    let rows = rows(&screen);
    assert!(
        rows.len() == 2
            && rows[0].ends_with("! printf 'history-first'")
            && rows[1].contains("What is 2+2?"),
        "{context}: /history should list the ! command, then the prompt: {rows:?}\n{screen}"
    );
    common::expect_input_line_cleared(sess, env.timeout(), context);
}

/// Wait until every `rows` entry is a whole rendered output row and the
/// editor is ready again. Whole rows, because the echoed command line also
/// contains the same words (`echo boom-on-stderr …; exit 3`).
#[cfg(unix)]
fn expect_output_rows(
    sess: &pty_expect::PtySession,
    rows: &[&str],
    budget: Duration,
    context: &str,
) {
    common::expect_screen(
        sess,
        |screen| {
            rows.iter()
                .all(|row| screen.lines().any(|line| line.trim() == *row))
        },
        budget,
        context,
    );
    common::expect_input_line_cleared(sess, budget, context);
}
