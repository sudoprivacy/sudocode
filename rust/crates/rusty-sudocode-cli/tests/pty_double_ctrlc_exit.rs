//! PTY test: double Ctrl-C within 800ms exits the process.
//!
//! Covers both the sync REPL (queue=off) and async REPL (queue=queue) paths.
//! The sync REPL uses `HookAbortMonitor`'s raw-mode stdin thread for the
//! double-press check; the async REPL routes through `cancel.rs` via the
//! `abort_hook` in `LineEditor`. Both should produce a clean exit code 0.

mod common;

use std::time::Duration;

/// Sync REPL: two rapid Ctrl-C on an empty prompt exits the process.
#[test]
fn double_ctrlc_exits_sync_repl() {
    let env = common::TestEnv::new("double-ctrlc-sync");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "off")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").expect("REPL prompt");

    // First Ctrl-C — clears prompt, shows "Press Ctrl-C again to exit".
    sess.send("\x03").expect("send first Ctrl-C");
    sess.expect("Press Ctrl-C again").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see exit hint after first Ctrl-C: {e}\nPTY screen:\n{screen}");
    });

    // Small delay so readline() is re-entered before the second SIGINT.
    std::thread::sleep(Duration::from_millis(200));

    // Second Ctrl-C within 800ms — should exit.
    sess.send("\x03").expect("send second Ctrl-C");

    sess.set_default_timeout(Duration::from_secs(10));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("double Ctrl-C should exit (sync): {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "sync REPL should exit cleanly on double Ctrl-C");
}

/// Async REPL: two rapid Ctrl-C on an empty prompt exits the process.
#[test]
fn double_ctrlc_exits_async_repl() {
    let env = common::TestEnv::new("double-ctrlc-async");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").expect("async REPL prompt");

    // First Ctrl-C — cancels (no-op since no turn), records timestamp.
    sess.send("\x03").expect("send first Ctrl-C");
    sess.expect("Press Ctrl-C again").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see exit hint after first Ctrl-C: {e}\nPTY screen:\n{screen}");
    });

    // Small delay so readline() is fully re-entered before the second
    // SIGINT arrives — rustyline only catches SIGINT while readline() is
    // active; between calls the default handler may be in effect.
    std::thread::sleep(Duration::from_millis(200));

    // Second Ctrl-C within 800ms — should exit.
    sess.send("\x03").expect("send second Ctrl-C");

    sess.set_default_timeout(Duration::from_secs(10));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("double Ctrl-C should exit (async): {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "async REPL should exit cleanly on double Ctrl-C");
}
