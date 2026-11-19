//! PTY tests for the `TodoWrite` tool — the CC-parity todo checklist.
//!
//! Coverage target: roadmap row "`TodoWrite` + auto-verification streak nudge".
//! `TodoWrite` takes the whole list and replaces the previous one (no per-item
//! id, no partial update), so these tests drive it through the deterministic
//! mock scenarios and assert the terminal + persisted store.
//!
//! ```bash
//! cargo test --test pty_todo_write                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_todo_write  # real API
//! ```

mod common;

use common::TestEnv;

fn receipt_calls(todos: &serde_json::Value) -> serde_json::Value {
    serde_json::json!([{"id":"todo-receipt","name":"TodoWrite","input":{"todos":todos}}])
}

fn submit_receipt_batch(
    env: &TestEnv,
    cli: &mut pty_expect::PtySession,
    todos: &serde_json::Value,
    receipt: &str,
) {
    let prompt = env.tool_batch_prompt(&receipt_calls(todos));
    cli.send(&format!("{prompt}\r")).unwrap();
    cli.expect(receipt)
        .expect("successful write leaves its receipt");
    cli.expect("Concurrency batch done\\.")
        .expect("the tool result returns to the model");
    common::expect_input_line_cleared(cli, common::DEFAULT_TIMEOUT, "todo turn finished");
}

fn assert_live_receipt(cli: &pty_expect::PtySession, has_hook: bool) {
    const HEADER: &str = "3 todos (1 done, 1 in progress, 1 open)";
    const LABELS: [&str; 3] = [
        "Ship native receipt",
        "Verifying the receipt",
        "Keep full tool records",
    ];
    common::expect_screen(
        cli,
        |screen| screen.contains(HEADER) && LABELS.iter().all(|label| screen.contains(label)),
        common::DEFAULT_TIMEOUT,
        "the live panel owns the saved list",
    );
    let screen = cli.render(common::screen_contents);
    let history = screen.split_once(HEADER).unwrap().0;
    assert_eq!(history.matches("✓ Todos updated").count(), 1, "{screen}");
    assert!(
        !history.contains("╭─ TodoWrite"),
        "no duplicate card: {screen}"
    );
    for label in LABELS {
        assert!(
            !history.contains(label),
            "list leaked into history: {screen}"
        );
        assert_eq!(
            screen.matches(label).count(),
            1,
            "list is shown once: {screen}"
        );
    }
    for hidden in ["verificationStreakNudge", "system-reminder", "activeForm"] {
        assert!(
            !screen.contains(hidden),
            "model-only field leaked: {screen}"
        );
    }
    if has_hook {
        assert!(history.contains("Hook feedback:"), "{screen}");
        assert!(history.contains("todo-hook-feedback"), "{screen}");
    }
}

/// Actual writes update the panel and a small receipt; replay uses the same
/// presentation. Complete result bytes, including model-only nudges, survive.
#[test]
fn todo_write_live_receipt_and_restart_keep_one_list_and_full_records() {
    for has_hook in [false, true] {
        let env = TestEnv::new_mock("todo-live-receipt");
        if has_hook {
            let config = env.workspace_root().join(".nexus/sudocode");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(
                config.join("settings.json"),
                r#"{"hooks":{"PostToolUse":["echo todo-hook-feedback"]}}"#,
            )
            .unwrap();
        }
        let store = env.workspace_root().join("todos.json");
        let vars = [
            ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
            ("SUDOCODE_VERIFICATION_STREAK_THRESHOLD", "1"),
        ];
        let mut cli =
            env.spawn_with_env_at_size(&["--permission-mode", "workspace-write"], &vars, 100, 80);
        cli.expect("❯").unwrap();
        submit_receipt_batch(
            &env,
            &mut cli,
            &serde_json::json!([
                {"content":"Ship native receipt","status":"completed","activeForm":"Shipping native receipt"},
                {"content":"Verify the receipt","status":"in_progress","activeForm":"Verifying the receipt"},
                {"content":"Keep full tool records","status":"pending","activeForm":"Keeping full tool records"},
            ]),
            "✓ Todos updated",
        );
        assert_live_receipt(&cli, has_hook);
        cli.send("/exit\r").unwrap();
        assert_eq!(cli.expect_eof().unwrap(), 0);

        let transcript =
            common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
                .expect("the real session was saved");
        let session = runtime::Session::load_from_path(&transcript).unwrap();
        let output = session
            .messages()
            .iter()
            .flat_map(|message| &message.blocks)
            .find_map(|block| match block {
                runtime::ContentBlock::ToolResult {
                    tool_use_id,
                    output,
                    ..
                } if tool_use_id == "todo-receipt" => Some(output),
                _ => None,
            })
            .expect("full tool result in session history");
        for retained in [
            "Ship native receipt",
            "Keep full tool records",
            "verificationStreakNudge",
        ] {
            assert!(
                output.contains(retained),
                "saved result lost {retained}: {output}"
            );
        }
        let encoded = serde_json::to_string(output).unwrap();
        assert!(
            env.captured_message_bodies()
                .iter()
                .any(|body| body.contains(&encoded)),
            "the model must receive the same complete result bytes as the saved session"
        );

        let mut resumed = env.spawn_with_env_at_size(
            &["--resume", "latest", "--permission-mode", "workspace-write"],
            &vars,
            100,
            80,
        );
        resumed.expect("Concurrency batch done\\.").unwrap();
        common::expect_input_line_cleared(
            &resumed,
            common::DEFAULT_TIMEOUT,
            "todo replay finished",
        );
        assert_live_receipt(&resumed, has_hook);
        resumed.send("/exit\r").unwrap();
        assert_eq!(resumed.expect_eof().unwrap(), 0);
    }
}

#[test]
fn todo_write_live_receipts_distinguish_clear_and_completion() {
    for is_clear in [true, false] {
        let env = TestEnv::new_mock("todo-receipt-milestone");
        let mut cli =
            env.spawn_with_env_at_size(&["--permission-mode", "workspace-write"], &[], 100, 80);
        cli.expect("❯").unwrap();
        submit_receipt_batch(
            &env,
            &mut cli,
            &serde_json::json!([{"content":"Finish receipt","status":"pending","activeForm":"Finishing receipt"}]),
            "✓ Todos updated",
        );
        let (todos, receipt) = if is_clear {
            (serde_json::json!([]), "✓ Todos cleared")
        } else {
            (
                serde_json::json!([{"content":"Finish receipt","status":"completed","activeForm":"Finishing receipt"}]),
                "✓ All todos complete",
            )
        };
        submit_receipt_batch(&env, &mut cli, &todos, receipt);
        if is_clear {
            common::expect_screen(
                &cli,
                |screen| !screen.contains(" todos ("),
                common::DEFAULT_TIMEOUT,
                "cleared list hides the panel",
            );
        }
        let screen = cli.render(common::screen_contents);
        assert_eq!(screen.matches(receipt).count(), 1, "{screen}");
        assert!(!screen.contains("╭─ TodoWrite"), "{screen}");
        cli.send("/exit\r").unwrap();
        assert_eq!(cli.expect_eof().unwrap(), 0);
    }
}

#[test]
fn todo_write_live_error_keeps_its_card_and_never_claims_success() {
    let env = TestEnv::new_mock("todo-receipt-error");
    let prompt = env.tool_batch_prompt(&receipt_calls(&serde_json::json!([
        {"content":"Invalid status","status":"unknown-status","activeForm":"Invalid status"}
    ])));
    let mut cli =
        env.spawn_with_env_at_size(&["--permission-mode", "workspace-write"], &[], 100, 80);
    cli.expect("❯").unwrap();
    cli.send(&format!("{prompt}\r")).unwrap();
    cli.expect("Concurrency batch done\\.").unwrap();
    common::expect_input_line_cleared(&cli, common::DEFAULT_TIMEOUT, "failed todo write settled");
    let screen = cli.render(common::screen_contents);
    assert!(screen.contains("╭─ TodoWrite"), "{screen}");
    assert!(screen.contains("unknown variant"), "{screen}");
    assert!(!screen.contains("✓ Todos updated"), "{screen}");
    assert!(!screen.contains(" todos ("), "{screen}");
    cli.send("/exit\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
}

// ──────────────────────────────────────────────────────────────────────
// 1. A single TodoWrite round-trips: tool call → result → final text
// ──────────────────────────────────────────────────────────────────────

/// The model opens a two-item todo list via `TodoWrite`; the tool call and its
/// result surface in the terminal and the turn exits 0.
#[test]
fn todo_write_roundtrips_a_list() {
    let env = TestEnv::new("todo-write");

    // Mock backend, so the checklist renders from a scripted tool call rather
    // than from a live model's choice of tool — the same scoping, for the same
    // reason, as `pty_hook_progress` and `pty_sleep`. What this test asserts is
    // that a `TodoWrite` call and its result reach the terminal; in live mode
    // whether the model reaches for `TodoWrite` at all is its decision, and a
    // run that answers in prose instead fails here on that decision rather than
    // on the rendering under test. The half that is meaningful against a real
    // model — the turn completing and exiting 0 — is covered live by
    // `todo_write_persists_then_empty_wipes_store`.
    if env.is_live() {
        eprintln!(
            "SKIP todo_write_roundtrips_a_list: SCODE_TEST_BACKEND=live — the \
             terminal assertion depends on the model choosing TodoWrite"
        );
        return;
    }

    let prompt = env.prompt(
        "Create a todo list with TodoWrite for: 'Write parser' (in progress) and \
         'Run tests' (pending). Just call the tool.",
        "todo_write_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "TodoWrite",
        &prompt,
    ]);

    sess.expect("(?i)todowrite")
        .expect("model must invoke TodoWrite (agent trigger)");

    if env.is_mock() {
        // The mock's final message echoes the tool output, which includes the
        // saved list — the command should round-trip to completion.
        sess.expect("todo_write roundtrip complete")
            .expect("TodoWrite result should feed the final message");
    }

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0, "todo_write turn should exit 0; got {exit}");
}

/// The card for a `TodoWrite` shows the saved list as the todo panel draws it,
/// not the result JSON. The verification nudge in the result is addressed to
/// the model: it must reach the model and stay off the screen.
#[test]
fn todo_write_card_shows_the_list_and_keeps_the_nudge_for_the_model() {
    let env = TestEnv::new("todo-write-card");
    if env.is_live() {
        return;
    }
    let calls = serde_json::json!([{
        "id": "todo-card",
        "name": "TodoWrite",
        "input": {"todos": [
            {"content": "Ship the card", "status": "completed", "activeForm": "Shipping the card"},
            {"content": "Write the test", "status": "in_progress", "activeForm": "Writing the test"},
            {"content": "Open the PR", "status": "pending", "activeForm": "Opening the PR"},
        ]},
    }]);
    let prompt = env.prompt(
        "Update the todo list with the supplied TodoWrite call.",
        &format!("tool_concurrency TOOL_BATCH:{calls}"),
    );
    // One completion crosses a threshold of 1, so the result carries the nudge.
    let mut cli = env.spawn_with_env(
        &["--permission-mode", "workspace-write", &prompt],
        &[("SUDOCODE_VERIFICATION_STREAK_THRESHOLD", "1")],
    );
    cli.expect("Concurrency batch done.")
        .expect("the TodoWrite call completes");
    let screen = cli.render(common::screen_contents);
    for shown in [
        "TodoWrite",
        "3 todos (1 done, 1 in progress, 1 open)",
        "\u{2713} Ship the card",
        "\u{25a0} Writing the test",
        "\u{25a1} Open the PR",
    ] {
        assert!(
            screen.contains(shown),
            "card is missing {shown:?}:\n{screen}"
        );
    }
    for hidden in ["verificationStreakNudge", "system-reminder", "activeForm"] {
        assert!(!screen.contains(hidden), "card shows {hidden:?}:\n{screen}");
    }
    assert_eq!(cli.expect_eof().unwrap(), 0);
    assert!(
        env.captured_message_bodies()
            .iter()
            .any(|body| body.contains("verificationStreakNudge")),
        "the nudge must still reach the model"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. TodoWrite persists the list to the store (verbatim), and an empty
//    list wipes it — whole-list replace semantics on disk.
// ──────────────────────────────────────────────────────────────────────

/// After a `TodoWrite` the store file contains the exact list; a subsequent
/// empty `TodoWrite` clears the store to `[]`. Both are verified by pointing
/// `SUDOCODE_TODO_STORE` at a temp file and reading it after the turn.
#[test]
fn todo_write_persists_then_empty_wipes_store() {
    let env = TestEnv::new("todo-write-persist");

    let store = env.workspace_root().join("todos.json");

    // First turn: open a list.
    let prompt = env.prompt(
        "Create a todo list with TodoWrite. Just call the tool.",
        "todo_write_roundtrip",
    );
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "TodoWrite",
            &prompt,
        ],
        &[("SUDOCODE_TODO_STORE", store.to_str().unwrap())],
    );
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);

    if env.is_mock() {
        let saved = std::fs::read_to_string(&store).expect("store file should exist after write");
        assert!(
            saved.contains("Write parser") && saved.contains("Run tests"),
            "store must hold the written list verbatim; got: {saved}"
        );

        // Second turn: an empty TodoWrite wipes the store to `[]`.
        let empty_prompt = env.prompt(
            "Clear the todo list with TodoWrite (send an empty list).",
            "todo_write_empty_roundtrip",
        );
        let mut sess = env.spawn_with_env(
            &[
                "--permission-mode",
                "workspace-write",
                "--allowedTools",
                "TodoWrite",
                &empty_prompt,
            ],
            &[("SUDOCODE_TODO_STORE", store.to_str().unwrap())],
        );
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);

        let wiped = std::fs::read_to_string(&store).expect("store file should still exist");
        let parsed: serde_json::Value =
            serde_json::from_str(&wiped).expect("store must be valid JSON");
        assert_eq!(
            parsed.as_array().map(Vec::len),
            Some(0),
            "empty TodoWrite must wipe the store to []; got: {wiped}"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 3. The header counts each item once: open means pending only
// ──────────────────────────────────────────────────────────────────────

/// Seed a list and read the header the panel paints at startup.
fn seeded_header(name: &str, todos: &str) -> String {
    let env = TestEnv::new(name);
    let store = env.workspace_root().join("todos.json");
    std::fs::write(&store, todos).expect("seed todo store");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_TODO_STORE", store.to_str().unwrap())],
    );
    sess.expect("❯").expect("input ready");
    let screen = common::expect_screen(
        &sess,
        |screen| screen.contains(" todos ("),
        env.timeout(),
        "todo header painted",
    );
    sess.send("/exit\r").unwrap();
    let _ = sess.expect_eof();
    screen
        .lines()
        .find(|line| line.contains(" todos ("))
        .expect("header line")
        .trim()
        .to_string()
}

fn todo(content: &str, status: &str) -> String {
    format!(r#"{{"content":"{content}","status":"{status}","activeForm":"Doing {content}"}}"#)
}

/// The reported dense list: in the live panel a long item wraps under its own
/// label, not under the icon, and is cut to the rows its status allows (three
/// in progress, two open), so the panel stays a glance.
#[test]
fn long_todos_wrap_under_their_label_and_are_cut_to_a_glance() {
    let env = TestEnv::new("todo-panel-long-items");
    let store = env.workspace_root().join("todos.json");
    let long = "wire the managed child entry through the bootstrap line on private stdin then the typed ready or error reply then ACP on the same reader and report each outcome to the root agent before the next step starts";
    std::fs::write(
        &store,
        format!("[{},{}]", todo(long, "in_progress"), todo(long, "pending")),
    )
    .expect("seed long todos");
    let mut sess = env.spawn_with_env_at_size(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_TODO_STORE", store.to_str().unwrap())],
        30,
        60,
    );
    common::expect_input_line_cleared(&sess, env.timeout(), "input ready");
    common::expect_screen(
        &sess,
        |screen| screen.contains(" todos ("),
        env.timeout(),
        "todo panel painted",
    );
    let rows: Vec<String> = sess.render(|screen| {
        let raw = screen.raw();
        raw.rows(0, raw.size().1).collect()
    });
    let header = rows
        .iter()
        .position(|row| row.contains(" todos ("))
        .expect("todo header row");
    let mut items: Vec<Vec<&str>> = Vec::new();
    for row in &rows[header + 1..] {
        // The panel sits right of the gutter: markers at column 4, labels
        // and their continuation rows at column 6, as in the TodoWrite card.
        if row.starts_with("    \u{25a0} ") || row.starts_with("    \u{25a1} ") {
            items.push(vec![row]);
        } else if row.starts_with("      ") && !row.starts_with("       ") && !items.is_empty() {
            items.last_mut().unwrap().push(row);
        } else {
            break;
        }
    }
    let rows_per_item: Vec<usize> = items.iter().map(Vec::len).collect();
    assert_eq!(rows_per_item, [3, 2], "rows per item: {rows:#?}");
    assert!(
        items[0][0].starts_with("    \u{25a0} Doing wire"),
        "{items:?}"
    );
    assert!(items[1][0].starts_with("    \u{25a1} wire"), "{items:?}");
    for item in &items {
        assert!(
            item.last().unwrap().trim_end().ends_with('\u{2026}'),
            "a cut item ends with …: {item:?}"
        );
    }
    sess.send("/exit\r").unwrap();
    let _ = sess.expect_eof();
}

#[test]
fn todo_header_counts_in_progress_once_and_open_as_pending() {
    let mixed = format!(
        "[{},{},{}]",
        todo("parse", "completed"),
        todo("render", "in_progress"),
        todo("ship", "pending")
    );
    assert_eq!(
        seeded_header("todo-header-mixed", &mixed),
        "3 todos (1 done, 1 in progress, 1 open)"
    );

    // The reported case: everything but the in-progress item is done.
    let mut items: Vec<String> = (1..=8)
        .map(|n| todo(&format!("step {n}"), "completed"))
        .collect();
    items.push(todo("ship", "in_progress"));
    assert_eq!(
        seeded_header(
            "todo-header-last-in-progress",
            &format!("[{}]", items.join(","))
        ),
        "9 todos (8 done, 1 in progress, 0 open)"
    );
}
