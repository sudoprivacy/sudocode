//! Resizing inline chrome must not commit old frames into terminal history.
mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use runtime::{ContentBlock, ConversationMessage, Session, TokenUsage};

fn settled_screen(sess: &PtySession) -> String {
    let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
    let mut previous = String::new();
    let mut stable = 0;
    loop {
        let screen = sess.render(|s| s.raw().contents());
        stable = if screen == previous && screen.contains('❯') {
            stable + 1
        } else {
            0
        };
        if stable >= 8 {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "chrome did not settle:\n{screen}"
        );
        previous = screen;
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn assert_single_chrome(screen: &str) {
    for marker in ["turn 1", "1 todos", "ResizeCompletedTask"] {
        assert_eq!(
            screen.matches(marker).count(),
            1,
            "{marker} must occur once after resize:\n{screen}"
        );
    }
}

#[test]
fn resize_keeps_one_status_and_todo_without_erasing_history_or_input() {
    let env = TestEnv::new("chrome-resize");
    let store = env.workspace_root().join("todos.json");
    std::fs::write(
        &store,
        r#"[{"content":"ResizeCompletedTask","activeForm":"Checking resize","status":"completed"}]"#,
    )
    .unwrap();
    let mut saved = Session::new().with_workspace_root(env.workspace_root());
    saved.push_user_text("Saved resize conversation").unwrap();
    let mut body = (0..70)
        .map(|i| format!("Earlier history line {i}\n"))
        .collect::<String>();
    body.push_str("ResizeHistorySentinel");
    let mut message = ConversationMessage::assistant(vec![ContentBlock::Text { text: body }]);
    message.usage = Some(TokenUsage {
        cache_read_input_tokens: 405_405,
        cache_creation_input_tokens: 4_095,
        output_tokens: 900,
        ..TokenUsage::default()
    });
    message.duration_ms = Some(61_000);
    saved.push_message(message).unwrap();
    let path = env.workspace_root().join("resize-session.jsonl");
    saved.save_to_path(&path).unwrap();
    let mut sess = env.spawn_with_env(
        &[
            "--resume",
            path.to_str().unwrap(),
            "--permission-mode",
            "read-only",
        ],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORFGBG", "15;0"),
        ],
    );
    sess.expect("❯").expect("input ready");
    assert_single_chrome(&settled_screen(&sess));
    sess.send("DraftSurvivesResize").unwrap();
    common::expect_input_line(
        &sess,
        "DraftSurvivesResize",
        common::DEFAULT_TIMEOUT,
        "draft",
    );

    for (rows, cols) in [
        (40, 240),
        (40, 100),
        (40, 240),
        (40, 80),
        (40, 240),
        (40, 60),
        (40, 240),
    ] {
        sess.resize(rows, cols).unwrap();
        let screen = settled_screen(&sess);
        assert_single_chrome(&screen);
        assert!(
            screen.contains("ResizeHistorySentinel"),
            "resize to {rows}x{cols} erased the preceding conversation:\n{screen}"
        );
        common::expect_input_line(
            &sess,
            "DraftSurvivesResize",
            common::DEFAULT_TIMEOUT,
            "resized draft",
        );
    }
    // A drag can deliver another resize before the preceding frame completes.
    for width in [200, 90, 140, 70, 240] {
        sess.resize(40, width).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_single_chrome(&settled_screen(&sess));
    common::expect_input_line(
        &sess,
        "DraftSurvivesResize",
        common::DEFAULT_TIMEOUT,
        "dragged draft",
    );
    // Reducing the viewport height may move history into scrollback. This
    // harness only retains the visible screen, so assert live chrome/input
    // here; the framework's reflow test also checks retained scrollback.
    for (rows, cols) in [(18, 60), (40, 240)] {
        sess.resize(rows, cols).unwrap();
        assert_single_chrome(&settled_screen(&sess));
        common::expect_input_line(
            &sess,
            "DraftSurvivesResize",
            common::DEFAULT_TIMEOUT,
            "short viewport draft",
        );
    }
    sess.send_ctrl('u').unwrap();
    assert!(
        !settled_screen(&sess).contains("DraftSurvivesResize"),
        "Ctrl-U must clear the draft"
    );
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "exit");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            0,
            "resize must not run a turn"
        );
    }
}
