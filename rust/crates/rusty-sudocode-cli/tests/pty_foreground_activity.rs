//! Foreground lifecycle remains visible independently of transcript writes.
mod common;

use std::{fs, path::PathBuf, time::Duration};

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::json;

// The vt100 test parser can retain previous live rows on resize. Inspect the
// current chrome between the two separators rather than old screen history.
fn live_status(screen: &str) -> &str {
    let mut lines = screen.lines().rev();
    for _ in 0..2 {
        if lines.find(|line| line.starts_with("──")).is_none() {
            return "";
        }
    }
    // StatusSlot reserves space for the post-turn result. That padding is
    // intentional, so inspect the content row above it after a resize.
    lines
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim_start()
}

fn running_status(screen: &str) -> bool {
    let line = live_status(screen);
    line.chars().next().is_some_and(|glyph| {
        ('\u{2800}'..='\u{28ff}').contains(&glyph) || ['◐', '◓', '◑', '◒'].contains(&glyph)
    }) && [
        "Working...",
        "Thinking...",
        "Reasoning...",
        "Running",
        "Retry",
    ]
    .iter()
    .any(|label| line.contains(label))
}

fn submit(session: &mut PtySession, prompt: &str) {
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        session,
        |view| view.contains("Pasted") || common::screen_contains(view, prompt),
        common::DEFAULT_TIMEOUT,
        "prompt arrived before submission",
    );
    session.send("\r").unwrap();
}

fn start(env: &TestEnv) -> PtySession {
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(60, 100).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "initial prompt");
    session
}

fn finish(session: &mut PtySession, marker: &str) {
    common::expect_turn_complete_after(
        session,
        marker,
        common::LIVE_TURN_BUDGET,
        "foreground turn completed",
    );
    common::expect_screen(
        session,
        |view| !running_status(view) && view.contains("/help"),
        common::DEFAULT_TIMEOUT,
        "idle footer restored and activity removed",
    );
    session.send("/exit").unwrap();
    common::expect_input_line(
        session,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "exit command reached input",
    );
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

struct ReleaseFiles(PathBuf);

impl Drop for ReleaseFiles {
    fn drop(&mut self) {
        for name in ["release-tool", "release-stream"] {
            let _ = fs::write(self.0.join(name), "release");
        }
    }
}

fn streaming_prompt(env: &TestEnv) -> String {
    if env.is_mock() {
        let calls = json!([{
            "id":"activity-tool", "name":"bash", "input":{"command":"sh quiet.sh", "timeout":30000},
            "stream_wait_for":env.workspace_root().join("release-stream"),
            "stream_text_chunks":["ACTIVITY_STREAM_STARTED.\n\n", "ACTIVITY_STREAM_FINISHED.\n\n"]
        }]);
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        "This is a terminal activity test. Read quiet.sh and run sh quiet.sh with Bash in the foreground. \
         It writes tool-started, then waits for the test runner to create release-tool. \
         Do not create the release file or run the command in the background. After it exits, say done."
            .into()
    }
}

fn quiet_fixture(name: &str, marker: &str) -> TestEnv {
    let env = TestEnv::new(name);
    fs::write(
        env.workspace_root().join("quiet.sh"),
        format!("printf started > tool-started; while test ! -f release-tool; do sleep 0.1; done; printf '{marker}\\n'"),
    )
    .unwrap();
    env
}

#[test]
fn streaming_output_cannot_hide_a_running_foreground_turn() {
    let env = quiet_fixture("foreground-stream-activity", "ACTIVITY_TOOL_FINISHED");
    let mut session = start(&env);
    let _release = ReleaseFiles(env.workspace_root().to_path_buf());
    let marker = common::turn_status_marker(&session);
    let prompt = streaming_prompt(&env);
    submit(&mut session, &prompt);
    common::expect_screen(
        &session,
        |view| {
            env.workspace_root().join("tool-started").exists()
                && (!env.is_mock() || view.contains("ACTIVITY_STREAM_STARTED."))
        },
        common::LIVE_TURN_BUDGET,
        "quiet tool started while the provider is still active",
    );
    // Observe several real render ticks while both the tool and provider are
    // held open. A stale frame before the first streamed line cannot pass.
    for _ in 0..12 {
        common::expect_screen(
            &session,
            |view| running_status(view) && !view.contains("· /help"),
            Duration::from_secs(2),
            "foreground work must not look idle during transcript output",
        );
        std::thread::sleep(Duration::from_millis(80));
    }
    session.send("activity-draft").unwrap();
    common::expect_input_line(
        &session,
        "activity-draft",
        common::DEFAULT_TIMEOUT,
        "typing during work",
    );
    session.send("\x15").unwrap();
    common::expect_screen(
        &session,
        |view| common::input_line_of(view).is_empty() && running_status(view),
        common::DEFAULT_TIMEOUT,
        "draft clears while foreground animation continues",
    );
    fs::write(env.workspace_root().join("release-stream"), "go").unwrap();
    fs::write(env.workspace_root().join("release-tool"), "go").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("ACTIVITY_TOOL_FINISHED"),
        common::LIVE_TURN_BUDGET,
        "complete tool output preserved",
    );
    finish(&mut session, &marker);
}

#[test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "PTY render needs a higher-ranked closure over screen lifetimes."
)]
fn question_owns_the_status_until_answered_then_running_resumes() {
    let env = quiet_fixture("foreground-question-activity", "QUESTION_TOOL_FINISHED");
    let mut session = start(&env);
    let _release = ReleaseFiles(env.workspace_root().to_path_buf());
    let marker = common::turn_status_marker(&session);
    let prompt = if env.is_mock() {
        let calls = json!([
            {"id":"question", "name":"AskUserQuestion", "input":{"questions":[{"id":"choice","header":"Activity choice","question":"Choose the second answer","options":[{"label":"First","value":"First","description":"First choice"},{"label":"Second","value":"Second","description":"Second choice"}],"multiSelect":false}]}},
            {"id":"quiet", "name":"bash", "input":{"command":"sh quiet.sh", "timeout":30000}}
        ]);
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}")
    } else {
        "This is a terminal activity test. Use AskUserQuestion to ask 'Choose the second answer' \
         with options First and Second. After I answer, read quiet.sh and run sh quiet.sh with Bash \
         in the foreground. Only the test runner creates release-tool. After it exits, say done."
            .into()
    };
    submit(&mut session, &prompt);
    common::expect_screen_rows(
        &session,
        |view| {
            common::input_and_footer_of(view).is_some_and(|input| {
                input.lines().any(|line| {
                    line.starts_with("1/1") && line.contains("Choose the second answer")
                }) && input.contains("[2] Second")
            })
        },
        common::LIVE_TURN_BUDGET,
        "actual question options",
    );
    common::expect_screen(
        &session,
        |view| {
            view.contains("[2] Second")
                && !running_status(view)
                && !view.contains("Waiting for your input")
        },
        common::DEFAULT_TIMEOUT,
        "question panel owns input without an animation or duplicate waiting row",
    );
    let waiting = session.render(|screen| screen.contents());
    std::thread::sleep(Duration::from_millis(320));
    assert_eq!(
        session.render(|screen| screen.contents()),
        waiting,
        "waiting does not animate"
    );
    session.send("\x1b[B\r").unwrap();
    common::expect_screen(
        &session,
        |view| {
            env.workspace_root().join("tool-started").exists()
                && running_status(view)
                && !view.contains("Waiting for your input")
        },
        common::LIVE_TURN_BUDGET,
        "answer restores running status while quiet work remains",
    );
    fs::write(env.workspace_root().join("release-tool"), "go").unwrap();
    common::expect_screen(
        &session,
        |view| view.contains("QUESTION_TOOL_FINISHED"),
        common::LIVE_TURN_BUDGET,
        "remaining tool completed",
    );
    finish(&mut session, &marker);
}

#[test]
#[cfg(unix)]
#[allow(
    clippy::too_many_lines,
    reason = "Connected PTY workflow exercises overlap, animation, resizing and input."
)]
fn brand_animation_and_parallel_summary_share_one_status_row() {
    use nix::{libc, sys::stat::Mode, unistd::mkfifo};
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, time::Instant};

    let env = TestEnv::new_mock("brand-parallel-status");
    for name in ["first.fifo", "second.fifo"] {
        mkfifo(
            &env.workspace_root().join(name),
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .unwrap();
    }
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    session.resize(60, 100).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "initial prompt");
    let marker = common::turn_status_marker(&session);
    let calls = json!([
        {"id":"first", "name":"bash", "input":{"command":"cat first.fifo", "timeout":30000}},
        {"id":"second", "name":"Bash", "input":{"command":"cat second.fifo", "timeout":30000}}
    ]);
    submit(
        &mut session,
        &format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{calls}"),
    );
    // Both nonblocking writers can open only after both real readers start.
    // The commands are read-only, so the scheduler may overlap them.
    let writer = |name: &str| {
        let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
        loop {
            match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(env.workspace_root().join(name))
            {
                Ok(file) => break file,
                Err(error)
                    if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("FIFO reader did not start: {name}: {error}"),
            }
        }
    };
    let mut first = writer("first.fifo");
    let mut second = writer("second.fifo");
    common::expect_screen(
        &session,
        |view| {
            let line = live_status(view);
            line.contains("Running 2 tools…") && line.contains("[claude")
        },
        common::DEFAULT_TIMEOUT,
        "both running calls share one aggregate status",
    );
    let first_frame = session.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw
            .rows(0, raw.size().1)
            .enumerate()
            .filter(|(_, line)| line.contains("Running 2 tools…") && line.contains("[claude"))
            .collect();
        assert_eq!(rows.len(), 1, "one live status row");
        let (row, line) = &rows[0];
        assert!(!line.contains('🦀') && !line.contains('🧠'));
        let logo: String = line.chars().take(4).collect();
        assert!(logo
            .chars()
            .all(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)));
        for col in 0..4 {
            assert_eq!(
                format!(
                    "{:?}",
                    raw.cell(u16::try_from(*row).unwrap(), col)
                        .unwrap()
                        .fgcolor()
                ),
                "Idx(214)",
                "logo uses brand amber"
            );
        }
        for (text, color) in [
            ("Running", "Rgb(166, 227, 161)"),
            ("[claude", "Idx(247)"),
            ("s)", "Idx(247)"),
        ] {
            let column = unicode_width::UnicodeWidthStr::width(&line[..line.find(text).unwrap()]);
            let cell = raw
                .cell(u16::try_from(*row).unwrap(), u16::try_from(column).unwrap())
                .unwrap();
            assert_eq!(
                format!("{:?}", cell.fgcolor()),
                color,
                "activity stays distinct from secondary metadata: {text}"
            );
            assert!(!cell.dim(), "muted metadata uses a readable theme color");
        }
        logo
    });
    common::expect_screen(
        &session,
        |view| {
            let line = live_status(view);
            line.contains("Running 2 tools…")
                && line.chars().take(4).collect::<String>() != first_frame
        },
        Duration::from_secs(2),
        "brand frames animate",
    );
    session.send("brand-draft").unwrap();
    common::expect_input_line(
        &session,
        "brand-draft",
        common::DEFAULT_TIMEOUT,
        "typing beside animation",
    );
    session.send("\x15").unwrap();
    session.resize(60, 44).unwrap();
    common::expect_screen(
        &session,
        |view| {
            let line = live_status(view);
            line.contains("Running")
                && !line.contains("Running 2 tools…")
                && line.contains("[claude")
                && line.contains("s)")
        },
        common::DEFAULT_TIMEOUT,
        "narrow status truncates the description before metadata",
    );
    session.resize(60, 100).unwrap();
    common::expect_screen(
        &session,
        |view| live_status(view).contains("Running 2 tools…"),
        common::DEFAULT_TIMEOUT,
        "widening restores the full tool summary",
    );
    writeln!(first, "first_DONE").unwrap();
    drop(first);
    common::expect_screen(
        &session,
        |view| {
            let line = live_status(view);
            view.contains("first_DONE")
                && line.contains("Running Bash…")
                && line.contains("[claude")
        },
        common::DEFAULT_TIMEOUT,
        "completion updates the remaining tool summary",
    );
    writeln!(second, "second_DONE").unwrap();
    drop(second);
    finish(&mut session, &marker);
}

/// Compare input and process cost while a real tool and streamed text are held
/// open. Baseline is allowed to hide its spinner: that is the bug under test.
#[test]
#[ignore = "manual release A/B measurement"]
fn release_foreground_activity_measurement() {
    use common::render_measurement::{measure_keys, report_resources};
    let env = quiet_fixture("foreground-activity-measurement", "ACTIVITY_TOOL_FINISHED");
    let mut session = start(&env);
    let _release = ReleaseFiles(env.workspace_root().to_path_buf());
    let marker = common::turn_status_marker(&session);
    submit(&mut session, &streaming_prompt(&env));
    common::expect_screen(
        &session,
        |view| {
            env.workspace_root().join("tool-started").exists()
                && (!env.is_mock() || view.contains("ACTIVITY_STREAM_STARTED."))
        },
        common::LIVE_TURN_BUDGET,
        "foreground workload in flight",
    );
    report_resources("foreground_before_wait");
    std::thread::sleep(Duration::from_secs(5));
    report_resources("foreground_after_wait");
    measure_keys(&mut session, "foreground_stream_and_tool");
    report_resources("foreground_after_input");
    fs::write(env.workspace_root().join("release-stream"), "go").unwrap();
    fs::write(env.workspace_root().join("release-tool"), "go").unwrap();
    finish(&mut session, &marker);
}
