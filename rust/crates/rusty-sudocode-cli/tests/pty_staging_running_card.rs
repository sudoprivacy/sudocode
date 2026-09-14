//! PTY probe: does the staging overlay actually render a *running* tool card
//! during a long-running tool call? The staging design shows an in-flight call
//! as a card in the overlay (above the prompt) until its ToolResult arrives.
//! For a fast tool the running phase is sub-frame and invisible; this uses the
//! `bash_interrupt_long_running` scenario (`printf 'interrupt-start'; sleep 30`)
//! so the in-flight window is ~30 s — plenty for the 80 ms tick to render it.
//!
//! Asserts: while the tool is running (after `interrupt-start`, before any
//! result), an L-frame card header (`╭─`) referencing the running Bash call is
//! on screen. Cleaned up by `/exit` (SIGTERM to the sleep).

mod common;

use common::TestEnv;
use std::time::Duration;

#[test]
fn staging_overlay_shows_a_running_card_during_a_long_tool() {
    let env = TestEnv::new("staging-running-card");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(30));
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("async REPL initial prompt");

    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("send long prompt");

    // Tool has started (bash printed this before the sleep) — turn is in-flight.
    sess.expect("interrupt-start")
        .expect("bash tool should print interrupt-start before sleeping");

    // While the tool is still sleeping, the staging overlay must show a running
    // L-frame card for the Bash call. `╭─` is the card header glyph; it is not
    // in the echoed prompt.
    sess.expect("╭─").expect(
        "a running tool card should be visible in the staging overlay during the tool call",
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}
