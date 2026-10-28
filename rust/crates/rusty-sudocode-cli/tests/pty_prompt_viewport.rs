//! Exercise the opt-in viewport through the real binary and terminal parser.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::json;

fn start(env: &TestEnv, enabled: bool, light: bool) -> PtySession {
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            (
                "SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT",
                if enabled { "1" } else { "0" },
            ),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", ""),
            ("COLORFGBG", if light { "0;15" } else { "15;0" }),
        ],
    );
    session.resize(32, 96).unwrap();
    common::expect_screen(
        &session,
        |s| s.contains('❯'),
        env.timeout(),
        "initial editor",
    );
    session
}

fn finish(session: &mut PtySession, env: &TestEnv) {
    session.send("\x15/exit").unwrap();
    common::expect_input_line(session, "/exit", env.timeout(), "exit entered");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

fn begin_history(env: &TestEnv, session: &mut PtySession) {
    let document = (0..90).fold(String::new(), |mut fixture_text, row| {
        let _ = std::fmt::Write::write_fmt(
            &mut fixture_text,
            format_args!("ViewportRow{row:03} ordinary answer text.\n\n"),
        );
        fixture_text
    });
    let prompt = common::terminal_host::prose_prompt(
        env,
        &document,
        json!({ "chunk_chars": 8192, "delay_ms": 1 }),
    );
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_screen(
        session,
        |s| s.contains("ViewportRow089") && env.workspace_root().join("prose-ready").exists(),
        env.timeout(),
        "stream reached final paragraph",
    );
}

#[test]
fn scrolling_keeps_the_editor_and_context_and_preserves_the_draft() {
    let env = TestEnv::new_mock("prompt-viewport-scroll");
    let mut session = start(&env, true, false);
    let marker = common::turn_status_marker(&session);
    begin_history(&env, &mut session);
    session.send("draft界").unwrap();
    common::expect_input_line(&session, "draft界", env.timeout(), "draft entered");
    session.send("\x1b[5~").unwrap();
    common::expect_screen(
        &session,
        |s| !s.contains("ViewportRow089") && s.contains("ViewportRow"),
        env.timeout(),
        "browse older answer",
    );
    assert_browsing_editor_surface(&session);
    let before = session.render(|screen| {
        screen
            .raw()
            .rows(0, screen.raw().size().1)
            .skip(1)
            .take(6)
            .collect::<Vec<_>>()
    });
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    common::expect_screen_settled(
        &session,
        |screen| {
            let status = common::turn_status_identity(screen);
            !status.is_empty() && status != marker && common::input_line_of(screen) == "draft界"
        },
        env.timeout(),
        "turn ends while retaining the draft and reading position",
    );
    assert_eq!(
        before,
        session.render(|screen| screen
            .raw()
            .rows(0, screen.raw().size().1)
            .skip(1)
            .take(6)
            .collect::<Vec<_>>()),
        "stream completion preserves the reading anchor"
    );
    common::expect_input_line(
        &session,
        "draft界",
        env.timeout(),
        "draft survives streaming",
    );
    session.resize(36, 64).unwrap();
    common::expect_input_line(&session, "draft界", env.timeout(), "draft survives reflow");
    session.send("\x1b[<64;20;5M").unwrap();
    common::expect_input_line(
        &session,
        "draft界",
        env.timeout(),
        "mouse wheel preserves editor",
    );
    session.send("\x1b[1;3F").unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("ViewportRow089"),
        env.timeout(),
        "return to latest",
    );
    finish(&mut session, &env);
    let requests_before = env.captured_message_bodies().len();
    let mut resumed = env.spawn_with_env(
        &["--resume", "latest", "--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT", "1"),
        ],
    );
    resumed.resize(32, 96).unwrap();
    common::expect_screen(
        &resumed,
        |s| s.contains("ViewportRow089") && s.contains("PROSE_CONTROL"),
        env.timeout(),
        "resume restores the answer and its semantic prompt header",
    );
    assert_eq!(
        env.captured_message_bodies().len(),
        requests_before,
        "presentation resume makes no model request"
    );
    finish(&mut resumed, &env);
}

#[test]
fn shaded_prompts_replay_into_native_history_on_exit() {
    let env = TestEnv::new_mock("prompt-viewport-replay");
    let mut session = start(&env, true, true);
    let marker = common::turn_status_marker(&session);
    let prompt = env.prompt("Say hello briefly", "single_turn_text");
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(&session, &marker, env.timeout(), "answer completes");
    session.render(|screen| {
        let raw = screen.raw();
        let row = raw
            .rows(0, raw.size().1)
            .position(|row| row.contains("Say hello briefly"))
            .expect("submitted user prompt visible");
        assert_eq!(
            format!(
                "{:?}",
                raw.cell(row.try_into().unwrap(), 0).unwrap().fgcolor()
            ),
            "Idx(0)",
            "light prompt surface keeps dark text"
        );
        assert_ne!(
            format!(
                "{:?}",
                raw.cell(row.try_into().unwrap(), 1).unwrap().bgcolor()
            ),
            "Default"
        );
    });
    finish(&mut session, &env);
    assert!(
        session
            .render(common::screen_contents)
            .contains("Say hello briefly"),
        "native record restored after the viewport closes"
    );
}

#[test]
fn unfinished_long_prose_keeps_prompt_context_and_the_draft() {
    let env = TestEnv::new_mock("prompt-viewport-live-context");
    let mut session = start(&env, true, false);
    let marker = common::turn_status_marker(&session);
    let document = format!(
        "ViewportLiveStart {} ViewportLiveEnd",
        "ordinary flowing words ".repeat(160)
    );
    let prompt = common::terminal_host::prose_prompt(
        &env,
        &document,
        json!({ "chunk_chars": 8192, "delay_ms": 1 }),
    );
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("ViewportLiveEnd") && env.workspace_root().join("prose-ready").exists(),
        env.timeout(),
        "unfinished paragraph is visible before EOF",
    );
    session.render(|screen| {
        assert!(
            screen
                .raw()
                .rows(0, screen.raw().size().1)
                .take(2)
                .any(|row| row.contains("PROSE_CONTROL")),
            "prompt context remains above the live paragraph: {}",
            screen.contents()
        );
    });
    session.send("long-draft").unwrap();
    common::expect_input_line(
        &session,
        "long-draft",
        env.timeout(),
        "typing during an unfinished paragraph",
    );
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    common::expect_screen_settled(
        &session,
        |screen| {
            let status = common::turn_status_identity(screen);
            !status.is_empty() && status != marker && common::input_line_of(screen) == "long-draft"
        },
        env.timeout(),
        "paragraph handoff preserves the draft",
    );
    finish(&mut session, &env);
}

#[test]
fn older_history_remains_readable_after_many_pages() {
    let env = TestEnv::new_mock("prompt-viewport-many-pages");
    let mut session = start(&env, true, false);
    let marker = common::turn_status_marker(&session);
    let document = (0..600).fold(String::new(), |mut fixture_text, row| {
        let _ = std::fmt::Write::write_fmt(
            &mut fixture_text,
            format_args!("ArchiveRow{row:03} ordinary answer text.\n\n"),
        );
        fixture_text
    });
    let prompt = common::terminal_host::prose_prompt(
        &env,
        &document,
        json!({ "chunk_chars": 8192, "delay_ms": 1 }),
    );
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("ArchiveRow599") && env.workspace_root().join("prose-ready").exists(),
        env.timeout(),
        "large history is available",
    );
    session.send("history-draft").unwrap();
    common::expect_input_line(
        &session,
        "history-draft",
        env.timeout(),
        "draft before paging",
    );
    session.send(&"\x1b[5~".repeat(150)).unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("Auth mode") && !s.contains("ArchiveRow599"),
        env.timeout(),
        "paging reaches the startup record beyond the layout cache",
    );
    // The banner can occupy more than one viewport. Walk overlapping pages
    // forward to the first answer instead of assuming the banner's height.
    for _ in 0..16 {
        if session.render(|screen| screen.contents().contains("ArchiveRow000")) {
            break;
        }
        let before = session.render(|screen| {
            screen
                .raw()
                .rows(0, screen.raw().size().1)
                .take(20)
                .collect::<Vec<_>>()
                .join("\n")
        });
        session.send("\x1b[6~").unwrap();
        common::expect_screen_rows(
            &session,
            |screen| screen.lines().take(20).collect::<Vec<_>>().join("\n") != before,
            env.timeout(),
            "page forward from the startup banner",
        );
    }
    common::expect_screen(
        &session,
        |s| s.contains("ArchiveRow000"),
        env.timeout(),
        "the first answer remains readable after cache eviction",
    );
    common::expect_input_line(
        &session,
        "history-draft",
        env.timeout(),
        "paging retains the draft",
    );
    session.send("\x1b[1;3F").unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("ArchiveRow599"),
        env.timeout(),
        "return from old history",
    );
    std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
    common::expect_screen_settled(
        &session,
        |screen| {
            let status = common::turn_status_identity(screen);
            !status.is_empty()
                && status != marker
                && common::input_line_of(screen) == "history-draft"
        },
        env.timeout(),
        "large history completion preserves input",
    );
    finish(&mut session, &env);
}

#[test]
fn disabled_experiment_retains_inline_input() {
    let env = TestEnv::new_mock("prompt-viewport-off");
    let mut session = start(&env, false, false);
    session.send("plain-draft").unwrap();
    common::expect_input_line(&session, "plain-draft", env.timeout(), "normal editor");
    session.render(|screen| {
        let raw = screen.raw();
        let row = raw
            .rows(0, raw.size().1)
            .position(|row| row.contains("plain-draft"))
            .unwrap();
        assert_eq!(
            format!(
                "{:?}",
                raw.cell(row.try_into().unwrap(), 1).unwrap().bgcolor()
            ),
            "Default"
        );
    });
    finish(&mut session, &env);
}

#[test]
fn no_color_keeps_the_pinned_editor_plain() {
    let env = TestEnv::new_mock("prompt-viewport-no-color");
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT", "1"),
            ("NO_COLOR", "1"),
        ],
    );
    session.resize(32, 96).unwrap();
    common::expect_screen(
        &session,
        |s| s.contains('❯'),
        env.timeout(),
        "plain pinned editor",
    );
    session.send("plain-surface").unwrap();
    common::expect_input_line(&session, "plain-surface", env.timeout(), "plain draft");
    session.render(|screen| {
        let raw = screen.raw();
        let row = raw
            .rows(0, raw.size().1)
            .position(|row| row.contains("plain-surface"))
            .unwrap();
        assert_eq!(
            format!(
                "{:?}",
                raw.cell(row.try_into().unwrap(), 80).unwrap().bgcolor()
            ),
            "Default"
        );
    });
    finish(&mut session, &env);
}

#[test]
#[ignore = "explicit release-mode viewport measurements; requires the pinned terminal host"]
fn viewport_terminal_measurements() {
    let enabled = std::env::var("SCODE_VIEWPORT_MEASURE_ENABLED").as_deref() == Ok("1");
    run_viewport_observer("prompt_viewport.cjs", enabled);
}

#[test]
#[ignore = "real browser painting, wheel and native selection; requires Playwright Chromium"]
fn viewport_browser_interaction() {
    let enabled = std::env::var("SCODE_VIEWPORT_MEASURE_ENABLED").as_deref() != Ok("0");
    run_viewport_observer("prompt_viewport_browser.cjs", enabled);
}

fn run_viewport_observer(observer: &str, enabled: bool) {
    let env = TestEnv::new_mock("prompt-viewport-measurements");
    let root = env.workspace_root();
    let document = (0..90).fold(String::new(), |mut fixture_text, row| {
        let _ = std::fmt::Write::write_fmt(
            &mut fixture_text,
            format_args!("ViewportRow{row:03} ordinary answer text.\n\n"),
        );
        fixture_text
    });
    let prompt = common::terminal_host::prose_prompt(
        &env,
        &document,
        json!({ "chunk_chars": 8192, "delay_ms": 1 }),
    );
    let logs = std::env::var_os("SCODE_VIEWPORT_MEASURE_DIR").map_or_else(
        || root.join("viewport-measurements"),
        std::path::PathBuf::from,
    );
    std::fs::create_dir_all(&logs).unwrap();
    let manifest = root.join("viewport-measurement.json");
    let report = logs.join("report.json");
    let config = json!({
        "binary": common::scode_bin(), "args": ["--auth", "api-key", "--model", "sonnet", "--permission-mode", "read-only"],
        "root": root, "configHome": env.config_home(), "todos": root.join("todos.json"),
        "backend": std::env::var("SCODE_CONPTY_BACKEND").unwrap_or_else(|_| "bundled".into()),
        "cols": 96, "rows": 40, "enabled": enabled, "prompt": prompt, "logRoot": logs, "report": report,
        "env": { "SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT": if enabled { "1" } else { "0" } },
    });
    std::fs::write(&manifest, serde_json::to_vec(&config).unwrap()).unwrap();
    let output = std::process::Command::new(
        std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into()),
    )
    .arg(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/support")
            .join(observer),
    )
    .arg(manifest)
    .env("ELECTRON_RUN_AS_NODE", "1")
    .output()
    .unwrap();
    common::terminal_host::assert_success(&output);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(value["status"], "complete", "{value}");
    assert_eq!(value["enabled"], enabled);
}

fn assert_browsing_editor_surface(session: &PtySession) {
    session.render(|screen| {
        let raw = screen.raw();
        assert!(
            raw.rows(0, raw.size().1)
                .next()
                .unwrap()
                .contains("PROSE_CONTROL"),
            "context header: {}",
            screen.contents()
        );
        let input_row = raw
            .rows(0, raw.size().1)
            .position(|row| row.contains("draft界"))
            .unwrap();
        assert!(
            input_row >= 27,
            "composer remains at the bottom: {input_row}"
        );
        assert_ne!(
            format!(
                "{:?}",
                raw.cell(input_row.try_into().unwrap(), 80)
                    .unwrap()
                    .bgcolor()
            ),
            "Default",
            "filled composer"
        );
        assert_ne!(
            format!(
                "{:?}",
                raw.cell(input_row.try_into().unwrap(), 0)
                    .unwrap()
                    .fgcolor()
            ),
            "Default",
            "fallback surface keeps readable text"
        );
    });
}
