//! PTY test: double Ctrl-C within 800ms exits the process with code 0.
//!
//! The REPL's Ctrl-C handling records the first press through `cancel.rs`
//! and exits on a second press within the window.

mod common;

use std::time::Duration;

/// Two rapid Ctrl-C on an empty prompt exit the process.
#[test]
fn double_ctrlc_exits_repl() {
    let env = common::TestEnv::new("double-ctrlc");

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(common::at_least(Duration::from_secs(10)));

    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "REPL input ready");

    // First Ctrl-C — cancels (no-op since no turn), records timestamp.
    sess.send("\x03").expect("send first Ctrl-C");
    sess.expect("Press Ctrl-C again").unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("should see exit hint after first Ctrl-C: {e}\nPTY screen:\n{screen}");
    });

    // Keep the second press inside the window but after the first is handled.
    std::thread::sleep(Duration::from_millis(200));

    // Second Ctrl-C within 800ms — should exit.
    sess.send("\x03").expect("send second Ctrl-C");

    sess.set_default_timeout(common::at_least(Duration::from_secs(10)));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("double Ctrl-C should exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "the REPL should exit cleanly on double Ctrl-C");
}
