//! PTY tests for the live spinner token counter.
//!
//! The spinner displays `↓ N tokens` during streaming once enough bytes
//! have arrived and at least 1 s has elapsed. After the turn, the status
//! line prints cumulative token info (`Nk tokens`).
//!
//! ```bash
//! cargo test --test pty_spinner_token_counter                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_spinner_token_counter  # real API
//! ```
mod common;

use std::time::Duration;

use common::TestEnv;

/// After a single-turn prompt the status line shows token info.
///
/// Mock mode: the status line always appears (fast response, token count
/// from the mock usage payload).
/// Live mode: same — the status line is post-turn, always rendered.
#[test]
fn status_line_shows_token_count_after_turn() {
    let env = TestEnv::new("spinner-tokens");
    let prompt = env.prompt(
        "What is 2+2? Answer with just the number.",
        "single_turn_text",
    );

    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);

    // The response must contain the answer.
    sess.expect("4").expect("response should contain '4'");

    // After the turn, the status line shows `N tokens` or `N.Nk tokens`.
    sess.expect("tokens")
        .expect("post-turn status line should show token count");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "single-turn should exit 0; got {exit}");
}

/// In live mode, the spinner should display `↓` (the token counter
/// marker) while a long unfinished paragraph is still streaming.
///
/// Mock mode skips this assertion because the mock response completes
/// too fast for the 1 s display threshold.
#[test]
fn live_spinner_shows_token_counter_during_streaming() {
    let env = TestEnv::new("spinner-tokens-live");

    if env.is_mock() {
        // Mock responses complete in <100 ms — the spinner never reaches
        // the SHOW_TOKENS_AFTER_SECS threshold. Just verify no crash.
        let prompt = env.prompt("Tell me a short joke.", "streaming_text");
        let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
        sess.expect("\\w+").expect("some response text");
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);
        return;
    }

    // Visible reasoning also pauses the spinner. This case measures a plain
    // response stream; reasoning visibility is covered by pty_thinking_visible.
    let config = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.json"), r#"{"thinking":false}"#).unwrap();
    // Complete paragraphs are committed to scrollback and pause the spinner.
    // Keep one paragraph unfinished long enough to observe its token counter.
    let prompt = env.prompt(
        "Write a single paragraph of at least 800 words about the history of the Rust programming language. Use plain prose only: no headings, lists or newline characters. Keep writing until the paragraph is complete.",
        "unused_live_only",
    );
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    sess.set_default_timeout(common::at_least(Duration::from_secs(60)));

    // The spinner shows `↓` in the raw PTY byte stream while tokens
    // are being received. We must catch it before the response text
    // overwrites the spinner line — `expect` watches the live stream.
    sess.expect("↓").unwrap_or_else(|error| {
        panic!(
            "spinner token counter: {error}\n{}",
            sess.render(|screen| screen.raw().contents())
        );
    });

    // Wait for the turn to finish and status line to appear.
    sess.expect("tokens")
        .expect("post-turn status line should show token count");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

// Timing faults are injected at the HTTP stream, exercising the actual engine,
// renderer and terminal. Live coverage above checks the real provider path.
#[test]
fn spinner_warning_tracks_idle_time_and_recovers_on_progress() {
    use std::time::Instant;
    let env = TestEnv::new_mock("spinner-activity");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    sess.expect("❯").unwrap();
    let prompt = env.prompt("unused deterministic stream", "spinner_activity");
    sess.send(&prompt).unwrap();
    common::expect_input_line(&sess, &prompt, common::DEFAULT_TIMEOUT, "stream prompt");
    sess.send("\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_initial_wait = false;
    let mut saw_long_progress = false;
    let mut saw_stall = false;
    let mut saw_recovery = false;
    while Instant::now() < deadline {
        let snapshot = sess.render(|screen| {
            let raw = screen.raw();
            raw.rows(0, raw.size().1)
                .enumerate()
                .filter_map(|(row, line)| {
                    let col = line.find("Thinking")?;
                    let column = unicode_width::UnicodeWidthStr::width(&line[..col]);
                    let cell = raw.cell(u16::try_from(row).ok()?, u16::try_from(column).ok()?)?;
                    Some((line, format!("{:?}", cell.fgcolor())))
                })
                .last()
        });
        if let Some((line, color)) = snapshot {
            if line.contains("(3.") && !line.contains('↓') {
                assert!(
                    common::colors_equal(&color, "Rgb(166, 227, 161)"),
                    "initial wait remains green: {color}"
                );
                saw_initial_wait = true;
            }
            if line.contains("↓ 9 tokens") || line.contains("↓ 11 tokens") {
                if common::colors_equal(&color, "Idx(220)") {
                    saw_stall = true;
                } else {
                    assert!(common::colors_equal(&color, "Rgb(166, 227, 161)"));
                    saw_long_progress = true;
                }
            }
            if line.contains("↓ 13 tokens") && common::colors_equal(&color, "Rgb(166, 227, 161)")
            {
                saw_recovery = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(saw_initial_wait && saw_long_progress && saw_stall && saw_recovery, "initial={saw_initial_wait} progress={saw_long_progress} stalled={saw_stall} recovered={saw_recovery}\n{}", sess.render(|s| s.raw().contents()));
    common::expect_screen_settled(
        &sess,
        |s| s.contains("resumed") && s.contains("ctx "),
        common::DEFAULT_TIMEOUT,
        "stream completes",
    );
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}
