//! PTY test: ESC cancels a running turn in the async REPL.
//!
//! Verifies that the `EscCancelHandler` wired in commit 3 (ESC bound as a
//! rustyline `ConditionalEventHandler`) successfully aborts a turn and returns
//! to the `❯` prompt. Same contract as `pty_cancel::esc_cancels_turn_in_repl`
//! but exercised through the async REPL path (queue mode).

mod common;

use std::fs;
use std::time::Duration;

/// Submit a long-running bash prompt under async REPL (queue mode), press ESC
/// mid-turn, verify the turn is cancelled and the prompt returns.
#[test]
fn esc_cancels_turn_in_async_repl() {
    let env = common::TestEnv::new("async-esc-cancel");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let prompt = env.prompt(
        "Run this exact bash command: printf 'ready' > cancel-ready; printf 'esc-start'; sleep 30; printf 'esc-done'",
        "bash_interrupt_long_running",
    );
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    let timeout = if env.is_live() {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(15)
    };
    sess.set_default_timeout(timeout);

    sess.expect("❯").expect("async REPL prompt");

    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // Only the running Bash command can create this marker. The banner and
    // echoed prompt can match before the abort monitor is armed, and stdout
    // may remain buffered until the command finishes.
    common::expect_screen(
        &sess,
        |_| fs::read_to_string(root.join("cancel-ready")).is_ok_and(|s| s == "ready"),
        if env.is_live() {
            common::LIVE_TURN_BUDGET
        } else {
            env.timeout()
        },
        "Bash must start before cancellation",
    );

    // Press ESC to cancel the turn.
    sess.send("\x1b").expect("send ESC");

    // A persistent or replayed prompt is not evidence that ESC cancelled.
    common::expect_screen(
        &sess,
        |screen| screen.to_lowercase().contains("cancelled"),
        env.timeout(),
        "ESC must cancel the running turn",
    );
    common::expect_input_line_cleared(&sess, env.timeout(), "input ready after cancellation");
    sess.send("/exit").expect("type exit");
    common::expect_input_line(&sess, "/exit", env.timeout(), "exit entered");
    sess.send("\r").expect("submit exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}
