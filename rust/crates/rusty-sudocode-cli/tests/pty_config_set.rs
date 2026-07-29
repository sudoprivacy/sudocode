//! PTY test: `/config set` toggles work in async REPL mode.
//!
//! Exercises `handle_config_set` through the async REPL, verifying that
//! `/config set auto-interrupt on|off` and `/config set queue on|off` produce
//! the expected output. This also implicitly tests the `parse_on_off` helper.

mod common;

use std::time::Duration;

/// `/config set auto-interrupt on` then `off` in async REPL.
#[test]
fn config_set_auto_interrupt_toggles() {
    let env = common::TestEnv::new("config-set-interrupt");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").expect("async REPL prompt");

    sess.send("/config set auto-interrupt on\r")
        .expect("send config set auto-interrupt on");
    sess.expect("auto-interrupt: on").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should confirm auto-interrupt on: {e}\nPTY screen:\n{screen}");
    });

    sess.send("/config set auto-interrupt off\r")
        .expect("send config set auto-interrupt off");
    sess.expect("auto-interrupt: off").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should confirm auto-interrupt off: {e}\nPTY screen:\n{screen}");
    });

    sess.send("/exit\r").expect("send exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

/// `/config set queue off` then `on` in async REPL.
#[test]
fn config_set_queue_toggles() {
    let env = common::TestEnv::new("config-set-queue");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").expect("async REPL prompt");

    sess.send("/config set queue off\r")
        .expect("send config set queue off");
    sess.expect("queue: off").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should confirm queue off: {e}\nPTY screen:\n{screen}");
    });

    sess.send("/config set queue on\r")
        .expect("send config set queue on");
    sess.expect("queue: on").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should confirm queue on: {e}\nPTY screen:\n{screen}");
    });

    sess.send("/exit\r").expect("send exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}
