//! PTY tests for the iocraft REPL: keyboard input delivery, auto_grow
//! exit, and basic interaction.
//!
//! These guard critical render-loop correctness in queue mode (iocraft).
//! Interactive features (history, tab completion) are validated by unit
//! tests in `repl_ui::tests`.

mod common;

use common::TestEnv;
use std::fs;
use std::time::{Duration, Instant};

/// Budget for the process to exit after `/exit`, separate from the budget a
/// test gives the behaviour it asserts.
///
/// Teardown is its own cost — unwinding the iocraft render loop and persisting
/// the session — and it is unrelated to how long the assertion under test
/// should take. Sharing a test's tight interaction budget made these exits
/// intermittently outrun it on a cold macOS runner. Generous by design: the
/// hangs these tests guard against are unbounded, so a wide budget still
/// catches them while runner speed no longer decides the verdict.
const EXIT_BUDGET: Duration = Duration::from_secs(60);

/// The Ctrl-C confirmation hint, verbatim from `repl_ui`.
const HINT: &str = "Press Ctrl-C again to exit";

/// Index of the first rendered row containing `needle`, if any.
///
/// A row index, not a boolean, because "the hint is in the footer" is a claim
/// about WHERE it rendered — below the input line rather than up in the
/// transcript — and only a position can express that.
fn row_containing(sess: &mut pty_expect::PtySession, needle: &str) -> Option<usize> {
    sess.render(|s| s.contents().lines().position(|line| line.contains(needle)))
}

/// Block until `needle` occupies a row, and return that row.
fn wait_for_row_containing(
    sess: &mut pty_expect::PtySession,
    needle: &str,
    budget: Duration,
) -> usize {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(row) = row_containing(sess, needle) {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "no row showed {needle:?} within {budget:?}\nPTY:\n{}",
            sess.render(|s| s.contents())
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Block until `needle` is gone from every row.
fn wait_for_no_row_containing(sess: &mut pty_expect::PtySession, needle: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    while row_containing(sess, needle).is_some() {
        assert!(
            Instant::now() < deadline,
            "{needle:?} was still on screen after {budget:?}\nPTY:\n{}",
            sess.render(|s| s.contents())
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

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

    // Wait for iocraft to render the typed text on the INPUT LINE. If the
    // render loop is starving term.wait() (the bug), this times out — the
    // characters never appear because key events are never distributed.
    common::expect_input_line(
        &sess,
        "/exit",
        Duration::from_secs(10),
        "typed text must appear in terminal (keyboard input frozen?)",
    );

    // Now press Enter to submit /exit and verify clean process exit.
    sess.send("\r").expect("press Enter");
    sess.set_default_timeout(EXIT_BUDGET);
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
    sess.set_default_timeout(EXIT_BUDGET);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("auto_grow exit must not hang: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// Ctrl-C hint renders in the FooterSlot, below the input line, not in
/// scrollback.
///
/// Split from the auto-dismiss check below, and run with a TTL long enough
/// that the hint cannot expire mid-test. The two claims need opposite timing
/// to observe — one needs the hint present, the other needs it gone — and
/// asserting both against one 3-second transient is what made this the flake
/// that blocked merges: a polling loop on a loaded runner can miss the window
/// entirely, after which the hint is gone for good and the test spins out its
/// budget having measured the scheduler rather than the footer.
#[test]
fn iocraft_repl_ctrlc_hint_renders_in_the_footer() {
    let env = TestEnv::new("iocraft-ctrlc-hint");
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            // Effectively never expires for the life of this test.
            ("SUDOCODE_CTRLC_HINT_TTL_MS", "600000"),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("\u{276f}").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    // Readiness, not a guess. Until iocraft has taken the terminal out of
    // canonical mode, ^C is still a terminal signal and would kill the child
    // outright instead of arriving as a key event. The prompt can be on screen
    // before that happens, so the prompt alone is not the signal — a keystroke
    // that renders is: it proves iocraft owns the keyboard and is distributing
    // key events. Ctrl-C clears the input line, so the probe leaves nothing.
    sess.send("~probe~").expect("type readiness probe");
    sess.expect("~probe~").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("iocraft should render typed input before Ctrl-C is sent: {e}\nPTY:\n{screen}");
    });

    sess.send("\x03").expect("send Ctrl-C");

    // Read the SCREEN, not the byte stream: iocraft redraws everything on any
    // change, so a stream match says a frame went past, not what is displayed.
    let hint_row = wait_for_row_containing(&mut sess, HINT, Duration::from_secs(30));

    // BELOW the prompt, which is what "in the footer and not scrollback" means
    // in terms a test can check.
    let prompt_row = row_containing(&mut sess, "\u{276f}")
        .expect("the prompt row is on screen once the REPL is up");
    assert!(
        hint_row > prompt_row,
        "the Ctrl-C hint must render in the footer, below the input line, \
         not in the scrollback above it (hint row {hint_row}, prompt row {prompt_row})\nPTY:\n{}",
        sess.render(|s| s.contents())
    );

    // Ctrl-C also clears the input line. Asserted here rather than by typing
    // afterwards: keystrokes in the window right after Ctrl-C are dropped
    // (issue #621), so a test that typed would be testing that bug instead.
    common::expect_input_line_cleared(
        &sess,
        Duration::from_secs(15),
        "Ctrl-C should clear the input line",
    );
}

/// The hint auto-dismisses. This matters more than it sounds: the hint is the
/// ONE thing standing between a second Ctrl-C and an exit, so a hint that never
/// cleared would leave the REPL one keystroke from quitting indefinitely.
///
/// Run with a very short TTL so the end state — hint gone — is what the test
/// waits for, instead of having to catch the hint mid-flight first. The cleared
/// input line is the evidence that Ctrl-C was actually processed, so absence of
/// the hint cannot pass vacuously.
#[test]
fn iocraft_repl_ctrlc_hint_auto_dismisses() {
    let env = TestEnv::new("iocraft-ctrlc-dismiss");
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_CTRLC_HINT_TTL_MS", "300"),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("\u{276f}").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });
    sess.send("~probe~").expect("type readiness probe");
    sess.expect("~probe~").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("iocraft should render typed input before Ctrl-C is sent: {e}\nPTY:\n{screen}");
    });

    sess.send("\x03").expect("send Ctrl-C");

    // Ctrl-C landed: the handler clears the input line in the same pass that
    // raises the hint.
    common::expect_input_line_cleared(
        &sess,
        Duration::from_secs(15),
        "Ctrl-C should clear the input line",
    );

    // And the hint does not outlive its TTL.
    wait_for_no_row_containing(&mut sess, HINT, Duration::from_secs(15));
}

/// TurnPhase::Thinking renders in the StatusSlot during a turn.
/// Verifies the spinner shows the model name during streaming.
/// This is the foundation test for the TurnPhase ChromeSlot — it
/// confirms that phase-based rendering works end-to-end.
///
/// The retry sub-phase (TurnPhase::Retry) cannot be reliably triggered
/// in automated tests — it requires a specific proxy error. Manual
/// verification: use `/model claude-fable-5` (non-existent) and send a
/// prompt to trigger retries, then verify retry text appears in the
/// StatusSlot and Ctrl-C cancels cleanly.
#[test]
fn iocraft_repl_turn_phase_thinking_renders() {
    let env = TestEnv::new("iocraft-turn-phase");

    if env.is_mock() {
        eprintln!(
            "iocraft_repl_turn_phase_thinking_renders: \
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
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("What is 2+2? Answer only the number.\r")
        .expect("send prompt");

    // The spinner should show "Thinking" during the turn.
    sess.expect("(?i)thinking").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("Thinking phase should render in StatusSlot: {e}\nPTY:\n{screen}");
    });

    // Wait for response and reprompt.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should return to prompt: {e}\nPTY:\n{screen}");
    });

    // Clean exit.
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// SSOT endpoint routing: Claude models use anthropic-messages format
/// (not openai-completions) when the model_capabilities SSOT has
/// endpoint_types from sudorouter. Verifies that extended thinking
/// content is visible (non-zero chars) — this only works with native
/// Anthropic format, not OpenAI-compatible.
///
/// Live-only: requires real API with extended thinking model.
#[test]
fn iocraft_repl_anthropic_format_thinking_visible() {
    let env = TestEnv::new("iocraft-anthropic-thinking");

    if env.is_mock() {
        eprintln!(
            "iocraft_repl_anthropic_format_thinking_visible: \
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
    sess.set_default_timeout(Duration::from_secs(60));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    // No `/model` switch: live mode already runs on `sonnet`, which supports
    // extended thinking. Asking for the model that is already active prints the
    // full model *listing* rather than a switch report, and that long block
    // desynchronised every `expect` that follows.

    // Send a prompt that triggers thinking. The response should include
    // a thinking summary with non-zero chars if anthropic format is used.
    sess.send("What is 247 * 183? Think step by step.\r")
        .expect("send prompt");

    // Wait for the answer itself, not for `❯` — the prompt marker also prefixes
    // the echo of the question that was just submitted, so matching it returns
    // while the turn is still running. `/exit` then lands in the input queue
    // mid-turn and is never submitted, and the session never ends.
    sess.expect(r"45(?:[,. ]|\{,\})?201").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should see the computed answer: {e}\nPTY:\n{screen}");
    });

    // Check the full screen for thinking summary. With anthropic format,
    // we should see "Thinking (N chars hidden)" where N > 0.
    // With openai format, N would be 0 (adaptive thinking, content empty).
    let screen = sess.render(|s| s.contents());
    let has_thinking = screen.contains("Thinking");
    if has_thinking {
        // If thinking summary is present, verify it's not "0 chars"
        // which would indicate openai format (content lost).
        assert!(
            !screen.contains("0 chars hidden"),
            "Thinking content should be non-empty with anthropic-messages format.\n\
             If '0 chars hidden' appears, the model may be using openai-completions \
             format instead of anthropic-messages.\nPTY:\n{screen}"
        );
    }
    // Note: some models/prompts may not trigger thinking at all,
    // so we don't assert thinking is always present.

    // Clean exit. Input is queued during a turn (`SUDOCODE_INTERRUPT_QUEUE_MODE`),
    // so `/exit` only runs once the model has finished — and "think step by
    // step" invites a long answer. Give that more room than the default.
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(180));
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// SSOT endpoint routing: GPT models use openai-completions format.
/// Verifies basic request/response works through proxy passthrough.
///
/// Live-only: requires real API.
#[test]
fn iocraft_repl_openai_format_gpt_works() {
    let env = TestEnv::new("iocraft-openai-gpt");

    if env.is_mock() {
        eprintln!(
            "iocraft_repl_openai_format_gpt_works: \
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
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/model gpt-4.1-mini\r").expect("send /model");
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after /model: {e}\nPTY:\n{screen}");
    });

    sess.send("Say exactly: GPT_ENDPOINT_OK\r")
        .expect("send prompt");

    sess.expect("GPT_ENDPOINT_OK").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("GPT should respond with marker: {e}\nPTY:\n{screen}");
    });

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should return to prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// SSOT endpoint routing: Gemini models use gemini format via proxy.
/// Verifies basic request/response works through proxy passthrough.
///
/// Live-only: requires real API.
#[test]
fn iocraft_repl_gemini_format_works() {
    let env = TestEnv::new("iocraft-gemini");

    if env.is_mock() {
        eprintln!(
            "iocraft_repl_gemini_format_works: \
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
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/model gemini-2.5-flash\r").expect("send /model");
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after /model: {e}\nPTY:\n{screen}");
    });

    sess.send("Say exactly: GEMINI_ENDPOINT_OK\r")
        .expect("send prompt");

    sess.expect("GEMINI_ENDPOINT_OK").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("Gemini should respond with marker: {e}\nPTY:\n{screen}");
    });

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should return to prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
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

// ──────────────────────────────────────────────────────────────────────
// /config tree browser: navigate, drill in, Back, bool toggle, write-back
// ──────────────────────────────────────────────────────────────────────

/// End-to-end test for the interactive `/config` tree browser.
///
/// Journey (covers FieldSchema SSOT → DialPad/FuzzySelect dispatch →
/// ← Back navigation → bool toggle write-back):
///
///   /config → file picker (DialPad)
///   → [1] settings.json → field list (FuzzySelect, 13 items)
///   → type "sand" + Enter → sandbox children (DialPad, 6 items)
///   → [1] ← Back → back to settings level
///   → type "sand" + Enter → sandbox again
///   → [2] enabled → bool toggle (instant) → see "enabled = true"
///   → verify settings.json updated on disk
///   → /exit
#[test]
fn config_tree_navigate_back_and_toggle() {
    let env = TestEnv::new("config-tree");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // Seed settings.json with sandbox.enabled = false for toggle test.
    let config_home = env.config_home().to_path_buf();
    let settings_path = config_home.join("settings.json");
    fs::write(
        &settings_path,
        r#"{"model": "sonnet", "sandbox": {"enabled": false}}"#,
    )
    .expect("seed settings.json");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    // Wait for REPL prompt.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    // 1. Type /config → file picker (DialPad: settings.json, sudocode.json).
    sess.send("/config\r").expect("send /config");
    sess.expect("(?i)config file").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("file picker prompt: {e}\nPTY:\n{screen}");
    });

    // 2. Select [1] settings.json → field list (FuzzySelect: >9 items).
    sess.send("1").expect("select settings.json");
    // Wait for FuzzySelect to fully render (the 🔍 filter icon appears).
    sess.expect("Select field").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("settings field list: {e}\nPTY:\n{screen}");
    });

    // 3. Type "sand" to filter FuzzySelect → Enter selects sandbox ▸.
    //    Brief pause lets the render loop process the input-slot switch.
    std::thread::sleep(Duration::from_millis(300));
    sess.send("sand").expect("filter sandbox");
    sess.expect("sandbox").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("sandbox should appear in filtered list: {e}\nPTY:\n{screen}");
    });
    sess.send("\r").expect("select sandbox");
    sess.expect("enabled").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("sandbox children: {e}\nPTY:\n{screen}");
    });

    // 4. ← arrow → back to settings level.
    sess.send("\x1b[D").expect("left arrow");
    sess.expect("Select field").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("back to settings level: {e}\nPTY:\n{screen}");
    });

    // 5. Type "sand" + Enter again → drill back into sandbox.
    std::thread::sleep(Duration::from_millis(300));
    sess.send("sand").expect("filter sandbox again");
    sess.expect("sandbox").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("sandbox filter: {e}\nPTY:\n{screen}");
    });
    sess.send("\r").expect("select sandbox again");
    sess.expect("enabled").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("sandbox children showing enabled: {e}\nPTY:\n{screen}");
    });

    // 6. Select [1] enabled → instant bool toggle (BoolToggle).
    // DialPad layout: [1] enabled, [2] namespaceRestrictions, ...
    sess.send("1").expect("select enabled");
    sess.expect("enabled = true").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("bool toggle output: {e}\nPTY:\n{screen}");
    });

    // 7. Verify settings.json on disk has sandbox.enabled = true.
    let updated = fs::read_to_string(&settings_path).expect("read settings.json");
    let json: serde_json::Value = serde_json::from_str(&updated).expect("parse settings.json");
    assert_eq!(
        json["sandbox"]["enabled"], true,
        "settings.json should have sandbox.enabled = true after toggle\nFile contents:\n{updated}"
    );

    // ── Enum field: permissions.defaultMode ──

    // 8. /config again → settings → permissions → defaultMode (Enum DialPad).
    sess.send("/config\r").expect("send /config again");
    sess.expect("(?i)config file").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("file picker (2nd time): {e}\nPTY:\n{screen}");
    });
    sess.send("1").expect("select settings.json");
    sess.expect("Select field").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("settings field list (2nd): {e}\nPTY:\n{screen}");
    });
    std::thread::sleep(Duration::from_millis(300));
    sess.send("perm").expect("filter permissions");
    sess.expect("permissions").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("permissions filter: {e}\nPTY:\n{screen}");
    });
    sess.send("\r").expect("select permissions");
    sess.expect("defaultMode").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("permissions children: {e}\nPTY:\n{screen}");
    });

    // 9. Select [1] defaultMode → Enum DialPad (read-only, workspace-write, ...).
    sess.send("1").expect("select defaultMode");
    sess.expect("(?i)select value").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("enum picker for defaultMode: {e}\nPTY:\n{screen}");
    });

    // 10. Select [1] read-only → writes to settings.json.
    sess.send("1").expect("select read-only");
    sess.expect("= \"read-only\"").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("enum write confirmation: {e}\nPTY:\n{screen}");
    });

    // 11. Verify on disk.
    let updated2 = fs::read_to_string(&settings_path).expect("read settings.json (2)");
    let json2: serde_json::Value = serde_json::from_str(&updated2).expect("parse (2)");
    assert_eq!(
        json2["permissions"]["defaultMode"], "read-only",
        "settings.json should have permissions.defaultMode = read-only\nFile contents:\n{updated2}"
    );

    // ── sudocode.json browsing ──

    // 12. /config → [2] sudocode.json → verify web_search ▸ visible.
    sess.send("/config\r").expect("send /config (3rd)");
    sess.expect("(?i)config file").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("file picker (3rd): {e}\nPTY:\n{screen}");
    });
    sess.send("2").expect("select sudocode.json");
    sess.expect("web_search").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("sudocode.json fields: {e}\nPTY:\n{screen}");
    });

    // 13. ESC cancels out of tree entirely.
    sess.send("\x1b").expect("ESC to cancel");
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("back to prompt after ESC: {e}\nPTY:\n{screen}");
    });

    // 14. Clean exit.
    //
    // The `❯` matched above is ambiguous: the config-tree overlay also renders
    // `❯` as its row selector, so on a slow render (loaded macOS CI) `expect`
    // can match an intermediate tree frame while ESC is still tearing the
    // overlay down. If `/exit` is sent then, it lands on the closing overlay
    // instead of the REPL command line and the child never exits (10s eof
    // timeout — the macOS-only flake). Settle the render loop first — the same
    // input-slot pause the steps above use — so `/exit` reaches the command
    // parser, and give teardown extra headroom for a loaded runner.
    std::thread::sleep(Duration::from_millis(400));
    sess.set_default_timeout(Duration::from_secs(20));
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}

/// Regression: the `/model` picker sets `allow_custom_input` but renders as a
/// FuzzySelect (the bundled model list is long). Typing a name that matches no
/// listed model must still be submittable — Enter uses the typed filter text as
/// the answer instead of doing nothing. (The sibling DialPad path, used when a
/// question has <=9 options, is fixed the same way.)
#[test]
fn model_picker_accepts_custom_typed_name() {
    let env = TestEnv::new("dialpad-custom-input");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    // Open the model picker.
    sess.send("/model\r").expect("send /model");
    sess.expect("(?i)select model").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("model picker prompt: {e}\nPTY:\n{screen}");
    });

    // Type a model name that matches none of the listed options. The filter
    // empties, and the custom-input hint must appear.
    sess.send("zzz-custom-model").expect("type custom model");
    sess.expect("(?i)type a model name").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("custom-input hint missing on no-match: {e}\nPTY:\n{screen}");
    });

    // Enter submits the typed value as the answer; the switch names it.
    sess.send("\r").expect("submit custom model");
    sess.expect("zzz-custom-model").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("custom model not applied: {e}\nPTY:\n{screen}");
    });

    std::thread::sleep(Duration::from_millis(400));
    sess.set_default_timeout(Duration::from_secs(20));
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
