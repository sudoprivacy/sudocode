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
    sess.expect("❯").expect("REPL prompt should render");

    sess.send("! printf 'bash-mode-marker-%s' 42\r")
        .expect("send ! command");
    sess.expect("\u{23bf}")
        .expect("bash-mode output connector should render");
    sess.expect("bash-mode-marker-42")
        .expect("command stdout should render under the connector");
    sess.expect("❯")
        .expect("prompt should return after the ! command");

    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    sess.send(&format!("{prompt}\r"))
        .expect("send follow-up prompt");
    if env.is_mock() {
        sess.expect("4").expect("mock answer should render");
    } else {
        sess.expect("turn 1").expect("live turn should complete");
    }
    sess.expect("❯")
        .expect("prompt should return after the model turn");

    if env.is_mock() {
        let bodies = env.captured_message_bodies();
        let screen = sess.render(|s| s.contents());
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
    sess.set_default_timeout(Duration::from_secs(30));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
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
    sess.expect("❯").expect("REPL prompt should render");

    sess.send("! sh -c 'echo boom-on-stderr >&2; exit 3'\r")
        .expect("send failing ! command");
    sess.expect("boom-on-stderr").expect("stderr should render");
    sess.expect("exit_code:3")
        .expect("non-zero exit status should render");
    sess.expect("❯").expect("prompt should return");

    sess.send("! printf 'still-alive'\r")
        .expect("send second ! command");
    sess.expect("still-alive")
        .expect("REPL should still run ! commands after a failure");
    sess.expect("❯").expect("prompt should return");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(30));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL should exit cleanly after /exit; got {e:?}\nPTY screen:\n{screen}")
    });
    assert_eq!(exit, 0);
}
