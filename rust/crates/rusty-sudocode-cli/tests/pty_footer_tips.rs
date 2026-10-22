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
    assert!(session.render(|s| s.contents()).contains("unknown variant"));
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
