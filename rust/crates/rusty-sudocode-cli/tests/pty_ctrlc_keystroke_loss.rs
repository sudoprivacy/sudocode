//! TEMPORARY diagnostic — DO NOT MERGE.
//!
//! `iocraft_repl_ctrlc_hint_in_footer` has been failing intermittently on
//! macOS CI since at least the #603 merge, always at the same step: after
//! Ctrl-C, the test types `/exit` and the input line never shows it. The last
//! captured failure had the hint already expired (the footer was back to
//! normal) and a 10s budget, so the characters were not merely late — they were
//! gone for the whole budget.
//!
//! A single green run proves nothing about an intermittent failure, so this
//! measures instead of guessing. Two arms, same REPL, same keystrokes:
//!
//! - `control` types `/exit` with no Ctrl-C before it.
//! - `after_ctrlc` sends Ctrl-C, waits for the hint, then types `/exit`.
//!
//! Each arm runs `ROUNDS` times and reports how many rounds rendered the text.
//! The comparison is the whole point: if `control` is clean and `after_ctrlc`
//! is not, Ctrl-C is implicated and the next question is whether macOS is
//! delivering it as a signal that resets the terminal mode (the hazard the
//! readiness probe in `pty_repl_iocraft_features` already documents). If both
//! arms drop rounds, the loss is in the send/render path and has nothing to do
//! with Ctrl-C.
//!
//! Reports a summary and only then asserts, so one CI run yields a rate rather
//! than a single dump.

mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;

const ROUNDS: usize = 6;
const RENDER_BUDGET: Duration = Duration::from_secs(10);

/// What the prompt row holds: the text after the last prompt glyph on the
/// lowest row carrying one. Same rule as `pty_arrow_keys::input_line_of`,
/// which has to survive the chrome rule sharing the input's row.
fn input_line(sess: &mut PtySession) -> String {
    sess.render(|s| {
        s.contents()
            .lines()
            .rev()
            .find_map(|line| {
                let glyph = line.rfind('\u{276f}')?;
                Some(line[glyph + '\u{276f}'.len_utf8()..].trim().to_string())
            })
            .unwrap_or_default()
    })
}

/// Poll the prompt row for `needle`. Returns whether it showed, plus the last
/// thing seen and the final screen — the two facts a failing round needs.
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

/// One round. `with_ctrlc` selects the arm. Returns `Ok(())` when `/exit`
/// rendered, else a description of what the round saw instead.
fn run_round(label: &str, round: usize, with_ctrlc: bool) -> Result<(), String> {
    let env = TestEnv::new(&format!("{label}-{round}"));
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(RENDER_BUDGET);

    // Readiness: a keystroke that RENDERS proves iocraft owns the keyboard.
    // Read off the screen, not the byte stream — iocraft redraws the input in
    // pieces, so the text need not appear contiguously in the stream.
    sess.send("~probe~").expect("type readiness probe");
    let (probed, seen, screen) = wait_for_input(&mut sess, "~probe~");
    if !probed {
        return Err(format!(
            "round {round}: readiness probe never rendered (last saw {seen:?})\n{screen}"
        ));
    }

    if with_ctrlc {
        sess.send("\x03").expect("send Ctrl-C");
        // The hint is the observable that the Ctrl-C handler ran. Read it off
        // the screen so a stale stream match cannot satisfy it.
        let deadline = Instant::now() + RENDER_BUDGET;
        loop {
            let shown = sess.render(|s| s.contents().contains("Press Ctrl-C again to exit"));
            if shown {
                break;
            }
            if Instant::now() >= deadline {
                let screen = sess.render(|s| s.contents());
                return Err(format!(
                    "round {round}: Ctrl-C hint never appeared\n{screen}"
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // Ctrl-C clears the input, so the probe leaves nothing behind. Confirm
        // that, so a round that starts with leftovers is not mistaken for one
        // that lost keystrokes.
        let after = input_line(&mut sess);
        if after.contains("~probe~") {
            return Err(format!(
                "round {round}: Ctrl-C did not clear the input (saw {after:?})"
            ));
        }
    } else {
        // Same starting state as the Ctrl-C arm, reached without Ctrl-C, so the
        // two arms differ in exactly one thing.
        sess.send("\x15").expect("Ctrl-U to clear");
        let deadline = Instant::now() + RENDER_BUDGET;
        loop {
            if !input_line(&mut sess).contains("~probe~") {
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!("round {round}: Ctrl-U never cleared the input"));
            }
            std::thread::sleep(Duration::from_millis(25));
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

fn measure(label: &str, with_ctrlc: bool) -> Vec<String> {
    let mut failures = Vec::new();
    for round in 0..ROUNDS {
        match run_round(label, round, with_ctrlc) {
            Ok(()) => eprintln!("[{label}] round {round}: /exit rendered"),
            Err(why) => {
                eprintln!("[{label}] round {round}: LOST\n{why}");
                failures.push(why);
            }
        }
    }
    eprintln!(
        "[{label}] {}/{ROUNDS} rounds rendered /exit",
        ROUNDS - failures.len()
    );
    failures
}

#[test]
fn measure_keystroke_loss_with_and_without_ctrlc() {
    let control = measure("control", false);
    let after_ctrlc = measure("after-ctrlc", true);

    eprintln!(
        "SUMMARY control={}/{ROUNDS} after_ctrlc={}/{ROUNDS}",
        ROUNDS - control.len(),
        ROUNDS - after_ctrlc.len()
    );

    // Fail whenever either arm dropped a round, so CI surfaces the dumps. The
    // summary line above is what discriminates the hypotheses.
    assert!(
        control.is_empty() && after_ctrlc.is_empty(),
        "keystrokes were lost: control dropped {} of {ROUNDS}, after_ctrlc dropped {} of {ROUNDS}",
        control.len(),
        after_ctrlc.len()
    );
}
