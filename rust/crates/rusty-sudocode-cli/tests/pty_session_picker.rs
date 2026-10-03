//! Switch and delete saved sessions through the real iocraft input owner.
mod common;

use std::path::{Path, PathBuf};

fn branch_session(root: &Path, branch: &str) -> (String, PathBuf) {
    let mut pending = vec![root.join(".scode/sessions")];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name == "transcript.jsonl")
            {
                let session = runtime::Session::load_from_path(&path).unwrap();
                if session
                    .fork
                    .as_ref()
                    .and_then(|fork| fork.branch_name.as_deref())
                    == Some(branch)
                {
                    return (session.session_id, path);
                }
            }
        }
    }
    panic!("missing saved branch {branch}");
}

fn command(child: &mut pty_expect::PtySession, text: &str, result: &str) {
    child.send(&format!("{text}\r")).unwrap();
    child
        .expect(result)
        .unwrap_or_else(|error| panic!("{text}: {error}; {}", common::screen_tail(child, 6000)));
    common::expect_input_line_cleared(child, common::DEFAULT_TIMEOUT, "session prompt");
}

fn assert_active_session(child: &mut pty_expect::PtySession, session: &str) {
    // Active-session rejection verifies which session is selected without
    // starting a model turn or invoking the legacy /status pager.
    command(
        child,
        &format!("/session delete {session}"),
        "refusing to delete the active session",
    );
}

#[test]
fn session_picker_filters_switches_and_returns_to_the_prompt() {
    let env = common::TestEnv::new("iocraft-session-picker");
    let mut child = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line_cleared(&child, common::DEFAULT_TIMEOUT, "session prompt");
    command(&mut child, "/session fork picker-target", "Session forked");
    let (target, target_path) = branch_session(env.workspace_root(), "picker-target");
    command(&mut child, "/session fork picker-current", "Session forked");
    let (current, _) = branch_session(env.workspace_root(), "picker-current");

    for picker in ["/session", "/resume"] {
        child.send(&format!("{picker}\r")).unwrap();
        child.expect("Select a session").unwrap();
        child.send("\x1b").unwrap();
        common::expect_input_line_cleared(&child, common::DEFAULT_TIMEOUT, "session prompt");
        assert_active_session(&mut child, &current);
    }

    child.send("/session list\r").unwrap();
    child.expect("Select a session").unwrap();
    child.send("picker-target").unwrap();
    child
        .expect(r"1 match.*filter: picker-target")
        .unwrap_or_else(|error| panic!("filter: {error}; {}", common::screen_tail(&child, 6000)));
    child.send("\r").unwrap();
    child.expect("Session switched").unwrap();
    child.expect(&target).unwrap();
    common::expect_input_line_cleared(&child, common::DEFAULT_TIMEOUT, "session prompt");
    assert_active_session(&mut child, &target);

    // Active-session protection still applies in the new confirmation path.
    command(
        &mut child,
        &format!("/session delete {target}"),
        "refusing to delete the active session",
    );
    assert!(target_path.exists());
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn session_delete_can_cancel_then_confirm_without_force() {
    let env = common::TestEnv::new("iocraft-session-delete");
    let mut child = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line_cleared(&child, common::DEFAULT_TIMEOUT, "session prompt");
    command(&mut child, "/session fork delete-target", "Session forked");
    let (target, path) = branch_session(env.workspace_root(), "delete-target");
    command(&mut child, "/session fork keep-current", "Session forked");
    let (current, current_path) = branch_session(env.workspace_root(), "keep-current");

    for cancel in ["\x1b", "\r"] {
        child.send(&format!("/session delete {target}\r")).unwrap();
        child.expect("This cannot be undone").unwrap();
        child.send(cancel).unwrap();
        common::expect_input_line_cleared(&child, env.timeout(), "cancel returns to prompt");
        assert!(path.exists(), "cancel must preserve the saved session");
        assert_active_session(&mut child, &current);
    }
    child.send(&format!("/session delete {target}\r")).unwrap();
    child.expect("This cannot be undone").unwrap();
    child.send("\x1b[B\r").unwrap();
    child.expect("Session deleted").unwrap();
    common::expect_input_line_cleared(&child, common::DEFAULT_TIMEOUT, "session prompt");
    assert!(!path.exists());
    assert!(current_path.exists());
    assert_active_session(&mut child, &current);
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
}

#[test]
fn cancelled_session_picker_does_not_consume_the_next_agent_answer() {
    let env = common::TestEnv::new("cancelled-session-question");
    if env.is_live() {
        return;
    }
    let mut child = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_input_line_cleared(&child, env.timeout(), "initial prompt");
    child.send("/session\r").unwrap();
    child.expect("Select a session").unwrap();
    child.send("\x1b").unwrap();
    common::expect_input_line_cleared(&child, env.timeout(), "cancelled picker");
    let prompt = env.prompt("", "ask_user_question_roundtrip");
    child.send(&format!("{prompt}\r")).unwrap();
    child.expect("Which colour?").unwrap();
    child.send("\r").unwrap();
    child.expect("ask_user_question answered:").unwrap();
    common::expect_input_line_cleared(&child, env.timeout(), "answered question");
    child.send("/exit\r").unwrap();
    assert_eq!(child.expect_eof().unwrap(), 0);
}
