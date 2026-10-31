//! Background task discovery, inspection and control through the real terminal.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

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
    common::expect_screen_rows_settled(
        session,
        |rows| rows.contains('\u{276f}') && common::input_line_of(rows).is_empty(),
        budget,
        context,
    );
}

fn expect_parent_complete(session: &PtySession, before: &str) {
    common::expect_turn_complete_after(
        session,
        before,
        common::LIVE_TURN_BUDGET,
        "background launch turn completed",
    );
    expect_input_ready(session, common::DEFAULT_TIMEOUT, "parent returned to input");
}

fn spawn_for_calls(env: &TestEnv, calls: &[Value]) -> PtySession {
    spawn_for_calls_with_env(env, calls, &[])
}

fn spawn_for_calls_with_env(env: &TestEnv, calls: &[Value], extra: &[(&str, &str)]) -> PtySession {
    // The declared workflow is the source of truth for the offered tool pool.
    // A POSIX Bash fixture must not silently become a PowerShell task on Windows.
    let allowed = calls
        .iter()
        .map(|call| tools::canonicalize_tool_name(call["name"].as_str().unwrap()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    let mut vars = vec![("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")];
    vars.extend_from_slice(extra);
    env.spawn_with_env(
        &[
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            &allowed,
        ],
        &vars,
    )
}

fn start(env: &TestEnv, calls: &[Value]) -> PtySession {
    start_with_env(env, calls, &[])
}

fn start_with_env(env: &TestEnv, calls: &[Value], extra: &[(&str, &str)]) -> PtySession {
    let mut session = spawn_for_calls_with_env(env, calls, extra);
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
    screen_with_budget(session, text, Duration::from_secs(90))
}

fn screen_with_budget(session: &PtySession, text: &str, budget: Duration) -> String {
    common::expect_screen(
        session,
        |s| {
            s.rsplit_once("Background tasks")
                .map_or(s, |(_, panel)| panel)
                .contains(text)
        },
        budget,
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
    let footer = screen(&session, "1 terminal");
    assert!(
        !footer.contains("↓ to view"),
        "idle task summary hides its navigation hint"
    );
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

fn assert_background_activity(session: &PtySession, view: &str, background: &str, no_color: bool) {
    let status = |view: &str| {
        view.lines()
            .find(|line| line.contains("1 task still running · turn "))
            .unwrap()
            .to_string()
    };
    let first = status(view);
    let logo: String = first.chars().take(4).collect();
    assert!(logo
        .chars()
        .all(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch)));
    let changed = common::expect_screen_rows(
        session,
        |view| {
            view.lines().any(|line| {
                line.contains("1 task still running · turn ") && !line.starts_with(&logo)
            })
        },
        common::DEFAULT_TIMEOUT,
        "background logo animates after the foreground finishes",
    );
    assert_eq!(
        first.chars().skip(4).collect::<String>(),
        status(&changed).chars().skip(4).collect::<String>(),
        "only the fixed-width icon changes; the task conclusion and turn stay still"
    );
    session.render(|screen| {
        let raw = screen.raw();
        let (row, line) = raw
            .rows(0, raw.size().1)
            .enumerate()
            .find(|(_, line)| line.contains("1 task still running · turn "))
            .unwrap();
        let (activity, metadata) = if no_color {
            ("Default", "Default")
        } else if background == "0;15" {
            ("Idx(94)", "Idx(241)")
        } else {
            ("Idx(214)", "Idx(247)")
        };
        // ConPTY may retain the preceding style on invisible separator spaces.
        // Assert every visible metadata glyph, including the separator itself.
        let metadata_start = line.find("· turn ").unwrap();
        let metadata_cells = line[metadata_start..]
            .char_indices()
            .filter(|(_, ch)| !ch.is_whitespace())
            .map(|(offset, _)| {
                (
                    unicode_width::UnicodeWidthStr::width(&line[..metadata_start + offset]),
                    metadata,
                )
            });
        for (column, color) in [(0, activity), (5, activity)]
            .into_iter()
            .chain(metadata_cells)
        {
            let cell = raw
                .cell(row.try_into().unwrap(), column.try_into().unwrap())
                .unwrap();
            assert_eq!(
                format!("{:?}", cell.fgcolor()),
                color,
                "status cell at row {row}, column {column}: {:?}; line {line:?}; nearby cells {:?}",
                cell.contents(),
                (column.saturating_sub(3)..(column + 20).min(usize::from(raw.size().1)))
                    .map(|column| {
                        let cell = raw
                            .cell(row.try_into().unwrap(), column.try_into().unwrap())
                            .unwrap();
                        (column, cell.contents(), format!("{:?}", cell.fgcolor()))
                    })
                    .collect::<Vec<_>>()
            );
            assert!(
                !cell.dim() && !cell.bold(),
                "theme spans have normal font weight"
            );
        }
    });
}

fn background_status_theme(background: &str, no_color: bool) {
    let (env, _release) = task_env("background-status-conclusion");
    let mut session = start_with_env(
        &env,
        &[shell(
            "status",
            "while [ ! -f release ]; do sleep 0.1; done; echo STATUS_RESULT_READY",
            "Status conclusion fixture",
        )],
        &[
            ("COLORFGBG", background),
            ("NO_COLOR", if no_color { "1" } else { "" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    let view = common::expect_screen_rows_settled(
        &session,
        |view| {
            view.lines().any(|line| {
                line.contains("1 task still running · turn ")
                    && line.trim_end().ends_with(" finished")
            }) && view
                .lines()
                .any(|line| line.contains("danger-full-access") && line.contains("1 terminal"))
        },
        common::LIVE_TURN_BUDGET,
        "foreground conclusion and task footer have distinct content",
    );
    assert_background_activity(&session, &view, background, no_color);
    session.send("后台-draft").unwrap();
    common::expect_input_line(
        &session,
        "后台-draft",
        common::DEFAULT_TIMEOUT,
        "idle typing",
    );
    session.resize(12, 32).unwrap();
    common::expect_screen_rows(
        &session,
        |view| {
            view.lines()
                .any(|line| line.trim_end().ends_with(" 1 running"))
        },
        common::DEFAULT_TIMEOUT,
        "compact status preserves the task conclusion",
    );
    session.resize(40, 100).unwrap();
    common::expect_input_line(
        &session,
        "后台-draft",
        common::DEFAULT_TIMEOUT,
        "resize retained draft",
    );
    session.send("\x15").unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "clear draft");
    let marker = common::turn_status_marker(&session);
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    common::expect_turn_complete_after(
        &session,
        &marker,
        common::LIVE_TURN_BUDGET,
        "completion notification starts and finishes a new foreground turn",
    );
    common::expect_screen_rows_settled(
        &session,
        |view| {
            let statistics = view.lines().position(|line| line.contains("· turn "));
            let unread = view.lines().position(|line| {
                line.contains("danger-full-access") && line.contains("1 new result")
            });
            matches!((statistics, unread), (Some(a), Some(b)) if a != b)
                && view.contains("ctx ")
                && !common::turn_status_line(view).contains("still running")
        },
        common::LIVE_TURN_BUDGET,
        "statistics and unread result remain on their own surfaces",
    );
    open(&mut session);
    screen(&session, "STATUS_RESULT_READY");
    close(session);
}

#[test]
fn dark_background_status_animates_and_results_preserve_turn_statistics() {
    background_status_theme("15;0", false);
}

#[test]
fn light_background_status_animates_and_results_preserve_turn_statistics() {
    background_status_theme("0;15", false);
}

#[test]
fn no_color_background_status_animates_and_results_preserve_turn_statistics() {
    background_status_theme("15;0", true);
}

#[test]
fn urgent_tip_shares_the_footer_and_survives_task_browsing_and_resize() {
    let (env, _release) = task_env("background-with-tip");
    let calls = [
        shell(
            "with-tip",
            "echo TIP_TASK_OUTPUT; while [ ! -f release ]; do sleep 0.1; done",
            "Tip alongside work",
        ),
        json!({"id":"advice","name":"set_tip","input":{"priority":"urgent","message":"REVIEW_JOB"}}),
    ];
    let mut session = spawn_for_calls(&env, &calls);
    session.resize(40, 100).unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "tip task ready");
    let before = common::turn_status_marker(&session);
    send_calls(&env, &mut session, &calls);
    expect_parent_complete(&session, &before);
    common::expect_screen(
        &session,
        |s| {
            s.rsplit_once('─').is_some_and(|(_, footer)| {
                footer.contains("1 terminal")
                    && footer.contains("REVIEW_JOB")
                    && !footer.contains("↓ to view")
            })
        },
        common::DEFAULT_TIMEOUT,
        "task counts and urgent advice share one footer",
    );
    session.resize(40, 40).unwrap();
    common::expect_screen_settled(
        &session,
        |s| {
            s.rsplit_once('─').is_some_and(|(_, footer)| {
                footer.contains("REVIEW_JOB") && footer.trim().lines().count() == 1
            })
        },
        common::DEFAULT_TIMEOUT,
        "urgent advice survives narrow footer",
    );
    session.resize(40, 100).unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "wide input restored");
    open(&mut session);
    screen(&session, "TIP_TASK_OUTPUT");
    assert!(
        session.render(|s| s
            .contents()
            .rsplit_once('─')
            .is_some_and(|(_, f)| f.contains("REVIEW_JOB"))),
        "urgent interaction feedback remains visible while browsing"
    );
    fs::write(env.workspace_root().join("release"), "go").unwrap();
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
    screen(&session, "1 new result");
    screen(&session, "1 background task failed");
    open(&mut session);
    screen(&session, "  FAILURE_EVIDENCE");
    screen(&session, "exit 7");
    let view = session.render(common::screen_contents);
    assert!(view.contains("failed"), "failure status:\n{view}");
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b").unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "result read");
    let view = session.render(common::screen_contents);
    assert!(
        !view.contains("new result"),
        "read result must hide footer:\n{view}"
    );
    common::expect_screen_rows_settled(
        &session,
        |view| {
            view.contains("ctx ")
                && !view
                    .lines()
                    .any(|line| line.starts_with("1 background task failed"))
        },
        common::LIVE_TURN_BUDGET,
        "reading the failed result restores turn statistics",
    );
    session.send("/ps\r").unwrap();
    screen(&session, "Enter details");
    session.send("\r").unwrap();
    screen(&session, "  FAILURE_EVIDENCE");
    close(session);
}

#[test]
fn task_command_enter_opens_the_list_without_also_opening_details() {
    let (env, _release) = task_env("background-task-command-key-owner");
    let mut session = start(
        &env,
        &[shell(
            "key-owner",
            "echo TASK_COMMAND_RESULT; exit 7",
            "Task command keyboard ownership",
        )],
    );
    screen(&session, "1 new result");
    // The editor drains its event batch before parent hooks are polled. Ctrl+A/E
    // give the asynchronous /ps command time to reach the UI in that batch.
    // Its submission Enter must never also navigate the newly opened panel.
    for cursor_pairs in [0, 64, 1024, 8192] {
        let input = format!("/ps\r{}", "\x01\x05".repeat(cursor_pairs));
        session.send(&input).unwrap();
        common::expect_screen_settled(
            &session,
            |s| {
                common::input_and_footer_of(s).is_some_and(|area| {
                    area.contains("Enter details") && !area.contains("Esc task list")
                })
            },
            common::DEFAULT_TIMEOUT,
            &format!("/ps Enter opens only the list ({cursor_pairs} cursor pairs)"),
        );
        session.send("\x1b").unwrap();
        expect_input_ready(&session, common::DEFAULT_TIMEOUT, "task list closed");
    }
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn inspecting_running_task_does_not_acknowledge_its_future_result() {
    let (env, _release) = task_env("background-result-after-peek");
    let calls = [shell(
        "background",
        "echo PEEK_RUNNING; while [ ! -f release ]; do sleep 0.1; done; echo PEEK_FINISHED",
        "Inspect before completion",
    )];
    let mut session = spawn_for_calls(&env, &calls);
    expect_input_ready(&session, Duration::from_secs(30), "ready");
    assert!(!session
        .render(common::screen_contents)
        .contains("↓ to view"));
    let before = common::turn_status_marker(&session);
    send_calls(&env, &mut session, &calls);
    // This transition includes a complete model tool call, before any local UI action.
    screen_with_budget(&session, "1 terminal", common::LIVE_TURN_BUDGET);
    expect_parent_complete(&session, &before);
    open(&mut session);
    screen(&session, "  PEEK_RUNNING");
    session.send("\x1b").unwrap();
    screen(&session, "Enter details");
    session.send("\x1b").unwrap();
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "back at input");
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    screen(&session, "1 new result");
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
        common::LIVE_TURN_BUDGET,
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
        screen(&session, "1 agent");
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
    for name in ["one", "two"] {
        for (level, next) in [("parent", "child"), ("child", "grandchild")] {
            fs::write(
                env.workspace_root().join(format!("{name}-{level}.sh")),
                format!("echo $$ > {name}-{level}.pid\nsh {name}-{next}.sh &\nwait\n"),
            )
            .unwrap();
        }
        fs::write(
            env.workspace_root().join(format!("{name}-grandchild.sh")),
            format!("echo $$ > {name}-grandchild.pid\nwhile [ ! -f release-{name} ]; do sleep 0.1; done\necho {}_FINISHED > {name}-done\ncat {name}-done\n", name.to_uppercase()),
        ).unwrap();
    }
    let mut session = start(
        &env,
        &[
            shell("one", "sh one-parent.sh", "Stop this task"),
            shell("two", "sh two-parent.sh", "Keep this task"),
        ],
    );
    screen(&session, "2 terminals");
    let deadline = Instant::now() + Duration::from_secs(10);
    for name in ["one", "two"] {
        while fs::read_to_string(env.workspace_root().join(format!("{name}-grandchild.pid")))
            .map_or(true, |pid| pid.trim().is_empty())
        {
            assert!(
                Instant::now() < deadline,
                "{name} process tree did not start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    open(&mut session);
    let details = screen(&session, "Esc task list");
    let (stopped, survivor) = if details.contains("Stop this task") {
        ("one", "two")
    } else {
        assert!(details.contains("Keep this task"), "{details}");
        ("two", "one")
    };
    session.send("k").unwrap();
    screen(&session, "cancelled");
    screen(&session, "1 terminal");
    #[cfg(unix)]
    for level in ["parent", "child", "grandchild"] {
        let pid = fs::read_to_string(env.workspace_root().join(format!("{stopped}-{level}.pid")))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
            Err(nix::errno::Errno::ESRCH),
            "cancelled was published while {stopped}'s {level} was still present"
        );
    }
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
    assert!(result.contains(&format!("{}_FINISHED", survivor.to_uppercase())));
    assert!(!env
        .workspace_root()
        .join(format!("{stopped}-done"))
        .exists());
    assert!(env
        .workspace_root()
        .join(format!("{survivor}-done"))
        .exists());
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
    common::expect_screen_rows_settled(
        &session,
        |_view| {
            session.render(|screen| {
                // ConPTY can mark the full-width rule above the question as
                // wrapped. Inspect physical rows so contents() cannot join it
                // to the header and hide the visible line boundary.
                let rows = screen
                    .raw()
                    .rows(0, screen.raw().size().1)
                    .collect::<Vec<_>>();
                rows.iter().any(|line| {
                    line.starts_with("1/1") && line.contains("Select the second answer")
                }) && rows.iter().any(|line| line.contains("[2] Second"))
            })
        },
        Duration::from_secs(90),
        "actual question options, rather than the echoed prompt",
    );
    screen(&session, "1 terminal");
    let before = common::turn_status_marker(&session);
    session.send("\x1b[B").unwrap();
    common::expect_screen_rows_settled(
        &session,
        |view| {
            view.lines()
                .any(|line| line.trim_start().starts_with("> [2] Second"))
        },
        Duration::from_secs(90),
        "second answer selected before confirming",
    );
    session.send("\r").unwrap();
    expect_parent_complete(&session, &before);
    if env.is_mock() {
        let results: Vec<Value> = env
            .captured_message_bodies()
            .iter()
            .flat_map(|body| {
                wire_requests::request_tool_blocks(&serde_json::from_str(body).unwrap())
            })
            .filter(|block| block["type"] == "tool_result" && block["tool_use_id"] == "question")
            .map(|block| {
                assert_ne!(block["is_error"], true, "question failed: {block}");
                let content = &block["content"];
                let text = content.as_str().map_or_else(
                    || {
                        content
                            .as_array()
                            .expect("question text blocks")
                            .iter()
                            .map(|part| part["text"].as_str().expect("question text"))
                            .collect::<Vec<_>>()
                            .join("")
                    },
                    str::to_owned,
                );
                serde_json::from_str(&text).expect("question result JSON")
            })
            .collect();
        assert!(
            !results.is_empty()
                && results.iter().all(|result| {
                    result["answers"][0]["value"] == "Second"
                        && result["answers"][0]["label"] == "Second"
                }),
            "actual selected answer must reach provider: question_results={results:?}; screen={}",
            common::screen_tail(&session, 5000)
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
    screen(&session, "1 terminal");
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
    screen(&session, "1 terminal");
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
    screen(&session, "1 terminal");
    send_calls(
        &env,
        &mut session,
        &[
            json!({"id":"foreground","name":"Bash","input":{"command":"touch foreground-ready; while [ ! -f foreground-release ]; do sleep 0.1; done; echo leaked > foreground-leaked.txt","description":"Foreground cleanup fixture"}}),
        ],
    );
    let deadline = Instant::now() + common::LIVE_TURN_BUDGET;
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

/// The tools promise "end your turn — you will be notified". A shell that
/// finishes while the prompt is idle must reach the model by itself: nobody
/// types after the launch turn ends.
#[test]
fn a_background_shell_finishing_at_an_idle_prompt_reports_to_the_model() {
    let (env, _release) = task_env("background-idle-wake");
    let mut session = start(
        &env,
        &[shell(
            "wake",
            "while [ ! -f release ]; do sleep 0.1; done; echo IDLE_WAKE_OUTPUT",
            "Idle wake fixture",
        )],
    );
    let before = common::turn_status_marker(&session);
    let launched = env.is_mock().then(|| env.captured_message_count());
    fs::write(env.workspace_root().join("release"), "go").unwrap();
    common::expect_screen(
        &session,
        |s| s.contains("Background task \"Idle wake fixture\" completed (exit 0)"),
        common::LIVE_TURN_BUDGET,
        "completion shown in scrollback",
    );
    common::expect_turn_complete_after(
        &session,
        &before,
        common::LIVE_TURN_BUDGET,
        "the completion ran a turn with no human input",
    );
    if let Some(launched) = launched {
        let bodies = env.captured_message_bodies();
        let woke = &bodies[launched..];
        assert!(
            woke.iter()
                .any(|body| body.contains("<task-notification>")
                    && body.contains("IDLE_WAKE_OUTPUT")),
            "the model never received the finished shell: {woke:?}"
        );
    }
    expect_input_ready(&session, common::DEFAULT_TIMEOUT, "back at input");
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

/// `/exit` while the provider is still answering ends that turn (`Close`
/// aborts in-flight work). A finished task now starts turns at idle, so a
/// turn under a racing `/exit` is ordinary and must not hold shutdown.
#[test]
fn exiting_while_the_provider_is_answering_cancels_the_turn() {
    let (env, _release) = task_env("exit-cancels-streaming-turn");
    if !env.is_mock() {
        // The stall is a mock control; a live provider has no such switch.
        return;
    }
    // Never released: the mock holds its stream until its own deadline.
    let stall = env.workspace_root().join("stream-never-released");
    let calls = [
        json!({"id":"stalled","name":"Bash","input":{"command":"echo unreachable","description":"Stalled stream fixture"},"stream_wait_for":stall}),
    ];
    let mut session = spawn_for_calls(&env, &calls);
    session.resize(40, 100).unwrap();
    expect_input_ready(&session, Duration::from_secs(30), "ready");
    send_calls(&env, &mut session, &calls);
    let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
    while env.captured_message_count() == 0 {
        assert!(
            Instant::now() < deadline,
            "the turn never reached the provider"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let the stream reach its stall point before leaving.
    std::thread::sleep(Duration::from_millis(500));
    let exit_started = Instant::now();
    session.set_default_timeout(Duration::from_secs(45));
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    let took = exit_started.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "exit waited for the stalled turn instead of cancelling it: {took:?}"
    );
}

/// Run the same real-shell workload against baseline and candidate releases.
#[test]
#[ignore = "manual release A/B measurement"]
fn release_background_task_input_measurement() {
    use common::render_measurement::{measure_keys, report_resources};
    let (env, _release) = task_env("background-input-measurement");
    let mut session = start(
        &env,
        &[shell(
            "background",
            &common::render_measurement::background_output_command(None),
            "Background output workload",
        )],
    );
    // start already waits for the parent turn; measure while the producer lives.
    common::expect_screen_rows(
        &session,
        |view| {
            view.lines()
                .any(|line| line.contains("danger-full-access") && line.contains("1 terminal"))
        },
        Duration::from_secs(30),
        "background producer active before measurement",
    );
    report_resources("background_before_wait");
    std::thread::sleep(Duration::from_secs(5));
    report_resources("background_after_wait");
    measure_keys(&mut session, "background_output");
    report_resources("background_after_input");
    if session.render(|screen| screen.contents().contains("1 terminal")) {
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
    screen(&session, "1 agent");
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
    screen(&session, "1 new result");
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
