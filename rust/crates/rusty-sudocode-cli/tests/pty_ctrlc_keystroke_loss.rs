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
//! Ctrl-C's handler does both of these in one pass (`repl_ui.rs`):
//!
//! ```ignore
//! footer_hint.set(Some((hint_msg, Instant::now() + Duration::from_secs(3))));
//! input_value.set(String::new());
//! ```
//!
//! If the frame carrying the hint can reach the PTY before the cleared buffer
//! is observable, then a test that waits on the hint and types immediately has
//! its characters inserted by `TextInput` and then wiped. Nothing appears.
//!
//! ## The experiment
//!
//! Two arms, differing only in what they wait for after Ctrl-C before typing:
//!
//! | arm | waits for |
//! |---|---|
//! | `stream-hint` | the hint in the PTY BYTE STREAM — what the real test does |
//! | `cleared-buffer` | the input buffer observably EMPTY — the proposed guard |
//!
//! A lossy `stream-hint` arm beside a clean `cleared-buffer` arm confirms the
//! mechanism and the fix together. Both clean says the mechanism is wrong and
//! the loss is elsewhere. Both lossy says waiting for the clear is not enough.
//!
//! ## Measured so far
//!
//! | arm | macOS | Windows |
//! |---|---|---|
//! | `stream-hint` | 4/12 rendered | 22/24 rendered |
//! | `cleared-buffer` | 10/12 rendered | 48/48 rendered |
//!
//! Two things worth recording. The loss is NOT macOS-only — Windows reproduces
//! it too, just rarely enough that the single-round real test almost always
//! passes there, which is why it read as a macOS problem. And `cleared-buffer`
//! has not lost a Windows round yet; both of its macOS losses came from the
//! first run, before the arms were separated from the earlier Ctrl-C/no-Ctrl-C
//! comparison.
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
const ROUNDS: usize = 24;
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

fn run_round(arm: AfterCtrlC, round: usize) -> Result<(), String> {
    let label = match arm {
        AfterCtrlC::StreamHint => "stream",
        AfterCtrlC::ClearedBuffer => "cleared",
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

    eprintln!(
        "SUMMARY stream_hint={}/{CONTROL_ROUNDS} cleared_buffer={}/{ROUNDS}",
        CONTROL_ROUNDS - stream_lost,
        ROUNDS - cleared_lost
    );

    // Fail whenever either arm lost a round, so CI surfaces the dumps. Two runs
    // put the control at 4 of 6 lost each time; the arm under test came back
    // 4/6 then 6/6, which is why it now gets twelve rounds instead of six.
    assert!(
        stream_lost == 0 && cleared_lost == 0,
        "keystrokes lost: stream_hint dropped {stream_lost} of {CONTROL_ROUNDS} (control), \
         cleared_buffer dropped {cleared_lost} of {ROUNDS}"
    );
}
