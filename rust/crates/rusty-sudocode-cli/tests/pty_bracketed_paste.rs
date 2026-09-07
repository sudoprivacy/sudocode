//! PTY test: bracketed paste shows a placeholder and does NOT auto-submit.
//!
//! When multi-line text is pasted into the iocraft REPL, the terminal wraps
//! it in a bracketed-paste sequence (ESC[200~ … ESC[201~). crossterm parses
//! this into a single Event::Paste, iocraft forwards it as
//! TerminalEvent::Paste, and the REPL's `on_paste` handler replaces the
//! block with a compact `[Pasted text #N +M lines]` placeholder instead of
//! submitting each line as a separate turn.
//!
//! This is a regression guard for the bug where each pasted newline arrived
//! as a KeyCode::Enter and triggered a submit per line.
//!
//! **Unix only.** crossterm's Windows event source reads the console via
//! `ReadConsoleInputW` (Console API) and has no VT byte-stream parser, so it
//! never produces `Event::Paste` — bracketed paste on Windows needs a
//! separate VT-input backend (tracked separately). On Unix, crossterm's tty
//! source parses `ESC[200~…ESC[201~` into `Event::Paste` natively.
//!
//! ```bash
//! cargo test --test pty_bracketed_paste                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_bracketed_paste  # real API
//! ```

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::TestEnv;

/// Spawn the iocraft REPL (queue mode is the iocraft path; `off` is rustyline).
fn spawn_iocraft_repl(env: &TestEnv) -> pty_expect::PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(30));
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("initial prompt");
    sess
}

/// A bracketed-paste sequence carrying three lines. A real terminal emits
/// this framing on paste; we synthesize it directly on the PTY.
const PASTE: &str = "\u{1b}[200~line one\nline two\nline three\u{1b}[201~";

#[test]
fn bracketed_paste_shows_placeholder_and_does_not_submit() {
    let env = TestEnv::new("bracketed-paste");
    let mut sess = spawn_iocraft_repl(&env);

    // Paste three lines wrapped in the bracketed-paste framing.
    sess.send(PASTE).expect("send bracketed paste");

    // The input box should show the compact placeholder, NOT the raw lines,
    // and NOT have submitted anything.
    sess.expect("Pasted text #1")
        .expect("paste placeholder should appear in the input box");

    let screen = sess.render(|s| s.contents());
    // Three lines => two newlines => "+2 lines".
    assert!(
        screen.contains("[Pasted text #1 +2 lines]"),
        "expected multi-line placeholder, screen was:\n{screen}"
    );

    // The raw pasted lines must NOT have been submitted as turns; the
    // placeholder replaces them entirely.
    assert!(
        !screen.contains("line two"),
        "raw pasted content leaked / was submitted; screen:\n{screen}"
    );

    // Clean exit.
    sess.send_ctrl('c').ok();
    sess.send_ctrl('c').ok();
    let _ = sess.expect_eof();
}
