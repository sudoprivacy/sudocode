//! Real PTY acceptance of application edits followed immediately by typing.
//! `SCODE_TEST_BACKEND=live` uses the configured real API for paste submission.
mod common;

use common::TestEnv;
use runtime::{ContentBlock, MessageRole, Session};
use std::path::{Path, PathBuf};

fn start(env: &TestEnv) -> pty_expect::PtySession {
    let session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    expect_input(&session, "", env, "initial input");
    session
}

fn expect_input(session: &pty_expect::PtySession, text: &str, env: &TestEnv, context: &str) {
    common::expect_screen_settled(
        session,
        |screen| screen.contains('\u{276f}') && common::input_line_of(screen) == text,
        env.timeout(),
        context,
    );
}

fn exit(session: &mut pty_expect::PtySession, env: &TestEnv) {
    session.send("\x15/exit").unwrap();
    expect_input(session, "/exit", env, "clear then type exit");
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn clear_and_typing_share_one_input_sequence() {
    let env = TestEnv::new("input-batch-clear");
    let mut session = start(&env);
    session.send("OldDraft").unwrap();
    expect_input(&session, "OldDraft", &env, "original draft");
    // One write, without a test-only pause after the parent shortcut.
    session.send("\x15FreshDraft").unwrap();
    expect_input(&session, "FreshDraft", &env, "replacement draft");
    exit(&mut session, &env);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[test]
fn logical_line_navigation_and_typing_share_one_input_sequence() {
    let env = TestEnv::new("input-batch-navigation");
    let mut session = start(&env);
    let draft = format!("BatchHead{}BatchTail", "_keep_".repeat(32));
    session.send(&draft).unwrap();
    common::expect_screen_settled(
        &session,
        |screen| {
            common::input_and_footer_of(screen)
                .is_some_and(|input| input.contains("BatchHead") && input.contains("BatchTail"))
        },
        env.timeout(),
        "wrapped draft",
    );
    // Up's logical-line jump must apply before Home/right/edit in the same
    // burst, not via a parent cursor request that waits for another render.
    session
        .send("\x1b[A\x1b[H\x1b[C\x1b[C\x1b[C\x1b[C\x1b[CX")
        .unwrap();
    common::expect_screen_settled(
        &session,
        |screen| {
            common::input_and_footer_of(screen).is_some_and(|input| input.contains("BatchXHead"))
        },
        env.timeout(),
        "logical start and middle insertion",
    );
    // The symmetric Down jump also belongs to the editor's event-time cursor.
    session.send("\x7f\x1b[B\x1b[D\x1b[D\x1b[D\x1b[DZ").unwrap();
    common::expect_screen_settled(
        &session,
        |screen| {
            common::input_and_footer_of(screen).is_some_and(|input| input.contains("BatchZTail"))
        },
        env.timeout(),
        "logical end and middle insertion",
    );
    exit(&mut session, &env);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}

fn transcripts(root: &Path, paths: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).expect("session directory") {
        let path = entry.unwrap().path();
        if path.is_dir() {
            transcripts(&path, paths);
        } else if path
            .file_name()
            .is_some_and(|name| name == "transcript.jsonl")
        {
            paths.push(path);
        }
    }
}

#[test]
fn middle_paste_then_typing_preserves_submitted_content() {
    let env = TestEnv::new("input-batch-paste");
    let mut session = start(&env);
    session.send("HeadTail").unwrap();
    expect_input(&session, "HeadTail", &env, "draft before paste");
    session
        .send("\x1b[H\x1b[C\x1b[C\x1b[C\x1b[C\x1b[200~中\x1b[201~Fresh")
        .unwrap();
    expect_input(&session, "Head中FreshTail", &env, "paste at middle cursor");

    let prompt = env.prompt(
        "Calculate the sum shown below:\n2\n+\n2",
        "single_turn_text",
    );
    let suffix = " Answer with just the number.";
    // Clear, paste a multiline prompt and continue typing in the same burst.
    session
        .send(&format!("\x15\x1b[200~{prompt}\x1b[201~{suffix}"))
        .unwrap();
    expect_input(
        &session,
        &format!("[Pasted text #1 +3 lines]{suffix}"),
        &env,
        "placeholder and subsequent typing",
    );
    let marker = common::turn_status_marker(&session);
    session.send("\r").unwrap();
    let turn_budget = if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        env.timeout()
    };
    common::expect_turn_complete_after(&session, &marker, turn_budget, "pasted prompt turn");
    exit(&mut session, &env);

    let mut paths = Vec::new();
    transcripts(&env.workspace_root().join(".scode"), &mut paths);
    assert_eq!(paths.len(), 1, "one conversation");
    let saved = Session::load_from_path(&paths[0]).unwrap();
    let text = |role| {
        saved
            .messages
            .iter()
            .rev()
            .find(|message| message.role == role)
            .expect("saved conversation role")
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
    };
    let submitted = text(MessageRole::User);
    assert!(
        submitted.contains(&format!("{prompt}{suffix}")),
        "submitted content: {submitted:?}"
    );
    assert!(
        !submitted.contains("[Pasted text #"),
        "placeholder was not expanded"
    );
    assert_eq!(
        text(MessageRole::Assistant).trim(),
        if env.is_mock() {
            "The answer is 4"
        } else {
            "4"
        },
        "saved assistant result"
    );
}
