//! PTY probe for the queue overlay: an input submitted WHILE a turn is running
//! must land in the transient queue overlay (a DIM `↳ queued:` line above the
//! prompt), NOT be echoed into scrollback as `❯ ...` — echoing it there is what
//! made a queued input look like it was sent (the confusing UX this fixes). The
//! coordinator defers the `❯` echo until the item actually flushes at the turn
//! boundary.
//!
//! Uses the `bash_interrupt_long_running` scenario (`printf 'interrupt-start';
//! sleep 30`) so the in-flight window is long enough for the 80 ms tick to
//! render the overlay. Cleaned up by `/exit` (SIGTERM to the sleep).

mod common;

use common::TestEnv;
use std::time::Duration;

const MARKER: &str = "QUEUE_OVERLAY_MARKER";

#[test]
fn input_submitted_during_a_turn_shows_in_the_queue_overlay() {
    let env = TestEnv::new("queue-overlay");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(30));
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("async REPL initial prompt");

    // Start a long turn so the next submit goes down the during-turn arm.
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("send long prompt");
    sess.expect("interrupt-start")
        .expect("bash tool should print interrupt-start before sleeping");

    // Submit a marker DURING the running turn. The coordinator queues it.
    sess.send(&format!("{MARKER}\r"))
        .expect("submit marker during running turn");

    // The marker must appear in the queue overlay as a `↳ queued:` line — it is
    // held there, not committed to scrollback. Seeing this proves the overlay
    // renders the coordinator's queue.
    sess.expect(&format!("↳ queued: {MARKER}"))
        .expect("queued input should render in the queue overlay, not scrollback");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
