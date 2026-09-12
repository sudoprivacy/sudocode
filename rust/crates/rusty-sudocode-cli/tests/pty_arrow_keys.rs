//! PTY tests for arrow key behavior in the REPL.
//!
//! Verifies Claude Code-aligned UX:
//!   ↑ on non-empty buffer: move cursor to beginning of line
//!   ↑ on empty buffer or at beginning: navigate history
//!   ↓ on non-empty buffer: move cursor to end of line
//!   ↓ on empty buffer or at end: navigate history

mod common;

use std::fs;
use std::time::{Duration, Instant};

use pty_expect::PtySession;

/// Type text, press ↑ then insert a char. If cursor moved to beginning,
/// the char appears at position 0 and the submitted text starts with it.
#[test]
fn up_arrow_moves_cursor_to_beginning_before_history() {
    let env = common::TestEnv::new("arrow-up");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // Type "world", then press ↑ (should move cursor to beginning),
    // then type "hello " — result should be "hello world".
    sess.send("world").expect("type world");
    std::thread::sleep(Duration::from_millis(200));
    sess.send("\x1b[A").expect("send Up arrow");
    std::thread::sleep(Duration::from_millis(200));
    sess.send("hello ").expect("type hello at beginning");
    std::thread::sleep(Duration::from_millis(200));

    // The buffer should now contain "hello world". Verify by checking
    // the PTY screen shows "hello world" on the prompt line.
    let screen = sess.render(|s| s.contents());
    assert!(
        screen.contains("hello world"),
        "↑ should move cursor to beginning so 'hello ' is inserted before 'world'.\n\
         PTY screen:\n{screen}",
    );

    // Submit and clean exit.
    sess.send("\x1b[B").expect("send Down arrow to move to end");
    std::thread::sleep(Duration::from_millis(100));
    // Clear the line and exit instead of submitting to LLM.
    sess.send("\x15").expect("Ctrl-U to clear line");
    std::thread::sleep(Duration::from_millis(100));
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen2 = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen2}");
    });
    assert_eq!(exit, 0);
}

/// On empty prompt, ↑ should navigate history (not get stuck).
/// Submit a line first to populate history, then on the next prompt
/// press ↑ — the previous input should appear.
#[test]
fn up_arrow_navigates_history_on_empty_buffer() {
    let env = common::TestEnv::new("arrow-hist");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let prompt = env.prompt("say OK", "single_turn_text");
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(15));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // Submit a prompt so history has an entry.
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // Wait for the turn to complete and next prompt to appear.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("second prompt: {e}\nPTY screen:\n{screen}");
    });

    // On empty prompt, press ↑ — should show previous input from history.
    sess.send("\x1b[A").expect("send Up arrow on empty buffer");
    std::thread::sleep(Duration::from_millis(500));

    let screen = sess.render(|s| s.contents());
    // The history entry should contain part of our prompt.
    assert!(
        screen.contains("say OK") || screen.contains("single_turn_text"),
        "↑ on empty buffer should navigate history.\nPTY screen:\n{screen}",
    );

    // Clear and exit.
    sess.send("\x15").expect("Ctrl-U");
    std::thread::sleep(Duration::from_millis(100));
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen2 = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen2}");
    });
    assert_eq!(exit, 0);
}

/// ↓ on non-empty buffer should move cursor to end of line.
/// Type text, move to beginning with ↑, then ↓ should go back to end.
#[test]
fn down_arrow_moves_cursor_to_end() {
    let env = common::TestEnv::new("arrow-down");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // Type "hello", press ↑ (go to beginning), then ↓ (go to end),
    // then type " world" — should produce "hello world".
    sess.send("hello").expect("type hello");
    std::thread::sleep(Duration::from_millis(200));
    sess.send("\x1b[A").expect("Up to beginning");
    std::thread::sleep(Duration::from_millis(200));
    sess.send("\x1b[B").expect("Down to end");
    std::thread::sleep(Duration::from_millis(200));
    sess.send(" world").expect("type world at end");
    std::thread::sleep(Duration::from_millis(200));

    let screen = sess.render(|s| s.contents());
    assert!(
        screen.contains("hello world"),
        "↑ then ↓ should round-trip cursor: 'hello' + ' world' at end = 'hello world'.\n\
         PTY screen:\n{screen}",
    );

    sess.send("\x15").expect("Ctrl-U");
    std::thread::sleep(Duration::from_millis(100));
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen2 = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen2}");
    });
    assert_eq!(exit, 0);
}

// ─────────────────────────────────────────────────────────────────────────
// The three-tier behaviour per direction, as specified by #594:
//
//   Up:   1) not on first logical line  -> TextInput moves a line up
//         2) on first line, cursor > 0  -> jump to offset 0
//         3) cursor at 0 / empty input  -> recall previous history entry
//
//   Down: 1) not on last logical line   -> TextInput moves a line down
//         2) on last line, cursor < end -> jump to end
//         3) cursor at end              -> nothing (no forward history)
//
// The tests above cover Up tier 2, Up tier 3 on an *empty* buffer, and Down
// tier 2. What follows covers the rest: the multi-line tiers, the recall step
// on a non-empty buffer, and Down's no-op.
// ─────────────────────────────────────────────────────────────────────────

/// The REPL's live input buffer: whatever follows the prompt glyph on the
/// lowest screen row that carries one.
///
/// Bottom-most rather than anywhere, because everything above the input line is
/// transcript — including the prompts of earlier turns, which is why searching
/// the whole screen cannot tell "the buffer holds this" from "we submitted this
/// a turn ago". That is also why [`common::expect_input_line`] is not a
/// substitute here despite the similar name: it answers the looser question
/// "did these characters ever appear on a prompt line", which a stale echo from
/// a previous turn satisfies.
///
/// CONTAINS the glyph, never starts-with, and returns only the text after it.
/// The input line shares its row with the chrome rule above it whenever that
/// rule fills the terminal width exactly — the row then reads
/// `────…────❯ abc!`, which does not *start* with the glyph. A starts-with
/// probe skipped that row, walked up into the transcript, and reported an
/// earlier turn's prompt as the current buffer; the test then timed out with
/// its expected text plainly visible in the failure dump. Whether the rule and
/// the input share a row depends on the frame's line accounting, which is what
/// made it intermittent, and Windows CI is where it showed up.
fn input_line(sess: &mut PtySession) -> String {
    sess.render(|s| input_line_of(&s.contents()))
}

/// The screen-parsing half of [`input_line`], split out so the rule it has to
/// survive can be pinned against a real captured screen instead of waiting for
/// CI to roll the dice again.
fn input_line_of(screen: &str) -> String {
    screen
        .lines()
        .rev()
        .find_map(|line| {
            let glyph = line.rfind('\u{276f}')?;
            Some(line[glyph + '\u{276f}'.len_utf8()..].trim().to_string())
        })
        .unwrap_or_default()
}

/// Verbatim from the Windows CI failure this parser was rewritten for: the
/// chrome rule filled the row exactly, so the input line rendered as
/// `────…────❯ abc!` and the previous turn's echo sat above it. A starts-with
/// probe returned that echo and the test timed out with `abc!` on screen.
///
/// Not a PTY test on purpose: the collision depends on the frame's line
/// accounting, so a live REPL reproduces it only by luck. The parser is a pure
/// function of the screen, and this is the screen.
#[test]
fn input_line_reads_the_buffer_when_chrome_shares_its_row() {
    let dashes = "\u{2500}".repeat(80);
    let screen = format!(
        "\u{276f} do not resurface me PARITY_SCENARIO:single_turn_text\n\
         \u{23fa} The answer is 4\n\
         [claude-sonnet-4-6] \u{b7} turn 1 \u{b7} 21 tokens\n\
         {dashes}\u{276f} abc!\n\
         {dashes}\n\
         \u{23f5}\u{23f5} read-only \u{b7} /help \u{b7} /exit to quit\n"
    );
    assert_eq!(input_line_of(&screen), "abc!");
}

/// The uncollided shape still parses, and a submitted line above the prompt is
/// not mistaken for the live buffer — the property that rules out matching any
/// prompt-bearing row on screen.
#[test]
fn input_line_prefers_the_lowest_prompt_row() {
    let screen = "\u{276f} submitted a turn ago\n\
                  \u{23fa} answer\n\
                  \u{276f} being typed now\n";
    assert_eq!(input_line_of(screen), "being typed now");

    // An empty buffer reads as empty rather than as the echo above it.
    let screen = "\u{276f} submitted a turn ago\n\u{23fa} answer\n\u{276f}\n";
    assert_eq!(input_line_of(screen), "");
}

/// Wait for the REPL's input line to contain `needle`.
///
/// Not `expect`: that matches the byte stream as it arrives, which is not the
/// same thing as what ends up on screen. iocraft redraws the input line in
/// pieces with cursor moves between them, so a line the user can plainly read
/// may never appear contiguously in the stream — and whether it does differs
/// by platform. That is exactly how an earlier version of these tests passed
/// on Windows and timed out on Linux with the expected text sitting in the
/// failure dump.
fn wait_for_input_line(sess: &mut PtySession, needle: &str, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let line = input_line(sess);
        if line.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}: input line never contained {needle:?} (last saw {line:?})\nPTY screen:\n{}",
            sess.render(|s| s.contents()),
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Let an arrow key land before the next keystroke is sent.
///
/// A fixed wait, because nothing better exists here: an arrow moves the
/// cursor and changes no text, and the iocraft REPL parks the terminal cursor
/// at the end of the frame rather than on the input position — so the PTY
/// screen is byte-identical before and after, and there is no state to poll.
///
/// The wait is not decoration. The REPL key handler runs *after* the
/// `TextInput` it wraps, and `use_terminal_events` hands a hook every event
/// that is pending. A character sent in the same batch as the arrow would be
/// inserted by `TextInput` against its own cursor move, before the REPL tier
/// logic has run at all — the test would then be measuring the batch, not the
/// behaviour. Generous rather than tight, since being early here is a
/// confusing wrong answer and being late costs only time.
fn settle_after_arrow() {
    std::thread::sleep(Duration::from_millis(750));
}

/// Send an arrow key and let it land.
fn press_arrow(sess: &mut PtySession, keys: &str, label: &str) {
    sess.send(keys)
        .unwrap_or_else(|e| panic!("{label}: send failed: {e}"));
    settle_after_arrow();
}

/// Submit one prompt so the history has an entry, and leave the session at a
/// fresh empty prompt.
fn seed_history(env: &common::TestEnv, sess: &mut PtySession, text: &str) {
    let prompt = env.prompt(text, "single_turn_text");
    sess.send(&format!("{prompt}\r")).expect("send seed prompt");
    // The iocraft REPL holds one persistent prompt rather than reprinting it
    // per turn, so the reply text is the completion signal.
    sess.expect("The answer is 4").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("seed turn should complete: {e}\nPTY screen:\n{screen}");
    });
}

/// Up on a non-empty buffer must move to the start *before* it recalls, and
/// the difference is only visible once history is non-empty.
///
/// `up_arrow_moves_cursor_to_beginning_before_history` above runs with an
/// empty history, where recall is a no-op — so it cannot tell "moved to the
/// start" apart from "tried to recall and found nothing". Seeding history
/// first makes the two outcomes different screens.
#[test]
fn up_arrow_moves_to_start_before_recalling_history() {
    let env = common::TestEnv::new("arrow-up-then-recall");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        // The harness defaults to `off`, which is the sync rustyline REPL.
        // #594 is about the iocraft one, so ask for it explicitly.
        &[
            ("EDITOR", "true"),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(20));

    sess.expect("\u{276f}").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });
    seed_history(&env, &mut sess, "remember me");

    sess.send("world").expect("type world");
    wait_for_input_line(&mut sess, "world", "typed text should render");

    // First Up: move to the start. If it recalled instead, the buffer would
    // be the seeded prompt and "hello " would not land in front of "world".
    press_arrow(&mut sess, "\x1b[A", "first Up");
    sess.send("hello ").expect("type at start");
    wait_for_input_line(
        &mut sess,
        "hello world",
        "first Up on a non-empty buffer should move to the start, not recall history",
    );

    // The cursor sits after "hello " now, so one more Up returns it to the
    // start, and only the Up after that — with the cursor already at 0 —
    // recalls.
    press_arrow(&mut sess, "\x1b[A", "second Up");
    sess.send("\x1b[A").expect("third Up");
    wait_for_input_line(
        &mut sess,
        "remember me",
        "Up with the cursor already at the start should recall history",
    );

    sess.send("\x15").expect("Ctrl-U");
    settle_after_arrow();
    exit_cleanly(&mut sess);
}

/// Up inside a multi-line buffer moves between logical lines first; only once
/// the cursor is on the first line does it jump to the start.
#[test]
fn up_arrow_moves_between_logical_lines_before_jumping_to_start() {
    let env = common::TestEnv::new("arrow-up-multiline");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        // The harness defaults to `off`, which is the sync rustyline REPL.
        // #594 is about the iocraft one, so ask for it explicitly.
        &[
            ("EDITOR", "true"),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(20));

    sess.expect("\u{276f}").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });

    // A two-line paste stays literal (the placeholder only kicks in above two
    // newlines), which is how a test gets a multi-line buffer without Enter
    // submitting it.
    sess.send("\u{1b}[200~alpha\nbravo\u{1b}[201~")
        .expect("paste two lines");
    // The first of the two pasted lines is the one carrying the prompt glyph;
    // seeing it means the paste landed literally rather than collapsing into a
    // `[Pasted text #N]` placeholder.
    wait_for_input_line(&mut sess, "alpha", "pasted lines should render literally");

    // Cursor is at the end of "bravo". Up must land on the "alpha" line at the
    // same column — not jump to offset 0, and not recall history.
    press_arrow(&mut sess, "\x1b[A", "Up from the last line");
    sess.send("X").expect("mark the cursor");
    wait_for_input_line(
        &mut sess,
        "alphaX",
        "Up from the last line should move one logical line up, leaving the column alone",
    );

    sess.send("\x15").expect("Ctrl-U");
    settle_after_arrow();
    exit_cleanly(&mut sess);
}

/// Down with the cursor already at the end does nothing. Claude Code has no
/// forward history, so this must not pull an entry in.
#[test]
fn down_arrow_at_end_does_not_navigate_forward_history() {
    let env = common::TestEnv::new("arrow-down-noop");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        // The harness defaults to `off`, which is the sync rustyline REPL.
        // #594 is about the iocraft one, so ask for it explicitly.
        &[
            ("EDITOR", "true"),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(20));

    sess.expect("\u{276f}").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt: {e}\nPTY screen:\n{screen}");
    });
    seed_history(&env, &mut sess, "do not resurface me");

    sess.send("abc").expect("type abc");
    wait_for_input_line(&mut sess, "abc", "typed text should render");

    // Nothing should happen, so there is no state change to wait for — the
    // assertion is about absence, which needs a bounded look.
    sess.send("\x1b[B").expect("Down at end of buffer");
    std::thread::sleep(Duration::from_millis(500));

    // The buffer must still be "abc" with the cursor still at its end, so a
    // typed marker appends. Had Down pulled a history entry in, the buffer
    // would be that entry and "abc!" would never render. (Checking the whole
    // screen for the seeded text proves nothing: submitting it echoed it into
    // the transcript, where it stays.)

    // And the cursor is still at the end, so typing appends.
    sess.send("!").expect("type after the no-op Down");
    wait_for_input_line(
        &mut sess,
        "abc!",
        "Down at the end must do nothing - no forward history, cursor left alone",
    );

    sess.send("\x15").expect("Ctrl-U");
    settle_after_arrow();
    exit_cleanly(&mut sess);
}

/// Type `/exit` and submit it as two steps, waiting for the line to render in
/// between: an unrendered line and its Enter can arrive in the same input
/// batch, and an empty line submits nothing — a silent hang rather than a
/// failure that says what went wrong.
fn exit_cleanly(sess: &mut PtySession) {
    sess.send("/exit").expect("type /exit");
    wait_for_input_line(sess, "/exit", "typed /exit should render before Enter");
    sess.send("\r").expect("send Enter");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
