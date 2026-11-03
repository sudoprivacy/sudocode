//! Real PTY journey: queue input during a tool, recall it with Up, clear the
//! recalled draft, then exit while the tool is still running.

mod common;

use common::TestEnv;
use std::time::Duration;

const MARKER: &str = "MARKER_QUEUED_INPUT";

#[test]
fn up_arrow_on_empty_buffer_dequeues_last_queued_input() {
    let env = TestEnv::new("repl-async-up-dequeue");

    let mut sess = env.spawn_with_env(
        // danger-full-access so the mock's canned bash command actually runs —
        // pty_core_conversation's bash_interrupt_long_running case uses the
        // same flag; without it the tool call is denied and we never get the
        // "interrupt-start" output we key on below.
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );

    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "async input ready");

    // The same prompt drives the local scenario and optional live provider.
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r"))
        .expect("send long-running prompt");

    sess.expect("interrupt-start")
        .expect("long-running turn has started");

    // Observe the queued marker and empty draft before sending Up.
    sess.send(&format!("{MARKER}\t"))
        .expect("queue marker during running turn");

    common::expect_screen(
        &sess,
        |screen| {
            common::input_line_of(screen).is_empty()
                && screen
                    .lines()
                    .any(|line| line.contains("↳ queued:") && line.contains(MARKER))
        },
        common::DEFAULT_TIMEOUT,
        "marker is queued before Up",
    );

    sess.send("\x1b[A").expect("send Up-arrow key sequence");

    // The queue already emitted MARKER. Read the current input row so stale
    // PTY bytes cannot stand in for a successful dequeue. The queued chip must
    // also disappear: copying into the draft would submit the same input twice.
    common::expect_screen(
        &sess,
        |screen| {
            common::input_line_of(screen) == MARKER
                && !screen
                    .lines()
                    .any(|line| line.contains("↳ queued:") && line.contains(MARKER))
        },
        common::DEFAULT_TIMEOUT,
        "Up moves the queued marker into the input row and removes its queued chip",
    );

    // Clear the recalled draft first; otherwise /exit becomes steering input
    // appended to MARKER and the ignored EOF wait used to expire in full.
    sess.send("\x15").expect("clear recalled draft");
    // The running tool keeps animating. Assert this draft's contents without
    // asking the entire terminal to settle as an idle/resumed REPL would.
    common::expect_screen(
        &sess,
        |screen| screen.contains('❯') && common::input_line_of(screen).is_empty(),
        common::DEFAULT_TIMEOUT,
        "recalled draft cleared",
    );
    sess.send("/exit").expect("type /exit");
    common::expect_input_line(
        &sess,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "exit command rendered before submission",
    );
    sess.send("\r").expect("submit /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let exit = sess.expect_eof().unwrap_or_else(|error| {
        panic!(
            "async REPL should exit cleanly after recalling input: {error:?}\nPTY:\n{}",
            sess.render(common::screen_contents)
        )
    });
    assert_eq!(exit, 0, "async REPL clean exit");
}
