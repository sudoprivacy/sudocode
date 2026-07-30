//! PTY test: input chrome (separator + footer) renders in async REPL.
//!
//! Verifies that the input chrome (top separator, prompt, bottom separator,
//! footer) renders correctly in both sync and async REPL modes, and that
//! the echo block replaces the prompt after submit.

mod common;

use std::time::Duration;

/// Async REPL: separator lines and footer render around the prompt.
#[test]
fn async_repl_renders_input_chrome() {
    let env = common::TestEnv::new("input-chrome-async");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    // The separator line (─) should appear before the prompt.
    sess.expect("─").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see separator in async REPL: {e}\nPTY screen:\n{screen}");
    });

    // The footer hint should appear.
    sess.expect("Tab for /commands").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see footer hint in async REPL: {e}\nPTY screen:\n{screen}");
    });

    // Submit a prompt and verify the echo block appears (› prefix).
    let prompt = env.prompt("What is 2+2? Answer briefly.", "single_turn_text");
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.expect("›").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see echo block (›) after submit: {e}\nPTY screen:\n{screen}");
    });

    // Clean exit.
    sess.expect("4").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see LLM answer: {e}\nPTY screen:\n{screen}");
    });

    std::thread::sleep(Duration::from_millis(500));
    sess.send("/exit\r").expect("send exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}
