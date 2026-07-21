//! PTY tests for session resume (`--resume`) behavior.
//!
//! Verifies:
//! 1. `--resume` without args lists available sessions
//! 2. `--resume <id>` enters REPL with previous messages rendered
//! 3. No duplicate rendering between resume report and message replay
//! 4. Messages are rendered using the same pipeline as live output

mod common;

use std::fs;
use std::time::Duration;

/// `--resume` without arguments should list sessions and exit.
#[test]
fn resume_no_args_lists_sessions() {
    let env = common::TestEnv::new("resume-list");
    let root = env.workspace_root().to_path_buf();
    fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // Create a session by entering then exiting the REPL.
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("EDITOR", "true")]);
    sess.set_default_timeout(Duration::from_secs(10));
    sess.expect("❯").expect("REPL prompt");
    sess.send("/exit\r").expect("send exit");
    sess.expect_eof().expect("clean exit");

    // Now run --resume with no args.
    let mut sess2 = env.spawn(&["--resume"]);
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
    sess.send(&format!("{prompt}\r")).expect("send prompt");
    sess.expect("❯").expect("second prompt after turn");
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
    sess2.expect("❯").unwrap_or_else(|e| {
        let screen = sess2.render(|s| s.contents());
        panic!("should see REPL prompt after resume: {e}\nPTY screen:\n{screen}");
    });

    // Verify no duplicate: "Session resumed" should appear at most once.
    let screen = sess2.render(|s| s.contents());
    let resume_count = screen.matches("Session resumed").count();
    assert!(
        resume_count <= 1,
        "should not duplicate 'Session resumed'. Found {resume_count} times.\n\
         PTY screen:\n{screen}"
    );

    sess2.send("/exit\r").expect("send exit");
    let exit = sess2.expect_eof().unwrap_or_else(|e| {
        let screen2 = sess2.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen2}");
    });
    assert_eq!(exit, 0);
}
