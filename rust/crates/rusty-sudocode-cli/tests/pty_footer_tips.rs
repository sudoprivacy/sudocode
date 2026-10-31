//! Real terminal + local synthetic tool calls. Never contacts a live provider.
mod common;
use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};

fn footer(session: &PtySession) -> String {
    session.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        let separator = rows
            .iter()
            .rposition(|row| row.contains('─') && row.trim().chars().all(|c| c == '─'))
            .expect("input separator");
        rows[separator + 1..].join("\n").trim_end().to_owned()
    })
}
fn wait_footer(session: &PtySession, text: &str) {
    common::expect_screen(
        session,
        |screen| {
            screen
                .rsplit_once('─')
                .is_some_and(|(_, tail)| tail.contains(text))
        },
        common::DEFAULT_TIMEOUT,
        text,
    );
}
fn tip(priority: &str, message: &str, id: &str) -> Value {
    json!({"id":id,"name":"set_tip","input":{"priority":priority,"message":message}})
}
fn turn(env: &TestEnv, session: &mut PtySession, calls: &[Value]) {
    let before = common::turn_status_marker(session);
    let prompt = if calls.is_empty() {
        "A plain follow-up.".to_string()
    } else {
        format!(
            "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
            json!(calls)
        )
    };
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        session,
        |s| !common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "tool request typed",
    );
    session.send("\r").unwrap();
    common::expect_turn_complete_after(session, &before, common::DEFAULT_TIMEOUT, "tip turn");
    common::expect_input_line_cleared(session, common::DEFAULT_TIMEOUT, "tip input ready");
    assert!(env.is_mock());
}
fn spawn(env: &TestEnv, background: &str, no_color: bool) -> PtySession {
    let mut session = env.spawn_with_env(
        &[
            "--permission-mode",
            "read-only",
            "--allowedTools",
            "set_tip",
        ],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", background),
            ("NO_COLOR", if no_color { "1" } else { "" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    session.resize(30, 180).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "initial input");
    wait_footer(&session, "Type /help for commands");
    session
}
fn exit(session: &mut PtySession) {
    session.send("/exit\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn footer_tip_tool_priorities_clear_and_schema_are_visible_end_to_end() {
    let env = TestEnv::new_mock("footer-tip-priorities");
    let mut session = spawn(&env, "15;0", false);
    turn(
        &env,
        &mut session,
        &[
            tip("urgent", "URGENT_ACTION", "u"),
            tip("important", "IMPORTANT_ADVICE", "i"),
            tip("normal", "NORMAL_GUIDANCE", "n"),
        ],
    );
    wait_footer(&session, "URGENT_ACTION");
    assert!(!footer(&session).contains("IMPORTANT_ADVICE"));
    assert!(!footer(&session).contains("NORMAL_GUIDANCE"));
    let bodies = env.captured_message_bodies();
    let body: Value = serde_json::from_str(&bodies[0]).unwrap();
    let spec = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "set_tip")
        .unwrap();
    assert_eq!(
        spec["input_schema"]["properties"]["priority"]["enum"],
        json!(["urgent", "important", "normal"])
    );
    assert!(spec["input_schema"]["required"]
        .as_array()
        .unwrap()
        .contains(&json!("priority")));
    assert_ne!(spec["defer_loading"], true);
    turn(
        &env,
        &mut session,
        &[
            tip("important", "KEEP_IMPORTANT", "i2"),
            tip("normal", "LOWER_PRIORITY", "n2"),
            tip("urgent", "TEMPORARY_URGENT", "u2"),
            tip("urgent", "", "clear"),
        ],
    );
    wait_footer(&session, "KEEP_IMPORTANT");
    assert!(!footer(&session).contains("TEMPORARY_URGENT"));
    turn(&env, &mut session, &[]);
    wait_footer(&session, "/help");
    assert!(!footer(&session).contains("KEEP_IMPORTANT"));
    turn(
        &env,
        &mut session,
        &[tip("highest", "INVALID_PRIORITY", "bad")],
    );
    assert!(!footer(&session).contains("INVALID_PRIORITY"));
    assert!(session
        .render(common::screen_contents)
        .contains("unknown variant"));
    turn(
        &env,
        &mut session,
        &[
            tip("important", "INVALID_LINE\nSECOND_LINE", "control"),
            tip("important", &"x".repeat(161), "oversize"),
        ],
    );
    let evidence = env.captured_message_bodies().join("\n");
    assert!(evidence.contains("single plain-text line"));
    assert!(evidence.contains("160 characters"));
    assert!(!footer(&session).contains("INVALID_LINE"));
    exit(&mut session);
}

#[test]
fn footer_tips_preserve_theme_and_unicode_and_draft_across_resize() {
    for (background, no_color, expected) in [
        ("15;0", false, "Idx(214)"),
        ("0;15", false, "Idx(94)"),
        ("15;0", true, "Default"),
    ] {
        let env = TestEnv::new_mock("footer-tip-colors");
        let mut session = spawn(&env, background, no_color);
        turn(
            &env,
            &mut session,
            &[tip(
                "important",
                "TIP_COLOUR 中文建议 keeps its theme and stays on one row",
                "theme",
            )],
        );
        wait_footer(&session, "TIP_COLOUR");
        eprintln!(
            "theme={background}, no_color={no_color}, footer={}",
            footer(&session)
        );
        session.render(|screen| {
            let raw = screen.raw();
            let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
            let row = rows.iter().rposition(|r| r.contains("TIP_COLOUR")).unwrap();
            let col = rows[row].chars().take_while(|c| *c != 'T').count();
            let cell = raw
                .cell(row.try_into().unwrap(), col.try_into().unwrap())
                .unwrap();
            assert_eq!(format!("{:?}", cell.fgcolor()), expected);
            assert!(!cell.dim(), "readable emphasis must not inherit dim");
        });
        session.send("KEEP_DRAFT").unwrap();
        common::expect_input_line(&session, "KEEP_DRAFT", common::DEFAULT_TIMEOUT, "draft");
        for (height, width) in [(30, 80), (30, 40), (30, 120), (8, 40), (8, 120), (30, 120)] {
            session.resize(height, width).unwrap();
            common::expect_input_line(
                &session,
                "KEEP_DRAFT",
                common::DEFAULT_TIMEOUT,
                "draft survives resize",
            );
            wait_footer(&session, "TIP_COLOUR");
            common::expect_screen_settled(
                &session,
                |_| {
                    let rendered = footer(&session);
                    rendered.contains("TIP_COLOUR") && rendered.lines().count() == 1
                },
                common::DEFAULT_TIMEOUT,
                "one stable footer row after resize",
            );
            assert_eq!(footer(&session).lines().count(), 1, "one footer row");
            assert!(!footer(&session).contains('\u{fffd}'));
        }
        session.send("\x15").unwrap();
        common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "clear draft");
        exit(&mut session);
    }
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Rows of the per-turn status line, which carries `turn N` and a token count.
fn status_rows(session: &PtySession) -> Vec<String> {
    session.render(|screen| {
        let raw = screen.raw();
        raw.rows(0, raw.size().1)
            .filter(|row| row.contains("turn ") && row.contains("tokens"))
            .collect()
    })
}

#[test]
fn footer_names_the_workspace_branch_and_yields_it_to_any_tip() {
    let env = TestEnv::new_mock("footer-branch");
    let root = env.workspace_root();
    git(root, &["init", "-q", "-b", "feat/footer-branch"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    let repo = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut session = spawn(&env, "15;0", false);
    // Idle, not typing: the position, `repo ⎇ branch`, leads the filler.
    wait_footer(
        &session,
        &format!("{repo} ⎇ feat/footer-branch · Type /help for commands"),
    );

    // A real tip takes the slot; the branch is state, not advice, and yields.
    turn(
        &env,
        &mut session,
        &[tip("important", "BRANCH_YIELDS", "i")],
    );
    wait_footer(&session, "BRANCH_YIELDS");
    assert!(
        !footer(&session).contains("feat/footer-branch"),
        "a tip must win the slot: {}",
        footer(&session)
    );
    // The per-turn line no longer repeats the branch.
    let rows = status_rows(&session);
    assert!(!rows.is_empty(), "the turn status line is on screen");
    assert!(
        rows.iter().all(|row| !row.contains("feat/footer-branch")),
        "branch left the per-turn line: {rows:?}"
    );

    // Detached on a branch tip, the footer names that tip and the commit,
    // refreshed when the turn ends.
    git(root, &["checkout", "-q", "--detach"]);
    let commit = git(root, &["rev-parse", "--short", "HEAD"]);
    turn(&env, &mut session, &[]);
    wait_footer(&session, &format!("⎇ feat/footer-branch@{commit} · /help"));
    assert!(!footer(&session).contains("detached HEAD"));
    exit(&mut session);
}

/// The position and the keys for the current state; typing never changes it,
/// so keystrokes never redraw the footer. It changes when a turn starts and
/// ends, as the rest of the screen does.
#[test]
fn footer_keeps_position_and_state_keys_while_typing_and_switches_with_the_turn() {
    let env = TestEnv::new_mock("footer-turn-state");
    let root = env.workspace_root();
    git(root, &["init", "-q", "-b", "feat/turn-state"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    std::fs::write(
        root.join("wait.sh"),
        "printf started > tool-started; while test ! -f release-tool; do sleep 0.1; done",
    )
    .unwrap();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    session.resize(30, 180).unwrap();
    common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "initial input");
    let idle = "⎇ feat/turn-state · Type /help for commands";
    wait_footer(&session, idle);

    // Typing a message or a command leaves the footer as it was.
    for draft in ["draft", "/he"] {
        session.send(draft).unwrap();
        common::expect_input_line(&session, draft, common::DEFAULT_TIMEOUT, "draft shown");
        assert!(
            footer(&session).contains(idle),
            "typing redrew the footer: {}",
            footer(&session)
        );
        session.send("\x15").unwrap();
        common::expect_input_line_cleared(&session, common::DEFAULT_TIMEOUT, "cleared");
    }

    // A turn held open by a real shell: the position stays, with the steering keys.
    let call =
        json!({"id":"hold","name":"Bash","input":{"command":"sh wait.sh","timeout":120_000}});
    let prompt = format!(
        "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",
        json!([call])
    );
    session
        .send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .unwrap();
    common::expect_screen(
        &session,
        |s| !common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "prompt typed",
    );
    session.send("\r").unwrap();
    common::expect_screen(
        &session,
        |_| root.join("tool-started").exists(),
        common::DEFAULT_TIMEOUT,
        "turn running",
    );
    let running = "⎇ feat/turn-state · Enter steer · Tab queue · Esc/Ctrl+C cancel";
    wait_footer(&session, running);
    session.send("steer").unwrap();
    common::expect_input_line(
        &session,
        "steer",
        common::DEFAULT_TIMEOUT,
        "typed during turn",
    );
    assert!(
        footer(&session).contains(running),
        "typing redrew the footer: {}",
        footer(&session)
    );
    session.send("\x15").unwrap();

    std::fs::write(root.join("release-tool"), "release").unwrap();
    wait_footer(&session, "⎇ feat/turn-state · /help");
    exit(&mut session);
}
