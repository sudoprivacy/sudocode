//! Background task discovery, inspection and control through the real terminal.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};
use std::{
    fs,
    time::{Duration, Instant},
};

// A failed assertion can kill the PTY before normal session teardown. Release
// only this fixture's waits so a failed run cannot leave orphan shell loops.
struct ReleaseOnDrop(std::path::PathBuf);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        for file in [
            "release",
            "release-one",
            "release-two",
            "ci-ready",
            "foreground-release",
        ] {
            let _ = fs::write(self.0.join(file), "released");
        }
    }
}
fn task_env(label: &str) -> (TestEnv, ReleaseOnDrop) {
    let env = TestEnv::new(label);
    let release = ReleaseOnDrop(env.workspace_root().to_path_buf());
    (env, release)
}

fn expect_input_ready(session: &PtySession, budget: Duration, context: &str) {
    common::expect_screen_settled(
        session,
        |_| {
            session.render(|screen| {
                // contents() joins soft-wrapped rows. At 80 columns the input
                // can otherwise appear to contain the following rule/footer.
                let rows = screen
                    .raw()
                    .rows(0, screen.raw().size().1)
                    .collect::<Vec<_>>()
                    .join("\n");
                rows.contains('\u{276f}') && common::input_line_of(&rows).is_empty()
            })
        },
        budget,
        context,
    );
}

fn expect_parent_complete(session: &PtySession, before: &str) {
    common::expect_screen(
        session,
        |_| {
            let after = common::turn_status_marker(session);
            !after.is_empty() && after != before
        },
        Duration::from_secs(180),
        "background launch turn completed",
    );
    expect_input_ready(session, common::DEFAULT_TIMEOUT, "parent returned to input");
}

fn start(env: &TestEnv, calls: &[Value]) -> PtySession {
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(40, 100).unwrap();
    expect_input_ready(&session, Duration::from_secs(30), "ready");
    let before = common::turn_status_marker(&session);
    send_calls(env, &mut session, calls);
    if calls
        .iter()
        .all(|call| call["name"] == "Bash" && call["input"]["run_in_background"] == true)
    {
        // A footer proves a task exists, not that the launch turn has finished.
        // Keep question workflows interactive while their parent is pending.
        expect_parent_complete(&session, &before);
    }
    session
}

fn send_calls(env: &TestEnv, session: &mut PtySession, calls: &[Value]) {
    let batch = serde_json::to_string(calls).unwrap();
    let prompt = if env.is_mock() {
        format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{batch}")
    } else {
        format!("I am testing the background tasks UI and process cleanup in this disposable workspace. The shell commands use fixture release files: my PTY test harness creates these after inspecting output or closing the session, and cleans up the fixture jobs. Please issue these tool calls together in one message using the supplied inputs: {batch}. After they return, reply briefly and finish the turn. Do not poll background tasks, wait for them or issue extra tools.")
    };
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        session,
        |s| {
            let input = common::input_line_of(s);
            input.contains("Pasted")
                || input.contains("TOOL_BATCH:")
                || input.contains("I am testing")
        },
        common::DEFAULT_TIMEOUT,
        "input",
    );
    session.send("\r").unwrap();
}

fn shell(id: &str, command: &str, description: &str) -> Value {
    json!({"id":id,"name":"Bash","input":{"command":command,"description":description,"run_in_background":true}})
}

fn screen(session: &PtySession, text: &str) -> String {
    common::expect_screen(
        session,
        |s| {
            s.rsplit_once("Background tasks")
                .map_or(s, |(_, panel)| panel)
                .contains(text)
        },
        Duration::from_secs(90),
        text,
    )
}

fn open(session: &mut PtySession) {
    session.send("\x1b[B").unwrap();
    screen(session, "Enter to view");
    session.send("\r").unwrap();
    screen(session, "Enter details");
    session.send("\r").unwrap();
    screen(session, "Esc task list");
}

fn close(mut session: PtySession) {
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b").unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "back at input");
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn background_shell_footer_and_details_keep_output_after_parent_finishes() {
    let (env, _release) = task_env("background-shell-details");
    let mut session = start(&env, &[shell("background", "echo LIVE_STDOUT; echo LIVE_STDERR >&2; while [ ! -f release ]; do sleep 0.1; done; echo FINAL_STDOUT", "Inspect background shell")]);
    let footer = screen(&session, "1 terminal · ↓ to view");
    assert!(
        footer
            .lines()
            .any(|line| line.contains("danger-full-access") && line.contains("1 terminal")),
        "share one footer row:\n{footer}"
    );
    open(&mut session);
    screen(&session, "  LIVE_STDOUT");
    screen(&session, "  LIVE_STDERR");
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    screen(&session, "  FINAL_STDOUT");
    screen(&session, "exit 0");
    close(session);
}

#[test]
fn failed_background_shell_exposes_unread_result_and_exit_code() {
    let (env, _release) = task_env("background-shell-failure");
    let mut session = start(
        &env,
        &[shell(
            "failed",
            "echo FAILURE_EVIDENCE >&2; exit 7",
            "Failed background job",
        )],
    );
    screen(&session, "1 new result · ↓ to view");
    open(&mut session);
    screen(&session, "  FAILURE_EVIDENCE");
    screen(&session, "exit 7");
    let view = session.render(|s| s.contents());
    assert!(view.contains("failed"), "failure status:\n{view}");
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b").unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "result read");
    let view = session.render(|s| s.contents());
    assert!(
        !view.contains("new result"),
        "read result must hide footer:\n{view}"
    );
    session.send("/ps\r").unwrap();
    screen(&session, "Enter details");
    session.send("\r").unwrap();
    screen(&session, "  FAILURE_EVIDENCE");
    close(session);
}

#[test]
fn inspecting_running_task_does_not_acknowledge_its_future_result() {
    let (env, _release) = task_env("background-result-after-peek");
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    expect_input_ready(&session, Duration::from_secs(30), "ready");
    assert!(!session.render(|s| s.contents()).contains("↓ to view"));
    let before = common::turn_status_marker(&session);
    send_calls(
        &env,
        &mut session,
        &[shell(
            "background",
            "echo PEEK_RUNNING; while [ ! -f release ]; do sleep 0.1; done; echo PEEK_FINISHED",
            "Inspect before completion",
        )],
    );
    screen(&session, "1 terminal · ↓ to view");
    expect_parent_complete(&session, &before);
    open(&mut session);
    screen(&session, "  PEEK_RUNNING");
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b").unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "back at input");
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    screen(&session, "1 new result · ↓ to view");
    open(&mut session);
    screen(&session, "  PEEK_FINISHED");
    close(session);
}

fn start_agent(env: &TestEnv, automatic: bool) -> PtySession {
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_AGENT_AUTO_BG_SECS", "1"),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        ],
    );
    session.resize(40, 100).unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "ready");
    let before = common::turn_status_marker(&session);
    let prompt = env.prompt(&format!("Use agent_spawn with agent Verification and run_in_background {}. Its task is to run `while [ ! -f ci-ready ]; do sleep 0.2; done; cat ci-result.txt` and return the exact file content. After spawning finish your turn immediately. Do not poll or wait in the parent. Report any completion notification verbatim.", !automatic), "subagent_workflow");
    session.send(&format!("{prompt}\r")).unwrap();
    common::expect_turn_complete_after(
        &session,
        &before,
        Duration::from_secs(180),
        "agent handed off",
    );
    session
}

#[test]
fn explicit_and_automatic_background_agents_share_the_task_browser() {
    for automatic in [false, true] {
        let (env, _release) = task_env("background-agent-browser");
        fs::write(
            env.workspace_root().join("ci-result.txt"),
            "AGENT_BROWSER_EVIDENCE",
        )
        .unwrap();
        let mut session = start_agent(&env, automatic);
        screen(&session, "1 agent · ↓ to view");
        open(&mut session);
        screen(&session, "ci-ready");
        fs::write(env.workspace_root().join("ci-ready"), "ready").unwrap();
        screen(&session, "AGENT_BROWSER_EVIDENCE");
        screen(&session, "completed");
        close(session);
    }
}

#[test]
fn stopping_one_background_task_keeps_its_sibling_running() {
    let (env, _release) = task_env("background-stop-one");
    let mut session = start(
        &env,
        &[
            shell(
                "one",
                "while [ ! -f release-one ]; do sleep 0.1; done; echo ONE_FINISHED",
                "Stop this task",
            ),
            shell(
                "two",
                "while [ ! -f release-two ]; do sleep 0.1; done; echo TWO_FINISHED",
                "Keep this task",
            ),
        ],
    );
    screen(&session, "2 terminals · ↓ to view");
    open(&mut session);
    session.send("k").unwrap();
    screen(&session, "cancelled");
    screen(&session, "1 terminal");
    // Both scripts are released; only the task not stopped may finish normally.
    fs::write(env.workspace_root().join("release-one"), "go").unwrap();
    fs::write(env.workspace_root().join("release-two"), "go").unwrap();
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b[A\r").unwrap();
    let result = common::expect_screen(
        &session,
        |s| s.contains("ONE_FINISHED") || s.contains("TWO_FINISHED"),
        Duration::from_secs(30),
        "surviving sibling output",
    );
    screen(&session, "exit 0");
    assert!(result.contains("FINISHED"));
    close(session);
}

#[test]
fn task_updates_do_not_take_keyboard_ownership_from_questions() {
    let (env, _release) = task_env("background-and-question");
    let mut session = start(
        &env,
        &[
            shell(
                "background",
                "echo QUESTION_BACKGROUND_OUTPUT; while [ ! -f release ]; do sleep 0.1; done",
                "Background during question",
            ),
            json!({"id":"question","name":"AskUserQuestion","input":{"questions":[{"id":"choice","header":"Choice","question":"Select the second answer","options":[{"label":"First","value":"First","description":"First option"},{"label":"Second","value":"Second","description":"Second option"}],"multiSelect":false}]}}),
        ],
    );
    common::expect_screen(
        &session,
        |view| {
            view.lines()
                .any(|line| line.starts_with("1/1") && line.contains("Select the second answer"))
                && view.contains("[2] Second")
        },
        Duration::from_secs(90),
        "actual question options, rather than the echoed prompt",
    );
    screen(&session, "1 terminal");
    session.send("\x1b[B\r").unwrap();
    common::expect_input_line_cleared(&session, Duration::from_secs(90), "question answered");
    if env.is_mock() {
        let bodies = env.captured_message_bodies();
        assert!(
            bodies.iter().any(|body| {
                let request: Value = serde_json::from_str(body).unwrap();
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["content"].as_array().is_some_and(|blocks| {
                            blocks.iter().any(|block| {
                                block["type"] == "tool_result"
                                    && block["tool_use_id"] == "question"
                                    && block["content"].to_string().contains("Second")
                            })
                        })
                    })
            }),
            "actual selected answer must reach provider"
        );
    }
    open(&mut session);
    screen(&session, "  QUESTION_BACKGROUND_OUTPUT");
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    close(session);
}

#[test]
fn task_browser_resizes_and_returns_to_fast_prompt_input() {
    let (env, _release) = task_env("background-resize-input");
    let mut session = start(
        &env,
        &[shell(
            "background",
            "echo RESIZE_OUTPUT; while [ ! -f release ]; do sleep 0.1; done",
            "A long background terminal title with 中文 and a wide emoji 🚀",
        )],
    );
    screen(&session, "1 terminal · ↓ to view");
    open(&mut session);
    session.resize(30, 45).unwrap();
    screen(&session, "  RESIZE_OUTPUT");
    session.resize(40, 120).unwrap();
    screen(&session, "Full output:");
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b").unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "browser closed");
    session.send("FAST_INPUT_AFTER_TASK_VIEW").unwrap();
    common::expect_input_line(
        &session,
        "FAST_INPUT_AFTER_TASK_VIEW",
        common::DEFAULT_TIMEOUT,
        "prompt remains responsive",
    );
    session.send("\x15").unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "draft cleared");
    session.send("\x1b[BFAST_INPUT_FROM_FOOTER").unwrap();
    common::expect_input_line(
        &session,
        "FAST_INPUT_FROM_FOOTER",
        common::DEFAULT_TIMEOUT,
        "typing returns footer focus to input without dropping batched keys",
    );
    session.send("\x15").unwrap();
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn leaving_the_session_reaps_background_processes() {
    let (env, _release) = task_env("background-session-close");
    let mut session = start(&env, &[shell("background", "echo READY_FOR_CLOSE; while [ ! -f release ]; do sleep 0.1; done; echo leaked > leaked.txt", "Session-owned job")]);
    screen(&session, "1 terminal · ↓ to view");
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert!(
            !env.workspace_root().join("leaked.txt").exists(),
            "background process survived session close"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn exiting_during_a_foreground_tool_reaps_both_process_groups() {
    let (env, _release) = task_env("background-close-during-turn");
    let mut session = start(
        &env,
        &[shell(
            "background",
            "while [ ! -f release ]; do sleep 0.1; done; echo leaked > background-leaked.txt",
            "Background cleanup fixture",
        )],
    );
    screen(&session, "1 terminal · ↓ to view");
    send_calls(
        &env,
        &mut session,
        &[
            json!({"id":"foreground","name":"Bash","input":{"command":"touch foreground-ready; while [ ! -f foreground-release ]; do sleep 0.1; done; echo leaked > foreground-leaked.txt","description":"Foreground cleanup fixture"}}),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    while !env.workspace_root().join("foreground-ready").exists() {
        assert!(
            Instant::now() < deadline,
            "foreground fixture never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    for file in ["release", "foreground-release"] {
        fs::write(env.workspace_root().join(file), "go").unwrap();
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        for file in ["background-leaked.txt", "foreground-leaked.txt"] {
            assert!(
                !env.workspace_root().join(file).exists(),
                "process survived session close: {file}"
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Run the same real-shell workload against baseline and candidate releases.
#[test]
#[ignore = "manual release A/B measurement"]
fn release_background_task_input_measurement() {
    use common::render_measurement::{measure_keys, report_resources};
    let (env, _release) = task_env("background-input-measurement");
    let mut session = start(&env, &[shell("background", "while [ ! -f release ]; do i=0; while [ $i -lt 200 ]; do echo PERFORMANCE_OUTPUT_LINE_0123456789; i=$((i+1)); done; sleep 0.2; done", "Background output workload")]);
    common::expect_screen(
        &session,
        |s| s.contains("Concurrency batch done.") && s.contains("ctx "),
        Duration::from_secs(30),
        "background launch complete",
    );
    report_resources("background_before_wait");
    std::thread::sleep(Duration::from_secs(5));
    report_resources("background_after_wait");
    measure_keys(&mut session, "background_output");
    report_resources("background_after_input");
    if session.render(|screen| screen.contents().contains("1 terminal · ↓ to view")) {
        open(&mut session);
        report_resources("background_detail_before_wait");
        std::thread::sleep(Duration::from_secs(5));
        report_resources("background_detail_after_wait");
        session.send("\x1b").unwrap();
        screen(&session, "Enter details");
        session.send("\x1b").unwrap();
        common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "detail closed");
    }
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn stopping_a_background_agent_cancels_its_running_child_tool() {
    let (env, _release) = task_env("background-agent-stop");
    fs::write(
        env.workspace_root().join("ci-result.txt"),
        "CANCELLED_AGENT_MUST_NOT_FINISH",
    )
    .unwrap();
    let mut session = start_agent(&env, false);
    screen(&session, "1 agent · ↓ to view");
    open(&mut session);
    screen(&session, "ci-ready");
    session.send("k").unwrap();
    screen(&session, "cancelled");
    fs::write(env.workspace_root().join("ci-ready"), "ready").unwrap();
    close(session);
    let manifest_dir = env.workspace_root().join(".sudocode-agents");
    for entry in fs::read_dir(manifest_dir).unwrap().flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            let manifest: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
            assert_ne!(
                manifest["status"], "completed",
                "stopped child cannot complete normally: {manifest}"
            );
        }
    }
}

#[test]
fn large_background_output_has_a_bounded_preview_and_a_complete_log() {
    let (env, _release) = task_env("background-bounded-output");
    let mut session = start(&env, &[shell("large", "echo LOG_PREAMBLE_STDOUT; echo LOG_PREAMBLE_STDERR >&2; echo FIRST_LOG_EVIDENCE; i=0; while [ $i -lt 2000 ]; do echo OUTPUT_FILLER_01234567890123456789; i=$((i+1)); done; echo LAST_LOG_EVIDENCE", "Large background output")]);
    screen(&session, "1 new result · ↓ to view");
    session.resize(45, 240).unwrap();
    open(&mut session);
    let view = screen(&session, "  LAST_LOG_EVIDENCE");
    screen(&session, "exit 0");
    let panel = view.rsplit_once("Background tasks").unwrap().1;
    assert!(
        !panel
            .lines()
            .any(|line| line.trim() == "FIRST_LOG_EVIDENCE"),
        "preview is a tail, not an unbounded log"
    );
    let path = panel
        .lines()
        .find_map(|line| line.trim().strip_prefix("Full output: "))
        .unwrap();
    let path = std::path::Path::new(path);
    assert_eq!(
        path.parent().unwrap().canonicalize().unwrap(),
        std::env::temp_dir()
            .join("sudocode-background-shells")
            .canonicalize()
            .unwrap()
    );
    let full = fs::read_to_string(path).unwrap();
    assert!(full.len() > 24 * 1024);
    // Login profiles may emit startup diagnostics before the fixture (MSYS2
    // does on first use). Keep that external output and verify the fixture's
    // complete ordered payload with either platform's line endings.
    let mut lines = full.lines();
    assert_eq!(
        lines.find(|line| *line == "LOG_PREAMBLE_STDOUT"),
        Some("LOG_PREAMBLE_STDOUT"),
        "full log prefix: {:?}",
        full.chars().take(200).collect::<String>()
    );
    assert_eq!(lines.next(), Some("LOG_PREAMBLE_STDERR"));
    assert_eq!(lines.next(), Some("FIRST_LOG_EVIDENCE"));
    for index in 0..2000 {
        assert_eq!(
            lines.next(),
            Some("OUTPUT_FILLER_01234567890123456789"),
            "full log filler record {index}"
        );
    }
    assert_eq!(lines.next(), Some("LAST_LOG_EVIDENCE"));
    assert!(lines.next().is_none(), "unexpected records after log end");
    close(session);
}
