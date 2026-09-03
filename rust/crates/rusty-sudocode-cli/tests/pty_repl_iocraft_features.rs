//! PTY tests for the iocraft REPL: keyboard input delivery, auto_grow
//! exit, and basic interaction.
//!
//! These guard critical render-loop correctness in queue mode (iocraft).
//! Interactive features (history, tab completion) are validated by unit
//! tests in `repl_ui::tests`.

mod common;

use common::TestEnv;
use std::fs;
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

/// Ctrl-C hint appears in the FooterSlot (not scrollback) and
/// auto-dismisses. Pressing Ctrl-C once while idle should show
/// "Press Ctrl-C again to exit" in the footer area, and a subsequent
/// keypress should dismiss it. The hint must NOT appear in scrollback.
#[test]
#[cfg(unix)]
fn iocraft_repl_ctrlc_hint_in_footer() {
    let env = TestEnv::new("iocraft-ctrlc-hint");
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

    // Settle: on macOS CI the signal handler may not be fully wired
    // when the prompt first renders. A brief pause avoids Ctrl-C
    // arriving before the REPL's SIGINT handler is installed.
    std::thread::sleep(Duration::from_millis(200));

    // Press Ctrl-C once — should show hint in footer area.
    sess.send("\x03").expect("send Ctrl-C");

    sess.expect("Press Ctrl-C again to exit")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!("Ctrl-C hint should appear in footer: {e}\nPTY:\n{screen}");
        });

    // Clean exit.
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
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
#[cfg(unix)]
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
#[cfg(unix)]
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

    // Use a model that supports extended thinking.
    sess.send("/model claude-sonnet-4-6\r")
        .expect("send /model");
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after /model: {e}\nPTY:\n{screen}");
    });

    // Send a prompt that triggers thinking. The response should include
    // a thinking summary with non-zero chars if anthropic format is used.
    sess.send("What is 247 * 183? Think step by step.\r")
        .expect("send prompt");

    // Wait for response to complete.
    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("should return to prompt: {e}\nPTY:\n{screen}");
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

    // Clean exit.
    sess.send("/exit\r").expect("send /exit");
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
#[cfg(unix)]
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
#[cfg(unix)]
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
#[cfg(unix)]
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

    // 9. Select [1] defaultMode → Enum DialPad (plan, read-only, ...).
    sess.send("1").expect("select defaultMode");
    sess.expect("(?i)select value").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("enum picker for defaultMode: {e}\nPTY:\n{screen}");
    });

    // 10. Select [1] plan → writes to settings.json.
    sess.send("1").expect("select plan");
    sess.expect("= \"plan\"").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("enum write confirmation: {e}\nPTY:\n{screen}");
    });

    // 11. Verify on disk.
    let updated2 = fs::read_to_string(&settings_path).expect("read settings.json (2)");
    let json2: serde_json::Value = serde_json::from_str(&updated2).expect("parse (2)");
    assert_eq!(
        json2["permissions"]["defaultMode"], "plan",
        "settings.json should have permissions.defaultMode = plan\nFile contents:\n{updated2}"
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
