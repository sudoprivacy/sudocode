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
                .rfind(|line| line.trim_start().starts_with("> ["));
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

/// The committed card says what was asked and what was chosen: every option
/// in the dialog's order, the chosen one marked and bold, the rest dimmed, and
/// none of the raw result fields.
// The cell type belongs to pty-expect's vt100, which these tests don't depend
// on directly, so it can't be named as a method path.
#[allow(clippy::redundant_closure_for_method_calls)]
fn assert_answered_card(sess: &PtySession) {
    let (rows, bold) = sess.render(|screen| {
        let raw = screen.raw();
        let (height, width) = raw.size();
        let rows: Vec<String> = raw.rows(0, width).take(usize::from(height)).collect();
        let bold: Vec<bool> = (0..height)
            .map(|row| {
                (0..width)
                    .find(|&col| {
                        raw.cell(row, col)
                            .is_some_and(|cell| cell.contents() == "→")
                    })
                    .and_then(|col| raw.cell(row, col))
                    .is_some_and(|cell| cell.bold())
            })
            .collect();
        (rows, bold)
    });
    let start = rows
        .iter()
        .rposition(|row| row.contains("╭─ AskUserQuestion"))
        .unwrap_or_else(|| panic!("answered card:\n{}", rows.join("\n")));
    let end = start
        + rows[start..]
            .iter()
            .position(|row| row.trim_start().starts_with("╰─"))
            .expect("card closes");
    let card: Vec<&str> = rows[start + 1..end]
        .iter()
        .map(|row| row.trim_start().trim_start_matches('│').trim_end())
        .collect();
    assert!(
        rows[start].contains("AskUserQuestion · 2 questions"),
        "{}",
        rows[start]
    );
    assert_eq!(
        card,
        [
            " Pick a fruit?",
            " → apple",
            "   pear",
            "   plum",
            " Pick a size?",
            "   small",
            " → large",
        ],
        "card rows"
    );
    let marked: Vec<bool> = (start + 1..end).map(|row| bold[row]).collect();
    assert_eq!(
        marked,
        [false, true, false, false, false, false, true],
        "only the chosen rows are bold"
    );
}

#[test]
fn going_back_reopens_an_answered_question_with_its_answer_selected() {
    let env = TestEnv::new("ask-user-question-back");
    if env.is_live() {
        return;
    }
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[]);
    // Tall enough to keep the committed card on screen above the mock's reply,
    // which echoes the whole raw result.
    sess.resize(200, 120).unwrap();
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
    assert_answered_card(&sess);
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
