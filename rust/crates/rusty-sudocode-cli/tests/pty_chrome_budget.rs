//! No-API acceptance of the shared chrome row budget using the actual REPL.
//! Resize/history ownership is additionally checked by `pty_chrome_resize` and
//! the framework's ConPTY/xterm driver; these are not substitutes for it.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session};

fn exit(sess: &mut pty_expect::PtySession) {
    // Both workflows finish on an empty input. Do not send a redundant
    // parent-owned Ctrl-U in the same key burst as TextInput-owned typing.
    common::expect_screen_settled(
        sess,
        |screen| screen.contains('❯') && common::input_line_of(screen).is_empty(),
        common::DEFAULT_TIMEOUT,
        "empty input before exit",
    );
    sess.send("/exit").unwrap();
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

#[test]
fn short_window_folds_todos_and_restores_the_entire_draft() {
    let env = TestEnv::new("chrome-budget-draft");
    let store = env.workspace_root().join("todos.json");
    let todos: Vec<_> = (0..3)
        .map(|i| {
            serde_json::json!({
                "content": format!("BudgetTask{i}"),
                "activeForm": format!("WorkingBudgetTask{i}"),
                "status": "pending",
            })
        })
        .collect();
    std::fs::write(&store, serde_json::to_vec(&todos).unwrap()).unwrap();
    let mut session = Session::new().with_workspace_root(env.workspace_root());
    session.push_user_text("Saved layout session").unwrap();
    session
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "BudgetHistorySentinel".into(),
        }]))
        .unwrap();
    let saved = env.workspace_root().join("session.jsonl");
    session.save_to_path(&saved).unwrap();
    let mut sess = env.spawn_with_env(
        &["--resume", saved.to_str().unwrap()],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
        ],
    );
    common::expect_screen_settled(
        &sess,
        |s| s.contains("BudgetTask2") && s.contains('❯'),
        common::DEFAULT_TIMEOUT,
        "expanded todos",
    );

    // Below the literal-paste threshold, but longer than the small viewport.
    let payload = format!("DraftHead{}DraftTail", "_keep_".repeat(90));
    let command = format!("! echo {payload} > draft.txt");
    sess.send(&format!("\x1b[200~{command}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("draft.txt"),
        common::DEFAULT_TIMEOUT,
        "long draft cursor",
    );
    sess.resize(8, 80).unwrap();
    let small = common::expect_screen_settled(
        &sess,
        |s| s.contains("compact") && s.contains("draft.txt"),
        common::DEFAULT_TIMEOUT,
        "compact draft",
    );
    assert!(
        small.contains("3 todos"),
        "counts must remain visible:\n{small}"
    );
    assert!(
        !small.contains("BudgetTask"),
        "todo details must fold:\n{small}"
    );
    assert_eq!(small.matches('❯').count(), 1, "one InputSlot:\n{small}");

    sess.resize(40, 80).unwrap();
    let restored = common::expect_screen_settled(
        &sess,
        |s| s.contains("BudgetTask2") && s.contains("DraftHead") && s.contains("draft.txt"),
        common::DEFAULT_TIMEOUT,
        "expanded draft",
    );
    assert!(
        !restored.contains("compact ·"),
        "summary must expand:\n{restored}"
    );
    sess.send("\r").unwrap();
    let result = env.workspace_root().join("draft.txt");
    let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
    while !result.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(
        std::fs::read_to_string(result).unwrap().trim(),
        payload,
        "resizing must retain all draft bytes, not just its visible tail"
    );
    exit(&mut sess);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn undersized_picker_cannot_confirm_an_unseen_selection() {
    let env = TestEnv::new("chrome-budget-picker");
    let mut sess = env.spawn_with_env(&[], &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")]);
    common::expect_screen(&sess, |s| s.contains('❯'), common::DEFAULT_TIMEOUT, "input");
    sess.send("/model\r").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Select model"),
        common::DEFAULT_TIMEOUT,
        "picker",
    );
    sess.resize(6, 80).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Enlarge terminal to review"),
        common::DEFAULT_TIMEOUT,
        "size warning",
    );
    sess.send("\r").unwrap();
    sess.resize(40, 80).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Select model") && !s.contains("Enlarge terminal to review"),
        common::DEFAULT_TIMEOUT,
        "unconfirmed picker restored",
    );
    sess.send("\x1b").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains('❯') && !s.contains("Select model"),
        common::DEFAULT_TIMEOUT,
        "cancel picker",
    );
    exit(&mut sess);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn too_narrow_input_preserves_draft_and_middle_cursor() {
    let env = TestEnv::new("chrome-budget-narrow");
    let mut sess = env.spawn_with_env(&[], &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")]);
    common::expect_screen(&sess, |s| s.contains('❯'), common::DEFAULT_TIMEOUT, "input");
    sess.send("DraftHeadTail").unwrap();
    common::expect_input_line(&sess, "DraftHeadTail", common::DEFAULT_TIMEOUT, "draft");
    // Navigation and editing in one burst must share the same cursor. No
    // artificial pause/render is allowed between moving and inserting.
    sess.send("\x1b[H\x1b[C\x1b[C\x1b[C\x1b[C\x1b[CX").unwrap();
    common::expect_input_line(
        &sess,
        "DraftXHeadTail",
        common::DEFAULT_TIMEOUT,
        "middle cursor probe",
    );
    sess.send("\x7f").unwrap();
    common::expect_input_line(
        &sess,
        "DraftHeadTail",
        common::DEFAULT_TIMEOUT,
        "remove probe",
    );
    sess.resize(8, 12).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Enlarge") && s.contains("terminal"),
        common::DEFAULT_TIMEOUT,
        "narrow warning",
    );
    sess.send("MustNotAppear").unwrap();
    // A visible size warning has focus, so hidden draft edits are rejected.
    // Give the key burst time to reach the app before restoring the viewport.
    std::thread::sleep(std::time::Duration::from_millis(150));
    sess.resize(24, 80).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| common::input_line_of(s) == "DraftHeadTail",
        common::DEFAULT_TIMEOUT,
        "unchanged draft",
    );
    sess.send("Kept").unwrap();
    common::expect_input_line(
        &sess,
        "DraftKeptHeadTail",
        common::DEFAULT_TIMEOUT,
        "preserved cursor",
    );
    sess.send_ctrl('u').unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains('❯') && common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "cleared draft",
    );
    exit(&mut sess);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}
