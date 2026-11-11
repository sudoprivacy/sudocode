//! Exercise shaded prompts on the shared native-history path through real PTYs.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::json;

fn start(env: &TestEnv, enabled: bool, light: bool) -> PtySession {
    let session = env.spawn_with_env_at_size(
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
        32,
        96,
    );
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
fn streaming_and_resume_share_the_inline_editor() {
    for enabled in [false, true] {
        let env = TestEnv::new_mock("prompt-native-stream");
        let mut session = start(&env, enabled, false);
        let marker = common::turn_status_marker(&session);
        begin_history(&env, &mut session);
        session.send("draft界").unwrap();
        common::expect_input_line(&session, "draft界", env.timeout(), "draft during streaming");
        std::fs::write(env.workspace_root().join("prose-release"), "release").unwrap();
        common::expect_screen_settled(
            &session,
            |screen| {
                let status = common::turn_status_identity(screen);
                !status.is_empty() && status != marker && common::input_line_of(screen) == "draft界"
            },
            env.timeout(),
            "history handoff preserves the draft",
        );
        session.resize(36, 64).unwrap();
        common::expect_input_line(&session, "draft界", env.timeout(), "draft survives resize");
        finish(&mut session, &env);
        let requests_before = env.captured_message_bodies().len();
        let mut resumed = env.spawn_with_env_at_size(
            &["--resume", "latest", "--permission-mode", "read-only"],
            &[
                ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                (
                    "SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT",
                    if enabled { "1" } else { "0" },
                ),
            ],
            32,
            96,
        );
        common::expect_screen(
            &resumed,
            |s| s.contains("ViewportRow089") && s.contains('❯'),
            env.timeout(),
            "resume replays history through the existing native output path",
        );
        common::expect_input_line(&resumed, "", env.timeout(), "resumed editor is ready");
        assert_eq!(
            env.captured_message_bodies().len(),
            requests_before,
            "presentation resume makes no model request"
        );
        finish(&mut resumed, &env);
    }
}

#[test]
fn live_and_resumed_prompts_share_the_editor_surface() {
    for light in [false, true] {
        let env = TestEnv::new_mock("prompt-native-style");
        let mut session = start(&env, true, light);
        session.send("surface-draft").unwrap();
        common::expect_input_line(&session, "surface-draft", env.timeout(), "shaded editor");
        let surface = prompt_surface(&session, "surface-draft", light);
        session.send("\x15").unwrap();
        let marker = common::turn_status_marker(&session);
        let prompt = env.prompt("Say hello briefly", "single_turn_text");
        session.send(&format!("{prompt}\r")).unwrap();
        common::expect_turn_complete_after(&session, &marker, env.timeout(), "answer completes");
        assert_eq!(
            prompt_surface(&session, "Say hello briefly", light),
            surface,
            "submitted and live prompts use one style"
        );
        finish(&mut session, &env);
        let mut resumed = env.spawn_with_env_at_size(
            &["--resume", "latest", "--permission-mode", "read-only"],
            &[
                ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                ("SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT", "1"),
                ("TERM", "xterm-256color"),
                ("COLORTERM", "truecolor"),
                ("NO_COLOR", ""),
                ("COLORFGBG", if light { "0;15" } else { "15;0" }),
            ],
            32,
            96,
        );
        common::expect_screen(
            &resumed,
            |s| s.contains("Say hello briefly") && s.contains('❯'),
            env.timeout(),
            "shaded prompt restored",
        );
        common::expect_input_line(&resumed, "", env.timeout(), "resumed editor is ready");
        assert_eq!(
            prompt_surface(&resumed, "Say hello briefly", light),
            surface,
            "resume reuses the same prompt formatter and style"
        );
        finish(&mut resumed, &env);
    }
}

/// A committed paste stands apart from what was typed. The shaded surface sets
/// the prompt's foreground explicitly and faint over it barely shows, so there
/// a paste takes a muted foreground; without the surface it stays faint.
#[test]
fn a_committed_paste_stands_apart_from_typed_text() {
    for (enabled, light) in [(true, false), (true, true), (false, false)] {
        let env = TestEnv::new_mock("prompt-paste-mark");
        let mut session = start(&env, enabled, light);
        common::expect_input_line_cleared(&session, env.timeout(), "editor ready");
        let typed = env.prompt("TypedPart", "single_turn_text");
        session.send(&format!("{typed} ")).unwrap();
        // Only a paste that collapses to a placeholder is recorded as pasted.
        session
            .send("\x1b[200~PastedPart one\nPastedLine two\nPastedLine three\x1b[201~")
            .unwrap();
        common::expect_screen(
            &session,
            |s| s.contains("[Pasted text #"),
            env.timeout(),
            "paste collapsed in the editor",
        );
        let marker = common::turn_status_marker(&session);
        session.send("\r").unwrap();
        common::expect_turn_complete_after(&session, &marker, env.timeout(), "answer completes");
        let typed = cell_style(&session, "TypedPart");
        for needle in ["PastedPart", "PastedLine two"] {
            let pasted = cell_style(&session, needle);
            assert_eq!(pasted.1, typed.1, "{needle} keeps the prompt background");
            if enabled {
                assert_ne!(pasted.0, typed.0, "{needle} takes a muted foreground");
                assert!(!pasted.2, "{needle} is not faint on top of it");
            } else {
                assert!(
                    pasted.2 && !typed.2,
                    "{needle} is faint without the surface"
                );
            }
        }
        finish(&mut session, &env);
    }
}

/// Foreground, background and faint of the first cell of the last `needle`.
fn cell_style(session: &PtySession, needle: &str) -> (String, String, bool) {
    session.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<String> = raw.rows(0, raw.size().1).collect();
        let (row, line) = rows
            .iter()
            .enumerate()
            .rev()
            .find(|(_, line)| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle}: {}", screen.contents()));
        let col = line[..line.find(needle).unwrap()].chars().count();
        let cell = raw
            .cell(row.try_into().unwrap(), col.try_into().unwrap())
            .unwrap();
        (
            format!("{:?}", cell.fgcolor()),
            format!("{:?}", cell.bgcolor()),
            cell.dim(),
        )
    })
}

fn prompt_surface(session: &PtySession, text: &str, light: bool) -> (String, String) {
    session.render(|screen| {
        let raw = screen.raw();
        let row = raw
            .rows(0, raw.size().1)
            .position(|line| line.contains(text))
            .unwrap_or_else(|| panic!("missing prompt {text}: {}", screen.contents()));
        let cell = raw.cell(row.try_into().unwrap(), 0).unwrap();
        let foreground = format!("{:?}", cell.fgcolor());
        let background = format!("{:?}", cell.bgcolor());
        assert_eq!(
            foreground,
            if light { "Idx(0)" } else { "Idx(15)" },
            "prompt text stays readable"
        );
        assert_ne!(background, "Default", "prompt surface is shaded");
        assert_eq!(
            format!(
                "{:?}",
                raw.cell(row.try_into().unwrap(), 80).unwrap().bgcolor()
            ),
            background,
            "the prompt background fills the row"
        );
        (foreground, background)
    })
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
fn no_color_keeps_the_editor_plain() {
    let env = TestEnv::new_mock("prompt-viewport-no-color");
    let mut session = env.spawn_with_env_at_size(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT", "1"),
            ("NO_COLOR", "1"),
        ],
        32,
        96,
    );
    common::expect_screen(
        &session,
        |s| s.contains('❯'),
        env.timeout(),
        "plain inline editor",
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

#[cfg(not(windows))]
#[test]
#[ignore = "real browser lifecycle and process accounting; requires Playwright Chromium"]
fn viewport_browser_resource_exit() {
    run_viewport_observer_with_exit("prompt_viewport_browser.cjs", true, true);
}

fn run_viewport_observer(observer: &str, enabled: bool) {
    run_viewport_observer_with_exit(observer, enabled, false);
}

fn run_viewport_observer_with_exit(observer: &str, enabled: bool, verify_process_exit: bool) {
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
    let logs = if verify_process_exit {
        logs.join("resource-exit")
    } else {
        logs
    };
    std::fs::create_dir_all(&logs).unwrap();
    let manifest = root.join("viewport-measurement.json");
    let report = logs.join("report.json");
    let config = json!({
        "binary": common::scode_bin(), "args": ["--auth", "api-key", "--model", "sonnet", "--permission-mode", "read-only"],
        "root": root, "configHome": env.config_home(), "todos": root.join("todos.json"),
        "backend": std::env::var("SCODE_CONPTY_BACKEND").unwrap_or_else(|_| "bundled".into()),
        "cols": 96, "rows": 40, "enabled": enabled, "prompt": prompt, "logRoot": logs, "report": report,
        "verifyProcessExit": verify_process_exit,
        "env": { "SUDOCODE_EXPERIMENT_PROMPT_VIEWPORT": if enabled { "1" } else { "0" } },
    });
    std::fs::write(&manifest, serde_json::to_vec(&config).unwrap()).unwrap();
    let output = std::process::Command::new(
        std::env::var_os("SCODE_TERMINAL_HOST").unwrap_or_else(|| "node".into()),
    )
    .arg(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rusty-sudocode-cli/tests/support")
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
