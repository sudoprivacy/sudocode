//! PTY tests for ESC / Ctrl-C cancel during streaming in REPL mode.
//!
//! These tests verify the direct-termios abort monitor (bypassing crossterm's
//! event system) works correctly after rustyline has toggled raw mode on the
//! main thread.

mod common;

use std::fs;
use std::time::Duration;

/// ESC during a tool call in REPL mode cancels the turn and returns to prompt.
///
/// Uses `bash_interrupt_long_running` (mock: `sleep 30` tool call) to create
/// a window where ESC can be pressed mid-execution. In live mode the model
/// may call bash or stream text — either way, ESC should cancel and return
/// to the `❯` prompt.
#[test]
#[cfg(unix)]
fn esc_cancels_turn_in_repl() {
    let env = common::TestEnv::new("esc-repl");
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
        &[("EDITOR", "true")],
    );
    let timeout = if env.is_live() {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(15)
    };
    sess.set_default_timeout(timeout);

    sess.expect("❯").expect("REPL prompt");

    // Submit the prompt — the tool call takes a while (sleep 30).
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // Wait for an indicator that the turn is in progress.
    // Mock: "bash" tool call appears. Live: could be thinking indicator
    // or model name or the tool call.
    sess.expect("(?i)(bash|thinking|sonnet|auto|claude|❯)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("should see turn activity: {e}\nPTY screen:\n{screen}");
        });

    // Small delay to ensure the abort monitor is polling.
    std::thread::sleep(Duration::from_millis(500));

    // Press ESC to cancel the turn.
    sess.send("\x1b").expect("send ESC");

    // Wait for the cancel to complete and REPL prompt to return.
    // The cancellation marker or prompt confirms the turn was aborted.
    sess.set_default_timeout(Duration::from_secs(15));
    sess.expect("(?i)(cancelled|interrupted|❯)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("ESC should cancel the turn: {e}\nPTY screen:\n{screen}");
        });

    // Wait for the REPL prompt specifically.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should return to prompt after cancel: {e}\nPTY screen:\n{screen}");
    });

    // Small delay to ensure the REPL is fully ready for input.
    std::thread::sleep(Duration::from_millis(300));

    sess.send("/exit\r").expect("send exit");
    sess.set_default_timeout(Duration::from_secs(10));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

/// Resume a session, press ↑ — previous prompt should appear from history.
///
/// Verifies that `run_repl_loop` seeds the rustyline history from user
/// messages when entering a resumed session.
#[test]
fn resume_seeds_history_for_up_arrow() {
    let env = common::TestEnv::new("resume-hist");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let prompt = env.prompt("say hello world", "single_turn_text");

    // Session 1: submit a prompt, then exit.
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(15));
    sess.expect("❯").expect("REPL prompt");
    sess.send(&format!("{prompt}\r")).expect("send prompt");
    sess.expect("❯").expect("second prompt after turn");
    sess.send("/exit\r").expect("send exit");
    sess.expect_eof().expect("clean exit");

    // Session 2: resume latest, then press ↑ on empty prompt.
    let mut sess2 = env.spawn_with_env(
        &["--resume", "latest", "--permission-mode", "read-only"],
        &[("EDITOR", "true")],
    );
    sess2.set_default_timeout(Duration::from_secs(15));

    // Wait for the resumed REPL prompt.
    sess2.expect("❯").unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("should see prompt after resume: {e}\nPTY screen:\n{screen}");
    });

    // Press ↑ on empty prompt — should show the previous user prompt from history.
    sess2.send("\x1b[A").expect("send Up arrow");
    std::thread::sleep(Duration::from_millis(500));

    let screen = sess2.render(|s| s.contents());
    assert!(
        screen.contains("say hello world") || screen.contains("single_turn_text"),
        "↑ after resume should recall the previous prompt from history.\n\
         PTY screen:\n{screen}",
    );

    // Clean exit.
    sess2.send("\x15").expect("Ctrl-U clear");
    std::thread::sleep(Duration::from_millis(100));
    sess2.send("/exit\r").expect("send exit");
    let exit = sess2.expect_eof().unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}
