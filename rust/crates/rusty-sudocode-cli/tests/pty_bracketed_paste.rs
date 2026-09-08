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
//! **Cross-platform.** On Unix, crossterm's tty source parses
//! `ESC[200~…ESC[201~` into `Event::Paste` natively. On Windows, the
//! sudoprivacy crossterm fork (upstream PR crossterm-rs/crossterm#1030)
//! enables `ENABLE_VIRTUAL_TERMINAL_INPUT` for the lifetime of
//! `EnableBracketedPaste` and feeds the console's VT byte stream through the
//! shared ANSI parser, producing the same `Event::Paste` as Unix.
//!
//! ```bash
//! cargo test --test pty_bracketed_paste                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_bracketed_paste  # real API
//! ```

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

/// Regression guard: with bracketed paste enabled (and therefore, on the
/// Windows crossterm fork, VT console input active for the whole REPL
/// session), ordinary keyboard typing must still reach the TextInput and
/// render. PR crossterm-rs/crossterm#1030 routes character-bearing key
/// records through the shared ANSI parser while VT input is on; this proves
/// that path delivers plain typed characters, not just paste sequences.
#[test]
fn typing_still_works_while_bracketed_paste_enabled() {
    let env = TestEnv::new("bracketed-paste-typing");
    let mut sess = spawn_iocraft_repl(&env);

    // Type a command WITHOUT Enter; the characters must render in the box.
    sess.send("/exit").expect("type /exit");
    sess.expect("/exit").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("typed text must render while VT input is active: {e}\nPTY:\n{screen}");
    });

    // Enter submits /exit and the process exits cleanly.
    sess.send("\r").expect("press Enter");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
