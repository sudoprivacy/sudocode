//! PTY tests for session resume (`--resume`) behavior.
//!
//! Verifies:
//! 1. `--resume list` lists available sessions; bare `--resume` resumes latest
//! 2. `--resume <id>` enters REPL with previous messages rendered
//! 3. No duplicate rendering between resume report and message replay
//! 4. Messages are rendered using the same pipeline as live output

mod common;

use std::fs;
use std::time::Duration;

/// `--resume list` should list sessions and exit.
#[test]
fn resume_list_shows_sessions_and_exits() {
    let env = common::TestEnv::new("resume-list");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // Create a session by entering then exiting the REPL.
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));
    sess.expect("❯").expect("REPL prompt");
    sess.send("/exit\r").expect("send exit");
    sess.expect_eof().expect("clean exit");

    // `--resume list` opens the session browser and exits.
    let mut sess2 = env.spawn(&["--resume", "list"]);
    sess2.set_default_timeout(Duration::from_secs(5));

    sess2.expect("Available sessions").unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("should list sessions: {e}\nPTY screen:\n{screen}");
    });

    sess2.expect("scode --resume").unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("should show usage tip: {e}\nPTY screen:\n{screen}");
    });

    let exit2 = sess2.expect_eof().unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("should exit after listing: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit2, 0);
}

/// Bare `--resume` (no id) resumes the latest session directly — it enters the
/// REPL rather than printing the session list.
#[test]
fn resume_no_args_resumes_latest() {
    let env = common::TestEnv::new("resume-bare");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // Create a session so there is a latest to resume.
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));
    sess.expect("❯").expect("REPL prompt");
    sess.send("/exit\r").expect("send exit");
    sess.expect_eof().expect("clean exit");

    // Bare `--resume` enters the REPL on the latest session (must NOT list).
    let mut sess2 = env.spawn_with_env(
        &["--resume", "--permission-mode", "read-only"],
        &[("EDITOR", "true")],
    );
    sess2.set_default_timeout(Duration::from_secs(10));
    sess2.expect("❯").unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!(
            "bare --resume should resume latest (enter REPL), not list: {e}\nPTY screen:\n{screen}"
        );
    });
    sess2.send("/exit\r").expect("send exit");
    let exit2 = sess2.expect_eof().expect("clean exit");
    assert_eq!(exit2, 0);
}

/// `--resume latest` should show banner, then previous messages, then
/// REPL prompt — with no duplicate rendering.
#[test]
fn resume_latest_renders_messages_after_banner() {
    let env = common::TestEnv::new("resume-render");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // First: run a session with a turn so it has messages.
    let prompt = env.prompt("say hello world", "single_turn_text");
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(15));
    sess.expect("❯").expect("REPL prompt");
    let marker = common::turn_status_marker(&sess);
    sess.send(&format!("{prompt}\r")).expect("send prompt");
    common::expect_turn_complete_after(&sess, &marker, Duration::from_secs(30), "initial turn");
    sess.send("/exit\r").expect("send exit");
    sess.expect_eof().expect("clean exit");

    // Resume latest session.
    let mut sess2 = env.spawn_with_env(
        &["--resume", "latest", "--permission-mode", "read-only"],
        &[("EDITOR", "true")],
    );
    sess2.set_default_timeout(Duration::from_secs(15));

    // Should see the banner.
    sess2.expect("Code").unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("should see banner: {e}\nPTY screen:\n{screen}");
    });

    // Should see the REPL prompt (messages rendered between banner and prompt).
    // The restored history now echoes user messages with `❯`, so a byte-stream
    // `expect("❯")` would match a history line; wait for the live input line
    // (lowest `❯` row) to be empty instead.
    common::expect_input_line_cleared(&sess2, Duration::from_secs(15), "resume ready");

    // Verify no duplicate: "Session resumed" should appear at most once.
    let screen = sess2.render(|s| s.contents());
    let resume_count = screen.matches("Session resumed").count();
    assert!(
        resume_count <= 1,
        "should not duplicate 'Session resumed'. Found {resume_count} times.\n\
         PTY screen:\n{screen}"
    );

    sess2.send("/exit").expect("type exit");
    common::expect_input_line(&sess2, "/exit", Duration::from_secs(15), "exit typed");
    sess2.send("\r").expect("submit exit");
    let exit = sess2.expect_eof().unwrap_or_else(|e| {
        let screen2 = sess2.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen2}");
    });
    assert_eq!(exit, 0);
}

/// Regression: the iocraft REPL (queue mode) must replay the restored
/// conversation to scrollback on resume, not just print the banner. Before the
/// fix, only the rustyline path (queue mode off) rendered history, so a resumed
/// session in the default async REPL looked empty.
#[test]
fn resume_renders_history_in_iocraft_queue_mode() {
    let env = common::TestEnv::new("resume-iocraft");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // Run a turn in queue (iocraft) mode so the session has a distinctive
    // assistant reply. The mock has a canned reply; live models are instructed
    // to emit the same stable sentinel that resume must replay.
    let prompt = env.prompt(
        "Reply with exactly: SCODE_RESUME_SENTINEL",
        "single_turn_text",
    );
    let expected_reply = if env.is_mock() {
        "The answer is 4"
    } else {
        "SCODE_RESUME_SENTINEL"
    };
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(15));
    sess.expect("❯").expect("REPL prompt");
    sess.send(&format!("{prompt}\r")).expect("send prompt");
    sess.expect(expected_reply).expect("assistant reply");
    std::thread::sleep(Duration::from_millis(400));
    sess.send("/exit\r").expect("send exit");
    sess.expect_eof().expect("clean exit");

    // Resume in queue mode: the restored assistant reply must appear in
    // scrollback (rendered history), not just the banner.
    let mut sess2 = env.spawn_with_env(
        &["--resume", "latest", "--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess2.set_default_timeout(Duration::from_secs(15));
    sess2.expect(expected_reply).unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!(
            "resumed iocraft REPL must replay history to scrollback: {e}\nPTY screen:\n{screen}"
        );
    });

    // The restored user message must be echoed with the same `❯` prompt glyph
    // the live input uses — NOT the old `›` style — so resume looks identical
    // to the pre-exit scrollback.
    let screen = sess2.render(|s| s.contents());
    assert!(
        !screen.contains("› say hello world"),
        "resumed user message used the `›` echo style instead of `❯`:\n{screen}"
    );

    std::thread::sleep(Duration::from_millis(400));
    sess2.send("/exit\r").expect("send exit");
    let exit = sess2.expect_eof().unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);
}
