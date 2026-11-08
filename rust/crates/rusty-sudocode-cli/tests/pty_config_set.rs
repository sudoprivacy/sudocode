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
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));

    sess.expect("❯").expect("async REPL prompt");

    sess.send("/config set queue off\r")
        .expect("send config set queue off");
    sess.expect("queue: off").unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("should confirm queue off: {e}\nPTY screen:\n{screen}");
    });

    // Small delay so the coordinator loop processes TurnDone before the
    // next slash command arrives.
    std::thread::sleep(Duration::from_secs(1));

    sess.send("/config set queue on\r")
        .expect("send config set queue on");
    sess.expect("queue: on").unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("should confirm queue on: {e}\nPTY screen:\n{screen}");
    });

    sess.send("/exit\r").expect("send exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}

/// `/config set autoCompactEnabled false` persists to the global settings and
/// applies to the running session; a Claude Code setting scode ignores is
/// refused with that reason instead of being written.
#[test]
fn config_set_applies_auto_compaction_and_refuses_ignored_settings() {
    let env = common::TestEnv::new("config-set-auto-compact");
    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    let budget = common::at_least(Duration::from_secs(15));
    common::expect_input_line_cleared(&sess, budget, "REPL input ready");

    sess.send("/config set autoCompactEnabled false\r")
        .expect("send config set autoCompactEnabled");
    common::expect_screen(
        &sess,
        |screen| screen.contains("autoCompactEnabled = false"),
        budget,
        "should confirm the setting",
    );
    let settings = std::fs::read_to_string(env.config_home().join("settings.json"))
        .expect("global settings written");
    let settings: serde_json::Value = serde_json::from_str(&settings).expect("settings json");
    assert_eq!(settings["autoCompactEnabled"], false);

    common::expect_input_line_cleared(&sess, budget, "input ready after the set");
    sess.send("/config set verbose true\r")
        .expect("send config set verbose");
    common::expect_screen(
        &sess,
        |screen| common::screen_contains(screen, "Claude Code setting scode does not implement"),
        budget,
        "should refuse the ignored setting",
    );
    let settings =
        std::fs::read_to_string(env.config_home().join("settings.json")).expect("global settings");
    assert!(!settings.contains("verbose"), "{settings}");

    common::expect_input_line_cleared(&sess, budget, "input ready before exit");
    sess.send("/exit\r").expect("send exit");
    sess.set_default_timeout(budget);
    assert_eq!(sess.expect_eof().expect("exit"), 0);
}
