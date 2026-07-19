//! PTY tests for arrow key behavior in the REPL.
//!
//! Verifies Claude Code-aligned UX:
//!   ↑ on non-empty buffer: move cursor to beginning of line
//!   ↑ on empty buffer or at beginning: navigate history
//!   ↓ on non-empty buffer: move cursor to end of line
//!   ↓ on empty buffer or at end: navigate history

mod common;

use std::fs;
use std::time::Duration;

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
