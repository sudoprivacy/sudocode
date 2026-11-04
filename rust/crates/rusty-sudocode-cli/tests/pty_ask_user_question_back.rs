//! PTY test: a multi-question `AskUserQuestion` can go back to an earlier
//! question, which reopens with its earlier answer selected (Claude Code
//! parity: ← or Shift+Tab goes to the previous question).

mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::Value;

const DOWN: &str = "\x1b[B";
const UP: &str = "\x1b[A";
const LEFT: &str = "\x1b[D";
const SHIFT_TAB: &str = "\x1b[Z";

/// Wait until the live question shows `prompt` with `label` selected. The
/// live panel is the bottom of the screen, so its selected row is the last.
fn expect_selected(env: &TestEnv, sess: &PtySession, prompt: &str, label: &str) -> String {
    common::expect_screen(
        sess,
        |screen| {
            let selected = screen
                .lines()
                .filter(|line| line.trim_start().starts_with("> ["))
                .last();
            screen.contains(prompt) && selected.is_some_and(|line| line.contains(label))
        },
        env.timeout(),
        &format!("{prompt} with {label} selected"),
    )
}

fn answers(env: &TestEnv) -> Value {
    let body = env
        .captured_message_bodies()
        .into_iter()
        .rev()
        .find(|body| body.contains("toolu_ask_user_question_two_fields"))
        .expect("request carrying the answers");
    let request: Value = serde_json::from_str(&body).unwrap();
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .find(|block| {
            block["type"] == "tool_result"
                && block["tool_use_id"] == "toolu_ask_user_question_two_fields"
        })
        .expect("AskUserQuestion result");
    let text = match &result["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks[0]["text"].as_str().unwrap().to_string(),
        other => panic!("unexpected tool_result content: {other}"),
    };
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

#[test]
fn going_back_reopens_an_answered_question_with_its_answer_selected() {
    let env = TestEnv::new("ask-user-question-back");
    if env.is_live() {
        return;
    }
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[]);
    common::expect_input_line_cleared(&sess, env.timeout(), "initial prompt");
    let prompt = env.prompt("", "ask_user_question_two_fields");
    sess.send(&format!("{prompt}\r")).unwrap();

    expect_selected(&env, &sess, "Pick a fruit?", "apple");
    sess.send(DOWN).unwrap();
    expect_selected(&env, &sess, "Pick a fruit?", "pear");
    sess.send("\r").unwrap();

    let size = expect_selected(&env, &sess, "Pick a size?", "small");
    assert!(
        size.contains("← back"),
        "second question offers back:\n{size}"
    );

    // ← reopens the first question on the answer already given.
    sess.send(LEFT).unwrap();
    expect_selected(&env, &sess, "Pick a fruit?", "pear");
    sess.send(UP).unwrap();
    expect_selected(&env, &sess, "Pick a fruit?", "apple");
    sess.send("\r").unwrap();

    // Shift+Tab goes back too, and the changed answer is the one restored.
    expect_selected(&env, &sess, "Pick a size?", "small");
    sess.send(SHIFT_TAB).unwrap();
    expect_selected(&env, &sess, "Pick a fruit?", "apple");
    sess.send("\r").unwrap();
    expect_selected(&env, &sess, "Pick a size?", "small");
    sess.send(DOWN).unwrap();
    expect_selected(&env, &sess, "Pick a size?", "large");
    sess.send("\r").unwrap();

    sess.expect("ask_user_question answered:").unwrap();
    common::expect_input_line_cleared(&sess, env.timeout(), "answered question");
    let result = answers(&env);
    let given: Vec<(String, String)> = result["answers"]
        .as_array()
        .unwrap_or_else(|| panic!("structured answers: {result}"))
        .iter()
        .map(|answer| {
            (
                answer["id"].as_str().unwrap().to_string(),
                answer["value"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // The corrected first answer replaces the old one; each question once.
    assert_eq!(
        given,
        [
            ("fruit".into(), "apple".into()),
            ("size".into(), "large".into())
        ]
    );

    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}
