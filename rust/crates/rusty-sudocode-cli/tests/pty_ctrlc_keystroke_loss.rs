//! TEMPORARY diagnostic — DO NOT MERGE.
//!
//! `iocraft_repl_ctrlc_hint_in_footer` has failed intermittently on macOS CI
//! since at least the #603 merge, always at the same step: after Ctrl-C the
//! test types `/exit` and the input line never shows it. The captured failure
//! had the prompt marker on screen, the hint already expired, and `/exit`
//! absent for the whole 10s budget — the characters were not late, they were
//! never there.
//!
//! ## The mechanism under test
//!
//! Ctrl-C's handler performs three real `State::set` calls in one pass
//! (`repl_ui.rs`):
//!
//! ```ignore
//! last_ctrlc.set(Some(now));
//! footer_hint.set(Some((hint_msg, Instant::now() + Duration::from_secs(3))));
//! input_value.set(String::new());
//! ```
//!
//! Per this REPL's own render model — spelled out in the comment above the
//! spinner block and guarded by `iocraft_repl_keyboard_input_not_frozen` — a
//! `State::set` resolves `component.wait()`, and doing so repeatedly starves
//! `term.wait()`, which is where key events get distributed. Keystrokes
//! arriving inside that window are dropped outright: the buffer reads empty and
//! the characters never render, which is the symptom exactly.
//!
//! Note the "only set when the value changed" discipline already in `repl_ui`
//! does not apply here. All three sets are genuine changes.
//!
//! ## The experiment
//!
//! Three arms, differing only in what they wait for after Ctrl-C before typing:
//!
//! | arm | waits for |
//! |---|---|
//! | `stream-hint` | the hint in the PTY BYTE STREAM — the real test's guard |
//! | `cleared-buffer` | the input buffer observably EMPTY |
//! | `quiesced` | cleared, AND the screen unchanged across three polls |
//!
//! `quiesced` clean while `cleared-buffer` loses ⇒ the window is the
//! post-Ctrl-C render churn, and the product fix is to coalesce those three
//! mutations into one. Both lossy ⇒ the loss is not about render churn.
//!
//! ## Measured so far
//!
//! | arm | macOS lost | Windows lost |
//! |---|---|---|
//! | `stream-hint` | 8/20 (40%) | 2/24 |
//! | `cleared-buffer` | 5/36 (14%) | 0/48 |
//!
//! Three things worth recording. The loss is NOT macOS-only — Windows
//! reproduces it, just rarely enough that the single-round real test almost
//! always passes there, which is why it read as a macOS problem. The rate
//! swings hard with runner load: the control came back 4-of-6 lost twice and
//! 0-of-8 once from identical code, so no single run discriminates and only a
//! within-run comparison means anything. And waiting for the clear reduces the
//! loss without removing it, so it is a mitigation, not a fix.
//!
//! Reports a rate and dumps every lost round, so one CI run is informative
//! rather than a coin flip.

mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;

/// Positive control: enough rounds to show this run can reproduce the loss at
/// all. Measured at 4 of 6 lost, twice, so 4 rounds losing none would itself be
/// the surprise.
const CONTROL_ROUNDS: usize = 8;

/// The arm under test gets the rounds, because 6 was too few to tell 4/6 from
/// 6/6 — the first two runs disagreed by exactly that much.
const ROUNDS: usize = 12;
const RENDER_BUDGET: Duration = Duration::from_secs(10);

/// What the arm waits for after Ctrl-C, before typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterCtrlC {
    /// The hint as it appears in the PTY byte stream — the real test's guard.
    StreamHint,
    /// The input buffer observably empty — the proposed guard.
    ///
    /// A re-probe arm was measured alongside this one — clear, then type a
    /// character and wait for it to RENDER, then clear again, on the theory that
    /// an empty buffer does not prove keystrokes are being delivered (the key
    /// handler routes `Char` by `current_slot`, and Ctrl-C's `InputEvent::Abort`
    /// has the coordinator reset slots out of band). It came back 5/6 against
    /// this arm's 6/6, i.e. no better for twice the work, so the slot-routing
    /// theory is not the residual and the arm is gone.
    ClearedBuffer,
    /// Cleared, and then the screen unchanged across several polls — the render
    /// loop observably QUIESCED.
    ///
    /// Tests the mechanism directly. Ctrl-C's handler performs three real
    /// `State::set` calls in one pass (`last_ctrlc`, `footer_hint`,
    /// `input_value`), and per this REPL's own render model each one resolves
    /// `component.wait()`; a run of them starves `term.wait()`, which is where
    /// key events get distributed. Keystrokes arriving inside that window are
    /// dropped outright — buffer empty, characters never rendered, which is the
    /// observed symptom exactly.
    ///
    /// Clean here while `ClearedBuffer` still loses ⇒ the window is the
    /// post-Ctrl-C render churn, and the product fix is to coalesce those three
    /// mutations into one. Still lossy ⇒ the loss is not about render churn.
    ///
    /// The "only set when the value changed" discipline already in `repl_ui`
    /// cannot help either way: all three of these sets are genuine changes.
    ClearedThenQuiesced,
}

/// The text after the last prompt marker on the lowest row carrying one.
///
/// Matches the marker anywhere on the row: when the chrome rule fills the
/// terminal width exactly the input shares that row, so it reads
/// `────…────❯ abc!` and does not start with the marker.
fn input_line(sess: &mut PtySession) -> String {
    sess.render(|s| {
        s.contents()
            .lines()
            .rev()
            .find_map(|line| {
                let marker = line.rfind('\u{276f}')?;
                Some(line[marker + '\u{276f}'.len_utf8()..].trim().to_string())
            })
            .unwrap_or_default()
    })
}

/// Poll the prompt row for `needle`. Returns whether it showed, what was last
/// seen, and the final screen — the facts a lost round needs to be diagnosed.
fn wait_for_input(sess: &mut PtySession, needle: &str) -> (bool, String, String) {
    let deadline = Instant::now() + RENDER_BUDGET;
    loop {
        let line = input_line(sess);
        if line.contains(needle) {
            return (true, line, String::new());
        }
        if Instant::now() >= deadline {
            let screen = sess.render(|s| s.contents());
            return (false, line, screen);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Block until the input buffer reads empty.
fn wait_for_cleared(sess: &mut PtySession, round: usize) -> Result<(), String> {
    let deadline = Instant::now() + RENDER_BUDGET;
    loop {
        if input_line(sess).is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let line = input_line(sess);
            let screen = sess.render(|s| s.contents());
            return Err(format!(
                "round {round}: buffer still held {line:?}\n{screen}"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Block until the screen stops changing — the render loop has nothing left to
/// flush, so it is no longer resolving `component.wait()` ahead of
/// `term.wait()`.
///
/// Three identical consecutive polls rather than one comparison: a single pair
/// can match across the gap between two frames of the same churn.
fn wait_for_quiescence(sess: &mut PtySession, round: usize) -> Result<(), String> {
    const STABLE_POLLS: usize = 3;
    let deadline = Instant::now() + RENDER_BUDGET;
    let mut previous = sess.render(|s| s.contents());
    let mut stable = 0;
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let current = sess.render(|s| s.contents());
        if current == previous {
            stable += 1;
            if stable >= STABLE_POLLS {
                return Ok(());
            }
        } else {
            stable = 0;
            previous = current;
        }
        if Instant::now() >= deadline {
            return Err(format!("round {round}: screen never stopped changing"));
        }
    }
}

fn run_round(arm: AfterCtrlC, round: usize) -> Result<(), String> {
    let label = match arm {
        AfterCtrlC::StreamHint => "stream",
        AfterCtrlC::ClearedBuffer => "cleared",
        AfterCtrlC::ClearedThenQuiesced => "quiesced",
    };
    let env = TestEnv::new(&format!("ctrlc-{label}-{round}"));
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(RENDER_BUDGET);

    // Readiness is identical in both arms so it cannot explain a difference: a
    // keystroke that RENDERS proves iocraft owns the keyboard, which matters
    // because until it does, ^C is still a terminal signal.
    sess.send("~probe~").expect("type readiness probe");
    let (probed, seen, screen) = wait_for_input(&mut sess, "~probe~");
    if !probed {
        return Err(format!(
            "round {round}: readiness probe never rendered (last saw {seen:?})\n{screen}"
        ));
    }

    sess.send("\x03").expect("send Ctrl-C");

    match arm {
        AfterCtrlC::StreamHint => {
            // The real test's guard, verbatim: match the hint in the byte
            // stream and type at once.
            sess.expect("Press Ctrl-C again to exit").map_err(|e| {
                let screen = sess.render(|s| s.contents());
                format!("round {round}: hint never reached the stream: {e}\n{screen}")
            })?;
        }
        AfterCtrlC::ClearedBuffer => {
            wait_for_cleared(&mut sess, round)?;
        }
        AfterCtrlC::ClearedThenQuiesced => {
            wait_for_cleared(&mut sess, round)?;
            wait_for_quiescence(&mut sess, round)?;
        }
    }

    sess.send("/exit").expect("type /exit");
    let (shown, seen, screen) = wait_for_input(&mut sess, "/exit");
    if !shown {
        return Err(format!(
            "round {round}: /exit never rendered (last saw {seen:?})\n{screen}"
        ));
    }
    Ok(())
}

fn measure(arm: AfterCtrlC, rounds: usize) -> usize {
    let mut lost = 0;
    for round in 0..rounds {
        match run_round(arm, round) {
            Ok(()) => eprintln!("[{arm:?}] round {round}: /exit rendered"),
            Err(why) => {
                eprintln!("[{arm:?}] round {round}: LOST\n{why}");
                lost += 1;
            }
        }
    }
    eprintln!("[{arm:?}] {}/{rounds} rounds rendered /exit", rounds - lost);
    lost
}

#[test]
fn which_post_ctrlc_wait_stops_losing_keystrokes() {
    // Control first, so a run that cannot reproduce the loss at all says so
    // before the arm under test is credited with anything.
    let stream_lost = measure(AfterCtrlC::StreamHint, CONTROL_ROUNDS);
    let cleared_lost = measure(AfterCtrlC::ClearedBuffer, ROUNDS);
    let quiesced_lost = measure(AfterCtrlC::ClearedThenQuiesced, ROUNDS);

    eprintln!(
        "SUMMARY stream_hint={}/{CONTROL_ROUNDS} cleared_buffer={}/{ROUNDS} quiesced={}/{ROUNDS}",
        CONTROL_ROUNDS - stream_lost,
        ROUNDS - cleared_lost,
        ROUNDS - quiesced_lost
    );

    // Fail whenever any arm lost a round, so CI surfaces the dumps. The control
    // has come back 4-of-6 lost twice and 0-of-8 once, so it establishes only
    // whether a given run can reproduce at all; the comparison that matters is
    // `cleared` against `quiesced` within one run.
    assert!(
        stream_lost == 0 && cleared_lost == 0 && quiesced_lost == 0,
        "keystrokes lost: stream_hint dropped {stream_lost} of {CONTROL_ROUNDS} (control), \
         cleared_buffer dropped {cleared_lost} of {ROUNDS}, \
         quiesced dropped {quiesced_lost} of {ROUNDS}"
    );
}
