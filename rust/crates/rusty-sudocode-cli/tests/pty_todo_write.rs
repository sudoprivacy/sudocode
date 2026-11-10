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
