//! No-API acceptance of the shared chrome row budget using the actual REPL.
//! Resize/history ownership is additionally checked by `pty_chrome_resize` and
//! the framework's ConPTY/xterm driver; these are not substitutes for it.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session};

struct DraftWriterGate(std::path::PathBuf);

impl Drop for DraftWriterGate {
    fn drop(&mut self) {
        // A failed assertion must also let the waiting shell finish.
        let _ = std::fs::write(&self.0, "");
    }
}

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
    // Drop before the PTY, including while an assertion is unwinding.
    let writer_gate = DraftWriterGate(env.workspace_root().join("draft-go"));
    common::expect_screen_settled(
        &sess,
        |s| s.contains("BudgetTask2") && s.contains('❯'),
        common::DEFAULT_TIMEOUT,
        "expanded todos",
    );

    // Below the literal-paste threshold, but longer than the small viewport.
    let payload = format!("DraftHead{}DraftTail", "_keep_".repeat(90));
    // Hold the shell after redirection creates the file. Existence alone
    // cannot establish that the restored command has finished writing it.
    let command = format!(
        "! : > draft.txt; while [ ! -f draft-go ]; do sleep 0.01; done; \
         echo {payload} > draft.txt; printf 'DraftDone%s' '-marker'"
    );
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
        |s| {
            s.contains("BudgetTask2")
                && common::input_and_footer_of(s)
                    .is_some_and(|area| area.contains("DraftHead") && area.contains("draft.txt"))
        },
        common::DEFAULT_TIMEOUT,
        "expanded draft",
    );
    assert!(
        !common::input_and_footer_of(&restored)
            .expect("current InputSlot and FooterSlot")
            .contains("compact ·"),
        "summary must expand:\n{restored}"
    );
    sess.send("\r").unwrap();
    let result = env.workspace_root().join("draft.txt");
    let deadline = std::time::Instant::now() + common::DEFAULT_TIMEOUT;
    while !result.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert_eq!(
        std::fs::read_to_string(&result).unwrap(),
        "",
        "the file must exist before the shell is allowed to write"
    );
    drop(writer_gate);
    // This marker is assembled by the shell, so an echoed command cannot
    // satisfy the wait. Check the draft only after its command has completed.
    common::expect_screen(
        &sess,
        |s| s.contains("DraftDone-marker") && common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "restored draft command completed",
    );
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
    // ConPTY input writes and native resize calls have no shared ordering.
    // Inspect after Enter and wait for the frozen description before growing:
    // this proves Enter was handled while the controls were still hidden.
    sess.expect("Enlarge terminal to review").unwrap();
    sess.send("\r\x0f").unwrap();
    sess.expect("Current:").unwrap();
    sess.resize(40, 80).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| {
            common::input_and_footer_of(s).is_some_and(|area| {
                area.contains("Select model") && !area.contains("Enlarge terminal to review")
            })
        },
        common::DEFAULT_TIMEOUT,
        "unconfirmed picker restored",
    );
    sess.send("\x1b").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| {
            common::input_and_footer_of(s)
                .is_some_and(|area| area.contains('❯') && !area.contains("Select model"))
        },
        common::DEFAULT_TIMEOUT,
        "cancel picker",
    );
    exit(&mut sess);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn resize_and_confirm_burst_keeps_the_unseen_picker_open() {
    let env = TestEnv::new("chrome-budget-resize-confirm-burst");
    let mut sess = env.spawn_with_env(&[], &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")]);
    common::expect_screen(&sess, |s| s.contains('❯'), common::DEFAULT_TIMEOUT, "input");
    sess.send("/model\r").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Select model"),
        common::DEFAULT_TIMEOUT,
        "picker before resize",
    );
    for _ in 0..3 {
        sess.resize(6, 80).unwrap();
        // No paint, sleep or polling between resize and confirmation: admission
        // must use the new geometry even when events arrive in the same batch.
        sess.send("\r").unwrap();
        common::expect_screen_settled(
            &sess,
            |s| s.contains("Enlarge terminal to review"),
            common::DEFAULT_TIMEOUT,
            "warning after resize/confirm burst",
        );
        sess.resize(40, 80).unwrap();
        common::expect_screen_settled(
            &sess,
            |s| {
                common::input_and_footer_of(s).is_some_and(|area| {
                    area.contains("Select model") && !area.contains("Enlarge terminal to review")
                })
            },
            common::DEFAULT_TIMEOUT,
            "unconfirmed picker after immediate resize/Enter",
        );
    }
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
fn resize_and_typing_burst_cannot_edit_an_unreviewable_draft() {
    assert_resize_edit_burst_rejected("MustNotAppear", "chrome-budget-resize-typing-burst");
}

#[test]
fn resize_and_paste_burst_cannot_edit_an_unreviewable_draft() {
    assert_resize_edit_burst_rejected(
        "\x1b[200~MustNotAppear\x1b[201~",
        "chrome-budget-resize-paste-burst",
    );
}

fn assert_resize_edit_burst_rejected(burst: &str, name: &str) {
    let env = TestEnv::new(name);
    let mut sess = env.spawn_with_env(&[], &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")]);
    common::expect_screen(&sess, |s| s.contains('❯'), common::DEFAULT_TIMEOUT, "input");
    sess.send("DraftHeadTail").unwrap();
    common::expect_input_line(&sess, "DraftHeadTail", common::DEFAULT_TIMEOUT, "draft");
    sess.send("\x1b[H\x1b[C\x1b[C\x1b[C\x1b[C\x1b[CX").unwrap();
    common::expect_input_line(
        &sess,
        "DraftXHeadTail",
        common::DEFAULT_TIMEOUT,
        "cursor probe",
    );
    sess.send("\x7f").unwrap();
    common::expect_input_line(
        &sess,
        "DraftHeadTail",
        common::DEFAULT_TIMEOUT,
        "remove probe",
    );
    sess.resize(8, 12).unwrap();
    // Deliberately do not wait for the warning before this input burst.
    sess.send(burst).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Enlarge") && s.contains("terminal"),
        common::DEFAULT_TIMEOUT,
        "warning after resize/typing burst",
    );
    sess.resize(24, 80).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| common::input_line_of(s) == "DraftHeadTail",
        common::DEFAULT_TIMEOUT,
        "draft unchanged after immediate resize/typing",
    );
    sess.send("Kept").unwrap();
    common::expect_input_line(
        &sess,
        "DraftKeptHeadTail",
        common::DEFAULT_TIMEOUT,
        "cursor preserved after immediate resize/typing",
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
