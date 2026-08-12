//! PTY tests for the iocraft REPL: keyboard input delivery, auto_grow
//! exit, and basic interaction.
//!
//! These guard critical render-loop correctness in queue mode (iocraft).
//! Interactive features (history, tab completion) are validated by unit
//! tests in `repl_ui::tests`.

mod common;

use common::TestEnv;
use std::time::Duration;

/// **P0 regression guard**: typing in the iocraft REPL must produce
/// visible output in the terminal.
///
/// Root cause of the bug this guards: any unconditional `State::set(v)`
/// in the render-phase body (where `v == current_value`) triggers
/// `did_change` via DerefMut. This causes `component.wait()` to
/// resolve immediately in `select(component.wait(), term.wait())`,
/// starving `term.wait()` so keyboard events are never distributed to
/// subscribers. The user sees a frozen input — characters are typed
/// but nothing appears.
///
/// Journey: boot → type `/exit` character by character → verify `/exit`
/// appears in terminal → press Enter → clean exit.
#[test]
#[cfg(unix)]
fn iocraft_repl_keyboard_input_not_frozen() {
    let env = TestEnv::new("iocraft-input");
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    // Type "/exit" WITHOUT pressing Enter yet. The characters must
    // appear in the iocraft TextInput and be rendered to the PTY.
    sess.send("/exit").expect("type /exit");

    // Wait for iocraft to render the typed text. If the render loop is
    // starving term.wait() (the bug), this will timeout — the text
    // never appears because key events are never distributed.
    sess.expect("/exit").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("typed text must appear in terminal (keyboard input frozen?): {e}\nPTY:\n{screen}");
    });

    // Now press Enter to submit /exit and verify clean process exit.
    sess.send("\r").expect("press Enter");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// P0 regression guard: `/exit` in the iocraft REPL (with auto_grow
/// enabled) must not hang.
///
/// If the render loop's `should_exit()` check or `TextBufferView`
/// layout style caching is broken, the component stays "dirty"
/// indefinitely and this test times out.
#[test]
#[cfg(unix)]
fn iocraft_repl_auto_grow_exit_no_hang() {
    let env = TestEnv::new("iocraft-exit");
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("auto_grow exit must not hang: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// **P0 streaming regression guard**: markdown code blocks in model
/// responses must render with intact box-drawing borders.
///
/// Root cause of the bug this guards: every streaming chunk had a
/// spurious `\n` appended before being handed to the markdown renderer.
/// This broke the fenced code block parser — the renderer saw the
/// opening fence, a blank line, then the content on separate virtual
/// lines, causing it to emit plain text instead of a decorated block.
/// The box-drawing border characters (`╭─` / `╰─`) were absent entirely.
///
/// Journey: boot iocraft REPL in queue mode → send prompt that elicits
/// a fenced code block → verify `╭─` (opening border) appears → verify
/// `╰─` (closing border) appears → clean exit.
///
/// This MUST be a live test: mock responses are pre-recorded and bypass
/// the streaming chunk path that contained the bug.
#[test]
#[cfg(unix)]
fn iocraft_repl_streaming_code_block_not_corrupted() {
    let env = TestEnv::new("iocraft-code-block");

    if env.is_mock() {
        // This test targets a streaming-path bug that only manifests
        // with a real model producing chunks. Skip gracefully in mock
        // mode so CI continues unimpeded.
        eprintln!(
            "iocraft_repl_streaming_code_block_not_corrupted: \
             skipped in mock mode (requires SCODE_TEST_BACKEND=live)"
        );
        return;
    }

    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(30));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("initial prompt: {e}\nPTY:\n{screen}");
    });

    // Send a prompt that elicits a fenced code block response.
    let prompt = "Write a one-line hello world bash script in a fenced code block. Output only the code block, nothing else.";
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // The markdown renderer must emit box-drawing borders around fenced
    // code blocks. If the spurious-newline bug is present the renderer
    // falls back to plain text and these characters never appear.
    sess.expect("╭─").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "code block opening border '╭─' missing — \
             streaming chunk corruption suspected: {e}\nPTY:\n{screen}"
        );
    });

    sess.expect("╰─").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "code block closing border '╰─' missing — \
             code block split by spurious blank lines?: {e}\nPTY:\n{screen}"
        );
    });

    // Clean exit via /exit in the REPL.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("REPL prompt not seen after response: {e}\nPTY:\n{screen}");
    });
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("clean exit after code-block test: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
