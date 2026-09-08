//! PTY e2e: bracketed-paste UX in the iocraft REPL.
//!
//! When text is pasted, the terminal wraps it in a bracketed-paste sequence
//! (ESC[200~ … ESC[201~). crossterm parses this into a single Event::Paste,
//! iocraft forwards it as TerminalEvent::Paste, and the REPL's `on_paste`
//! handler decides — matching Claude Code — whether to collapse it into a
//! compact `[Pasted text #N +M lines]` placeholder (long or multi-line
//! pastes) or insert it literally (short single-/double-line pastes). On
//! submit, placeholders expand back to the real text.
//!
//! These are human-fidelity PTY tests: they drive `scode` through a real PTY
//! (Unix pty / Windows ConPTY) and assert against the VT100-rendered screen,
//! exactly what a user sees. They guard three real bugs found by hand:
//!   1. multi-line paste auto-submitting one turn per line,
//!   2. Windows CR/CRLF pastes losing their line count (no `+N lines`),
//!   3. paste + Enter freezing the whole REPL (placeholder expansion loop).
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

/// Wrap a payload in the bracketed-paste framing a real terminal emits.
fn bracketed(payload: &str) -> String {
    format!("\u{1b}[200~{payload}\u{1b}[201~")
}

/// Four lines separated by LF (3 newlines > maxLines, so it collapses).
const FOUR_LINES_LF: &str = "line one\nline two\nline three\nline four";
/// Four lines separated by CRLF (Windows Terminal paste, Windows clipboard).
const FOUR_LINES_CRLF: &str = "line one\r\nline two\r\nline three\r\nline four";
/// Four lines separated by bare CR (some terminals emit CR for Enter).
const FOUR_LINES_CR: &str = "line one\rline two\rline three\rline four";

/// A multi-line paste collapses into a compact `[Pasted text #N +M lines]`
/// placeholder and does NOT auto-submit one turn per line. This is the core
/// regression guard for the original bug where each pasted newline arrived as
/// KeyCode::Enter and submitted a separate turn.
#[test]
fn multiline_paste_shows_placeholder_and_does_not_submit() {
    let env = TestEnv::new("bracketed-paste-lf");
    let mut sess = spawn_iocraft_repl(&env);

    sess.send(&bracketed(FOUR_LINES_LF)).expect("send paste");

    sess.expect("Pasted text #1")
        .expect("paste placeholder should appear in the input box");

    let screen = sess.render(|s| s.contents());
    // Four lines => three newlines => "+3 lines".
    assert!(
        screen.contains("[Pasted text #1 +3 lines]"),
        "expected multi-line placeholder, screen was:\n{screen}"
    );
    // The raw pasted lines must NOT have been submitted as turns.
    assert!(
        !screen.contains("line two"),
        "raw pasted content leaked / was submitted; screen:\n{screen}"
    );

    sess.send_ctrl('c').ok();
    sess.send_ctrl('c').ok();
    let _ = sess.expect_eof();
}

/// Windows Terminal delivers CRLF between lines; the placeholder must still
/// report the multi-line count (`+3 lines`), not drop to `[Pasted text #1]`.
#[test]
fn multiline_paste_crlf_counts_lines() {
    let env = TestEnv::new("bracketed-paste-crlf");
    let mut sess = spawn_iocraft_repl(&env);

    sess.send(&bracketed(FOUR_LINES_CRLF)).expect("send CRLF paste");

    sess.expect("Pasted text #1")
        .expect("CRLF paste placeholder should appear");

    let screen = sess.render(|s| s.contents());
    assert!(
        screen.contains("[Pasted text #1 +3 lines]"),
        "expected +3 lines for CRLF paste, screen was:\n{screen}"
    );
    assert!(
        !screen.contains("line two"),
        "raw CRLF pasted content leaked; screen:\n{screen}"
    );

    sess.send_ctrl('c').ok();
    sess.send_ctrl('c').ok();
    let _ = sess.expect_eof();
}

/// Some terminals separate lines with a bare CR; the placeholder must still
/// report the multi-line count after newline normalization.
#[test]
fn multiline_paste_bare_cr_counts_lines() {
    let env = TestEnv::new("bracketed-paste-cr");
    let mut sess = spawn_iocraft_repl(&env);

    sess.send(&bracketed(FOUR_LINES_CR)).expect("send CR paste");

    sess.expect("Pasted text #1")
        .expect("CR paste placeholder should appear");

    let screen = sess.render(|s| s.contents());
    assert!(
        screen.contains("[Pasted text #1 +3 lines]"),
        "expected +3 lines for bare-CR paste, screen was:\n{screen}"
    );

    sess.send_ctrl('c').ok();
    sess.send_ctrl('c').ok();
    let _ = sess.expect_eof();
}

/// A short single-line paste is inserted literally (no placeholder), matching
/// Claude Code: only long (>800 chars) or multi-line (>2 lines) pastes
/// collapse. The pasted text itself must appear in the box.
#[test]
fn short_single_line_paste_inserts_literally() {
    let env = TestEnv::new("bracketed-paste-short");
    let mut sess = spawn_iocraft_repl(&env);

    sess.send(&bracketed("hello world")).expect("send short paste");

    sess.expect("hello world").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("short paste should insert literally: {e}\nPTY:\n{screen}");
    });

    let screen = sess.render(|s| s.contents());
    assert!(
        !screen.contains("Pasted text #"),
        "short paste must NOT become a placeholder; screen:\n{screen}"
    );

    sess.send_ctrl('c').ok();
    sess.send_ctrl('c').ok();
    let _ = sess.expect_eof();
}

/// Regression guard for the freeze: pasting content that itself contains a
/// `[Pasted text #N]` string, then pressing Enter, must not hang the REPL.
/// The placeholder-expansion pass previously re-scanned inserted text and
/// looped forever, freezing keyboard and Ctrl-C. Here we paste a multi-line
/// blob containing the literal placeholder string, submit, and assert the
/// REPL is still alive by typing `/exit` and getting a clean exit.
#[test]
fn paste_containing_placeholder_string_then_enter_does_not_hang() {
    let env = TestEnv::new("bracketed-paste-selfref");
    let mut sess = spawn_iocraft_repl(&env);

    // Multi-line (so it collapses to a placeholder) AND contains the literal
    // "[Pasted text #1]" string, which is what triggered the expansion loop.
    let payload = "alpha\nbeta\n[Pasted text #1]\ngamma";
    sess.send(&bracketed(payload)).expect("send self-referential paste");
    sess.expect("Pasted text #1")
        .expect("placeholder should appear");

    // Submit. With the bug, placeholder expansion looped forever here and
    // froze the whole event loop before any turn could start.
    sess.send("\r").expect("press Enter");

    // Prove the REPL is still responsive after the submit: typed characters
    // must still render. If expansion had hung, this send/expect would time
    // out. (We assert responsiveness, not a full turn — the submitted turn
    // itself goes to the backend and is irrelevant to the freeze regression.)
    sess.send("still alive").expect("type after submit");
    sess.expect("still alive").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL frozen after paste+Enter (expansion hang?): {e}\nPTY:\n{screen}");
    });

    sess.send_ctrl('c').ok();
    sess.send_ctrl('c').ok();
    let _ = sess.expect_eof();
}

/// With bracketed paste enabled (VT console input active for the whole REPL
/// session on the Windows fork), ordinary keyboard typing must still reach
/// the TextInput and render — proving the shared ANSI parser path delivers
/// plain characters, not just paste sequences.
#[test]
fn typing_still_works_while_bracketed_paste_enabled() {
    let env = TestEnv::new("bracketed-paste-typing");
    let mut sess = spawn_iocraft_repl(&env);

    sess.send("/exit").expect("type /exit");
    sess.expect("/exit").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("typed text must render while VT input is active: {e}\nPTY:\n{screen}");
    });

    sess.send("\r").expect("press Enter");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
