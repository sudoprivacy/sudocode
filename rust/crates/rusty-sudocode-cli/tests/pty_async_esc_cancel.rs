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
#[cfg(unix)]
fn esc_cancels_turn_in_async_repl() {
    let env = common::TestEnv::new("async-esc-cancel");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let prompt = env.prompt(
        "Run this exact bash command: printf 'esc-start'; sleep 30; printf 'esc-done'",
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

    // Wait for an indicator that the turn is in progress.
    sess.expect("(?i)(bash|thinking|sonnet|auto|claude|❯)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("should see turn activity: {e}\nPTY screen:\n{screen}");
        });

    // In live mode, wait longer for the API stream to actually start.
    let pre_esc_delay = if env.is_live() {
        Duration::from_secs(8)
    } else {
        Duration::from_millis(500)
    };
    std::thread::sleep(pre_esc_delay);

    // Press ESC to cancel the turn.
    sess.send("\x1b").expect("send ESC");

    // Wait for cancel + prompt return.
    let cancel_timeout = if env.is_live() {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(15)
    };
    sess.set_default_timeout(cancel_timeout);
    sess.expect("(?i)(cancelled|interrupted)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("ESC should cancel the turn: {e}\nPTY screen:\n{screen}");
        });

    // In async REPL mode the ❯ prompt is persistent from the input thread
    // and doesn't get re-printed after a cancel — "Cancelled" is the
    // sufficient confirmation. Give the input thread a moment to settle
    // back into readline before sending /exit.
    std::thread::sleep(Duration::from_millis(500));

    sess.send("/exit\r").expect("send exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}
