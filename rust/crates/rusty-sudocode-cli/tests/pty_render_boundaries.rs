//! History content and live activity have separate terminal ownership.
mod common;

use common::TestEnv;
use std::time::Duration;

fn start(env: &TestEnv, mode: &str, args: &[&str], size: (u16, u16)) -> pty_expect::PtySession {
    let mut sess = env.spawn_with_env(args, &[("SUDOCODE_INTERRUPT_QUEUE_MODE", mode)]);
    sess.resize(size.0, size.1).expect("resize");
    common::expect_input_line_cleared(&sess, Duration::from_secs(30), "initial prompt");
    sess
}

fn finish(sess: &mut pty_expect::PtySession) {
    common::expect_input_line_cleared(sess, common::DEFAULT_TIMEOUT, "ready before exit");
    sess.send("/exit").expect("exit input");
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").expect("submit exit");
    assert_eq!(sess.expect_eof().expect("exit"), 0);
}

#[test]
fn text_thinking_text_preserves_order_and_literal_content() {
    for mode in ["off", "queue"] {
        // The exact block order is the regression fixture; a live model is not
        // required to emit this less common but valid sequence on demand.
        let env = TestEnv::new_mock("render-block-transitions");
        let mut sess = start(&env, mode, &["--permission-mode", "read-only"], (60, 100));
        let prompt = env.prompt("Preserve response order.", "text_thinking_transitions");
        sess.send(&format!("{prompt}\r")).expect("send prompt");
        let screen = common::expect_screen_settled(
            &sess,
            |s| s.contains("The answer follows the reasoning."),
            common::DEFAULT_TIMEOUT,
            "completed response",
        );
        assert_ordered_response(&screen);
        finish(&mut sess);

        let mut resumed = start(
            &env,
            mode,
            &["--resume", "latest", "--permission-mode", "read-only"],
            (60, 100),
        );
        let replay = common::expect_screen_settled(
            &resumed,
            |s| s.contains("The answer follows the reasoning."),
            common::DEFAULT_TIMEOUT,
            "replayed response",
        );
        assert_ordered_response(&replay);
        finish(&mut resumed);
    }
}

fn assert_ordered_response(screen: &str) {
    let positions: Vec<_> = [
        "Boundary before.",
        "card",
        "✻ Thinking",
        "Reasoning step one.",
        "Reasoning step two continues the same line.",
        "The answer follows the reasoning.",
    ]
    .iter()
    .map(|text| {
        screen
            .find(text)
            .unwrap_or_else(|| panic!("missing {text}: {screen}"))
    })
    .collect();
    assert!(positions.windows(2).all(|p| p[0] < p[1]), "{screen}");
    assert_eq!(screen.matches("Boundary before.").count(), 1, "{screen}");
    assert_eq!(
        screen.lines().filter(|l| l.trim() == "card").count(),
        1,
        "{screen}"
    );
    for row in screen.lines().filter(|row| {
        row.contains("Reasoning step") || row.contains("Boundary before.") || row.trim() == "card"
    }) {
        assert!(
            row.len() - row.trim_start().len() <= 4,
            "response stair-stepped: {screen}"
        );
    }
}

#[cfg(unix)]
#[test]
fn captured_diagnostics_preserve_utf8_across_pipe_reads() {
    let env = TestEnv::new_mock("render-stderr-framing");
    let mut sess = start(
        &env,
        "queue",
        &["--permission-mode", "read-only"],
        (140, 120),
    );
    // The rejected key produces one diagnostic longer than the capture pipe's
    // read buffer, with a multibyte character spanning that read boundary.
    let key = format!("x{}DIAGNOSTIC-END", "界".repeat(1500));
    sess.send(&format!("\x1b[200~/config set {key} value\x1b[201~"))
        .expect("paste invalid config command");
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("/config set"),
        common::DEFAULT_TIMEOUT,
        "config input",
    );
    sess.send("\r").expect("submit config command");
    let screen = common::expect_screen_settled(
        &sess,
        |s| s.contains("Error: Unknown setting:") && s.contains("DIAGNOSTIC-END"),
        common::DEFAULT_TIMEOUT,
        "captured diagnostic",
    );
    assert!(
        common::screen_contains(&screen, &format!("Unknown setting: \"{key}\"")),
        "diagnostic was split or decoded before its bytes were complete: {screen}"
    );
    assert!(!screen.contains('�'), "corrupted UTF-8: {screen}");
    assert_eq!(env.captured_message_count(), 0);
    finish(&mut sess);
}

#[cfg(unix)]
#[test]
fn bash_progress_is_live_then_clears_without_entering_history() {
    let env = TestEnv::new("render-tool-progress");
    // The command runs through the real tool executor. A release file makes
    // completion deterministic after the live status has been inspected.
    std::fs::write(
        env.workspace_root().join("progress.sh"),
        r"sleep 1.1; printf '\033[2J\033[H界👩🏽‍💻PROGRESS-FIRST\n'; while test ! -f release-progress; do sleep 0.1; done; printf 'PROGRESS-RESULT\n'; printf finished > progress-finished",
    )
    .expect("progress script");
    let calls = serde_json::json!([{
        "id": "toolu_render_progress", "name": "bash", "input": {"command":"sh progress.sh"}
    }]);
    let prompt = if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        "This temporary workspace is a terminal-rendering test. Please read progress.sh, \
         then run sh progress.sh with Bash in the foreground. It prints a Unicode line and \
         waits for the test runner to create release-progress. After it exits, reply: \
         Concurrency batch done."
            .into()
    };
    let _release_files = ReleaseFiles {
        root: env.workspace_root().to_path_buf(),
        release: &["release-progress"],
        finished: "progress-finished",
    };
    let mut sess = start(
        &env,
        "queue",
        &["--permission-mode", "danger-full-access"],
        (90, 44),
    );
    let prompt = format!("Terminal progress check. {prompt}");
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .expect("paste prompt");
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("progress.sh"),
        common::DEFAULT_TIMEOUT,
        "batch input",
    );
    sess.send("\r").expect("submit");
    common::expect_screen(
        &sess,
        |s| {
            s.lines()
                .any(|l| l.contains('⟳') && l.contains("PROGRESS-FIRST"))
        },
        Duration::from_secs(60),
        "live tool progress",
    );
    std::fs::write(env.workspace_root().join("release-progress"), "release").expect("release tool");
    let screen = common::expect_screen_settled(
        &sess,
        |s| common::screen_contains(s, "Concurrency batch done.") && s.contains("PROGRESS-RESULT"),
        Duration::from_secs(90),
        "completed tool",
    );
    assert!(
        !screen.contains('⟳'),
        "transient progress leaked into history: {screen}"
    );
    assert!(
        screen.contains("PROGRESS-FIRST"),
        "complete result lost: {screen}"
    );
    assert!(
        screen.contains("Terminal progress check."),
        "prior history was erased: {screen}"
    );
    finish(&mut sess);
}

#[cfg(unix)]
#[test]
fn asynchronous_tool_result_does_not_close_a_streaming_code_fence() {
    let env = TestEnv::new_mock("render-fence-interleaving");
    std::fs::write(
        env.workspace_root().join("fence.sh"),
        "while test ! -f release-tool; do sleep 0.1; done; printf 'FENCE_TOOL_RESULT\\n'; printf finished > fence-finished",
    )
    .expect("tool script");
    let _release_files = ReleaseFiles {
        root: env.workspace_root().to_path_buf(),
        release: &["release-tool", "release-fence"],
        finished: "fence-finished",
    };
    let calls = serde_json::json!([{
        "id": "toolu_render_fence", "name": "bash", "input": {"command":"sh fence.sh"},
        "stream_wait_for": env.workspace_root().join("release-fence"),
        "stream_text_chunks": [
            "FENCE STREAM READY.\n\n```rust\nlet FENCE_FIRST = \"START\";\n",
            "let FENCE_SECOND = \"END\";\n```\n\nAnswer after fence.\n",
        ],
    }]);
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
            ("NO_COLOR", ""),
        ],
    );
    sess.resize(100, 80).expect("resize");
    common::expect_input_line_cleared(&sess, Duration::from_secs(30), "initial prompt");
    sess.send(&format!(
        "\x1b[200~PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}\x1b[201~"
    ))
    .expect("paste batch");
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("TOOL_BATCH:"),
        common::DEFAULT_TIMEOUT,
        "batch input",
    );
    sess.send("\r").expect("submit");
    // A complete paragraph in the same delta proves the open fence has reached
    // the renderer. The provider then waits while the real Bash tool finishes.
    common::expect_screen(
        &sess,
        |s| {
            s.lines()
                .any(|row| row.contains("FENCE STREAM READY.") && !row.contains("TOOL_BATCH:"))
        },
        Duration::from_secs(30),
        "streamed prefix",
    );
    std::fs::write(env.workspace_root().join("release-tool"), "release").expect("release tool");
    common::expect_screen(
        &sess,
        |s| s.contains("FENCE_TOOL_RESULT"),
        common::DEFAULT_TIMEOUT,
        "tool result before provider finishes the fence",
    );
    std::fs::write(env.workspace_root().join("release-fence"), "release").expect("release stream");
    let screen = common::expect_screen_settled(
        &sess,
        |s| s.contains("Concurrency batch done.") && s.contains("Answer after fence."),
        Duration::from_secs(30),
        "complete fenced response",
    );
    for content in [
        "let FENCE_FIRST = \"START\";",
        "let FENCE_SECOND = \"END\";",
        "Answer after fence.",
    ] {
        assert_eq!(
            screen
                .lines()
                .filter(|row| !row.contains("TOOL_BATCH:"))
                .filter(|row| row.contains(content))
                .count(),
            1,
            "{screen}"
        );
    }
    let colors = sess.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        [
            "let FENCE_FIRST = \"START\";",
            "let FENCE_SECOND = \"END\";",
        ]
        .iter()
        .map(|content| {
            let row = rows.iter().rposition(|row| row.trim() == *content).unwrap();
            let column = rows[row].find('"').unwrap() + 1;
            let column = rows[row][..column].chars().count();
            format!(
                "{:?}",
                raw.cell(row as u16, column as u16).unwrap().fgcolor()
            )
        })
        .collect::<Vec<_>>()
    });
    assert_ne!(colors[0], "Default", "code must be highlighted");
    assert_eq!(
        colors[0], colors[1],
        "code fence lost its parser context: {screen}"
    );
    finish(&mut sess);
}

/// Release an owned subprocess before its workspace disappears, even on panic.
#[cfg(unix)]
struct ReleaseFiles {
    root: std::path::PathBuf,
    release: &'static [&'static str],
    finished: &'static str,
}

#[cfg(unix)]
impl Drop for ReleaseFiles {
    fn drop(&mut self) {
        for name in self.release {
            let _ = std::fs::write(self.root.join(name), "release");
        }
        let _ = common::read_file_with_retry(
            &self.root.join(self.finished),
            10,
            Duration::from_millis(50),
        );
    }
}
