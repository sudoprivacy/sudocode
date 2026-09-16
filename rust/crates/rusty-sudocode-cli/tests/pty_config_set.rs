//! PTY test: `/config set` toggles work in async REPL mode.
//!
//! Exercises `handle_config_set` through the async REPL, verifying that
//! `/config set queue on|off` produces the expected output. This also
//! implicitly tests the `parse_on_off` helper.

mod common;

use std::time::Duration;

/// `/config set queue off` then `on` in async REPL.
#[test]
fn config_set_queue_toggles() {
    let env = common::TestEnv::new("config-set-queue");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(15));

    sess.expect("❯").expect("async REPL prompt");

    sess.send("/config set queue off\r")
        .expect("send config set queue off");
    sess.expect("queue: off").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should confirm queue off: {e}\nPTY screen:\n{screen}");
    });

    // Small delay so the coordinator loop processes TurnDone before the
    // next slash command arrives.
    std::thread::sleep(Duration::from_secs(1));

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
