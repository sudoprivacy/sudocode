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
//! Reports a rate and dumps every lost round, so one CI run is informative
//! rather than a coin flip. Windows does not reproduce this; read it off macOS.

mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;

const ROUNDS: usize = 6;
const RENDER_BUDGET: Duration = Duration::from_secs(10);

/// What the arm waits for after Ctrl-C, before typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterCtrlC {
    /// The hint as it appears in the PTY byte stream — the real test's guard.
    StreamHint,
    /// The input buffer observably empty — the proposed guard.
    ClearedBuffer,
    /// Readiness re-established the way the test establishes it BEFORE Ctrl-C:
    /// a keystroke that renders, then cleared again.
    ///
    /// An empty buffer says the clear landed; it does not say keystrokes are
    /// being delivered to the input again. The key handler routes `Char` by
    /// `current_slot`, and Ctrl-C's `InputEvent::Abort` has the coordinator call
    /// `repl.ui.clear_question()` — an out-of-band slot reset. A character that
    /// renders is the only observation that covers every such state.
    ReProbe,
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
        AfterCtrlC::ReProbe => "reprobe",
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
        AfterCtrlC::ReProbe => {
            wait_for_cleared(&mut sess, round)?;
            // A keystroke that renders — the same observation the test uses
            // for readiness before Ctrl-C, repeated because Ctrl-C changed the
            // state it was proving.
            sess.send("~").expect("type re-probe");
            let (probed, seen, screen) = wait_for_input(&mut sess, "~");
            if !probed {
                return Err(format!(
                    "round {round}: input never accepted a keystroke after Ctrl-C \
                     (last saw {seen:?})\n{screen}"
                ));
            }
            sess.send("\x15").expect("Ctrl-U to clear the re-probe");
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

fn measure(arm: AfterCtrlC) -> usize {
    let mut lost = 0;
    for round in 0..ROUNDS {
        match run_round(arm, round) {
            Ok(()) => eprintln!("[{arm:?}] round {round}: /exit rendered"),
            Err(why) => {
                eprintln!("[{arm:?}] round {round}: LOST\n{why}");
                lost += 1;
            }
        }
    }
    eprintln!("[{arm:?}] {}/{ROUNDS} rounds rendered /exit", ROUNDS - lost);
    lost
}

#[test]
fn which_post_ctrlc_wait_stops_losing_keystrokes() {
    let stream_lost = measure(AfterCtrlC::StreamHint);
    let cleared_lost = measure(AfterCtrlC::ClearedBuffer);
    let reprobe_lost = measure(AfterCtrlC::ReProbe);

    eprintln!(
        "SUMMARY stream_hint={}/{ROUNDS} cleared_buffer={}/{ROUNDS} reprobe={}/{ROUNDS}",
        ROUNDS - stream_lost,
        ROUNDS - cleared_lost,
        ROUNDS - reprobe_lost
    );

    // Fail whenever any arm lost a round, so CI surfaces the dumps. The summary
    // line is what picks the guard: the previous run measured stream_hint 2/6
    // and cleared_buffer 4/6, so waiting for the clear helps and is not enough.
    assert!(
        stream_lost == 0 && cleared_lost == 0 && reprobe_lost == 0,
        "keystrokes lost: stream_hint dropped {stream_lost} of {ROUNDS}, \
         cleared_buffer dropped {cleared_lost} of {ROUNDS}, \
         reprobe dropped {reprobe_lost} of {ROUNDS}"
    );
}
