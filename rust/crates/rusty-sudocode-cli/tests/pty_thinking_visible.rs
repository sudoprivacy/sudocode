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

/// Reasoning is written to the transcript under a `Thinking` header, and the
/// answer that follows it is still rendered.
#[test]
fn thinking_content_is_rendered_before_the_answer() {
    let env = TestEnv::new("thinking-visible");
    let prompt = env.prompt(
        "What is 17 times 23? Work it out step by step, then give the number.",
        "thinking_then_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only"]);
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // The header marks the dim block as reasoning. Without it dim text reads as
    // a faded answer.
    //
    // Matched on the ✻ glyph, not on the bare word "Thinking": the spinner's
    // own phase label says "Thinking" too, so a bare-word assertion passes even
    // when no reasoning is rendered at all. The glyph is emitted only by
    // `cli::format::thinking_header`.
    sess.expect("✻ Thinking").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("thinking header should render: {e}\nPTY:\n{screen}");
    });

    if env.is_mock() {
        // Both halves of the canned reasoning must survive. The second one is
        // the real gate: it arrives as a delta that starts mid-sentence on a
        // line the previous delta opened, which is exactly where a per-delta
        // spinner clear used to erase what had been drawn.
        sess.expect("Reasoning step one").unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("first reasoning delta should render: {e}\nPTY:\n{screen}");
        });
        sess.expect("step two continues the same line")
            .unwrap_or_else(|e| {
                let screen = sess.render(|s| s.contents());
                panic!(
                    "a reasoning delta that resumes mid-line must not be erased \
                     by the spinner clear: {e}\nPTY:\n{screen}"
                );
            });
        sess.expect("The answer follows the reasoning")
            .unwrap_or_else(|e| {
                let screen = sess.render(|s| s.contents());
                panic!("the answer must still render after thinking: {e}\nPTY:\n{screen}");
            });
    } else {
        // Live mode cannot assert on the model's wording, only that reasoning
        // produced visible text and the answer arrived.
        sess.expect("391").unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("the answer must still render after thinking: {e}\nPTY:\n{screen}");
        });
    }

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should return to prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
