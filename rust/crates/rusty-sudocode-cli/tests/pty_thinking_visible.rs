//! Extended thinking is shown, not swallowed.
//!
//! The renderer used to drop every `ThinkingDelta` on the floor and raise only
//! the spinner's "Reasoning…" cue, so a session with `thinking` on (the default)
//! paid for reasoning tokens and displayed none of them. These gates cover the
//! shape that regression would take again.
//!
//! Runs under a PTY rather than against `--print` redirected to a file on
//! purpose. The first version of this feature looked correct in a captured file
//! and was broken on a terminal: it paused the spinner once per delta, and the
//! spinner's `\r` + erase-line then landed mid-line and wiped the reasoning
//! already drawn on that row. A redirect records the escape and shows the text
//! anyway; only a terminal emulator applies it.

use std::time::Duration;

mod common;
use common::TestEnv;

/// Reasoning is written to the transcript led by a dim ✻, with no header row
/// (the live spinner already says "Thinking"), and the answer that follows it
/// still gets the response's •.
#[test]
fn thinking_content_is_rendered_before_the_answer() {
    let env = TestEnv::new_mock("thinking-visible");
    let prompt = env.prompt(
        "What is 17 times 23? Work it out step by step, then give the number.",
        "thinking_then_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    let marker = common::turn_status_marker(&sess);
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.expect("Reasoning step one").unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("first reasoning delta should render: {e}\nPTY:\n{screen}");
    });
    sess.expect("step two continues the same line")
        .unwrap_or_else(|e| {
            let screen = sess.render(common::screen_contents);
            panic!(
                "a reasoning delta that resumes mid-line must not be erased \
                 by the spinner clear: {e}\nPTY:\n{screen}"
            );
        });
    sess.expect("The answer follows the reasoning")
        .unwrap_or_else(|e| {
            let screen = sess.render(common::screen_contents);
            panic!("the answer must still render after thinking: {e}\nPTY:\n{screen}");
        });
    // Leads are styled apart from their text, so they are checked on the
    // rendered screen rather than in the byte stream. The ✻ marks the dim
    // block as reasoning (without it dim text reads as a faded answer), and
    // the answer keeps the turn's •.
    let screen = sess.render(common::screen_contents);
    assert!(
        !screen.contains("Thinking…"),
        "no header row repeats the spinner's label: {screen}"
    );
    let reasoning: Vec<_> = screen
        .lines()
        .filter(|line| line.contains("Reasoning step"))
        .collect();
    assert!(
        reasoning.len() == 2
            && reasoning[0].starts_with("✻ Reasoning step one")
            && reasoning[1].starts_with("  Reasoning step two"),
        "first row led by ✻, the next indented under it: {reasoning:?}"
    );
    assert!(
        screen
            .lines()
            .any(|line| line.starts_with("• The answer follows the reasoning")),
        "the answer keeps the response's •: {screen}"
    );

    common::expect_turn_complete_after(
        &sess,
        &marker,
        common::at_least(Duration::from_secs(30)),
        "should return to prompt",
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(common::screen_contents);
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
