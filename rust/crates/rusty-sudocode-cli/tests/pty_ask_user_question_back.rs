//! PTY test: a multi-question `AskUserQuestion` can go back to an earlier
//! question, which reopens with its earlier answer selected (Claude Code
//! parity: ← or Shift+Tab goes to the previous question).

mod common;
#[path = "support/request_evidence.rs"]
mod request_evidence;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::{json, Value};

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

fn question_requests(env: &TestEnv) -> Vec<Value> {
    if env.is_mock() {
        return env
            .captured_message_bodies()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect();
    }
    let log = env.workspace_root().join("question-requests.jsonl");
    let requests = request_evidence::accepted_messages(&log);
    assert!(
        !requests.is_empty(),
        "successful message requests missing from HTTP telemetry: {}",
        std::fs::read_to_string(log).unwrap()
    );
    requests
}

fn tool_result(requests: &[Value], tool_id: &str) -> Value {
    requests
        .iter()
        .rev()
        .flat_map(|request| request["messages"].as_array().into_iter().flatten())
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == tool_id)
        .cloned()
        .expect("AskUserQuestion result")
}

fn result_value(result: &Value) -> Value {
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
    let result = result_value(&tool_result(
        &question_requests(&env),
        "toolu_ask_user_question_two_fields",
    ));
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

fn expect_question(env: &TestEnv, sess: &PtySession, prompt: &str) -> String {
    let screen = common::expect_screen(
        sess,
        |screen| common::input_and_footer_of(screen).is_some_and(|area| area.contains(prompt)),
        env.timeout(),
        prompt,
    );
    common::input_and_footer_of(&screen).unwrap()
}

fn assert_contract_exchange(env: &TestEnv, input: &Value) {
    let requests = question_requests(env);
    let call = requests
        .iter()
        .rev()
        .flat_map(|request| request["messages"].as_array().into_iter().flatten())
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_use" && block["name"] == "AskUserQuestion")
        .unwrap_or_else(|| panic!("actual model question call in HTTP requests: {requests:#?}"));
    assert_eq!(
        &call["input"], input,
        "exercise omitted fields at the model boundary"
    );
    let result = result_value(&tool_result(&requests, call["id"].as_str().unwrap()));
    let actual = result["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|answer| answer["value"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        [
            "2",
            "1",
            "TypedTextAnswer",
            "value9",
            "UnlistedCustomAnswer",
            "alpha"
        ]
    );
    let policy = result["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["allowCustomInput"].as_bool().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(policy, [true, false, true, true, true, false]);

    let spec = requests
        .iter()
        .flat_map(|request| request["tools"].as_array().into_iter().flatten())
        .find(|tool| tool["name"] == "AskUserQuestion")
        .expect("advertised question schema");
    let schema = &spec["input_schema"];
    assert_eq!(schema["required"], json!(["questions"]));
    assert_eq!(schema["properties"]["questions"]["minItems"], 1);
    assert!(
        schema["properties"].get("question").is_none()
            && schema["properties"].get("options").is_none()
    );
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["questions"]["items"]["properties"]["allowCustomInput"]["default"],
        true
    );
}

fn contract_input() -> Value {
    let options = json!([{"label":"SuggestedAlpha","value":"alpha"},
                         {"label":"SuggestedBeta","value":"beta"}]);
    let many = (1..=12)
        .map(|n| json!({"label":format!("ContractOption{n}"), "value":format!("value{n}")}))
        .collect::<Vec<_>>();
    json!({"questions":[
        {"id":"default", "prompt":"DEFAULT_CHOICE", "options":options},
        {"id":"closed", "prompt":"CLOSED_CHOICE", "allowCustomInput":false, "options":[{"label":"NumericAlpha","value":"2"},{"label":"NumericBeta","value":"1"}]},
        {"id":"text", "prompt":"TEXT_ANSWER", "kind":"text", "allowCustomInput":false},
        {"id":"matched", "prompt":"MATCHED_SEARCH", "options":many},
        {"id":"unmatched", "prompt":"UNMATCHED_SEARCH", "options":many},
        {"id":"confirm", "prompt":"FINAL_CONFIRM", "allowCustomInput":false, "options":options}
    ]})
}

/// Exercise the model input, live editor and actual API tool result together.
#[test]
fn question_custom_input_defaults_and_explicit_opt_out_roundtrip() {
    let env = TestEnv::new("question-custom-policy");
    let input = contract_input();
    let prompt = if env.is_mock() {
        env.tool_batch_prompt(
            &json!([{"id":"question-contract", "name":"AskUserQuestion", "input":input}]),
        )
    } else {
        format!("Call AskUserQuestion exactly once with this exact input: {input}. Preserve omitted fields, including allowCustomInput: do not add that field where absent. Wait for all answers, then reply QUESTION_CONTRACT_DONE. Do not call other tools.")
    };
    let trace = env.workspace_root().join("question-requests.jsonl");
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "read-only",
            "--allowedTools",
            "AskUserQuestion",
        ],
        &[("SCODE_LOG_PATH", trace.to_str().unwrap())],
    );
    common::expect_input_line_cleared(&sess, env.timeout(), "initial prompt");
    let turn = common::turn_status_marker(&sess);
    sess.send(&format!("{prompt}\r")).unwrap();

    assert!(expect_question(&env, &sess, "DEFAULT_CHOICE").contains("[+]"));
    sess.send(&format!("{DOWN}{DOWN}")).unwrap();
    common::expect_screen(
        &sess,
        |screen| {
            common::input_and_footer_of(screen).is_some_and(|area| {
                area.lines()
                    .any(|line| line.trim_start().starts_with("> [+]"))
            })
        },
        env.timeout(),
        "custom answer focus before typing digits",
    );
    sess.send("2\r").unwrap();
    let closed = expect_question(&env, &sess, "CLOSED_CHOICE");
    assert!(!closed.contains("[+]"), "explicit opt-out: {closed}");
    sess.send(LEFT).unwrap();
    let restored = expect_question(&env, &sess, "DEFAULT_CHOICE");
    assert!(
        restored
            .lines()
            .any(|line| line.trim_start().starts_with("> [+]") && line.contains('2')),
        "restore the custom answer's focus: {restored}"
    );
    sess.send("\r").unwrap();
    expect_question(&env, &sess, "CLOSED_CHOICE");
    sess.send("2").unwrap();
    assert!(expect_question(&env, &sess, "TEXT_ANSWER").contains("[+]"));
    sess.send("TypedTextAnswer\r").unwrap();

    expect_question(&env, &sess, "MATCHED_SEARCH");
    sess.send("ContractOption9").unwrap();
    common::expect_screen(
        &sess,
        |screen| {
            common::input_and_footer_of(screen)
                .is_some_and(|area| area.contains("filter: ContractOption9"))
        },
        env.timeout(),
        "matched filter",
    );
    sess.send("\r").unwrap();
    expect_question(&env, &sess, "UNMATCHED_SEARCH");
    sess.send("UnlistedCustomAnswer").unwrap();
    common::expect_screen(
        &sess,
        |screen| {
            common::input_and_footer_of(screen).is_some_and(|area| {
                area.contains("no match") && area.contains("UnlistedCustomAnswer")
            })
        },
        env.timeout(),
        "unmatched custom input",
    );
    sess.send("\r").unwrap();
    expect_question(&env, &sess, "FINAL_CONFIRM");
    sess.send(LEFT).unwrap();
    let restored = expect_question(&env, &sess, "UNMATCHED_SEARCH");
    assert!(
        restored.contains("filter: UnlistedCustomAnswer") && restored.contains("no match"),
        "restore the custom filter: {restored}"
    );
    sess.send("\r").unwrap();
    expect_question(&env, &sess, "FINAL_CONFIRM");
    sess.send("1").unwrap();
    common::expect_turn_complete_after(&sess, &turn, env.timeout(), "question contract roundtrip");

    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    assert_contract_exchange(&env, &input);
}

#[test]
fn obsolete_question_formats_fail_without_opening_an_editor() {
    let env = TestEnv::new("question-obsolete-input");
    if env.is_live() {
        return;
    }
    let inputs = [
        json!({"question":"Obsolete top level", "options":["A", "B"]}),
        json!({"questions":[{"id":"q1", "question":"Obsolete alias"}]}),
        json!({"questions":[]}),
        json!({"title":"Missing questions"}),
        json!({"question":"Obsolete extra root field", "questions":[{"id":"q1", "prompt":"Valid required field"}]}),
        json!({"questions":[{"id":"q1", "prompt":"Valid required field", "question":"Obsolete extra alias"}]}),
    ];
    let expected_errors = inputs.len();
    let calls = inputs
        .into_iter()
        .enumerate()
        .map(|(i, input)| {
            json!({
                "id":format!("invalid-question-{i}"), "name":"AskUserQuestion", "input":input
            })
        })
        .collect::<Vec<_>>();
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[]);
    common::expect_input_line_cleared(&sess, env.timeout(), "initial prompt");
    sess.send(&format!("{}\r", env.tool_batch_prompt(&json!(calls))))
        .unwrap();
    sess.expect("Concurrency batch done.").unwrap();
    for i in 0..expected_errors {
        let result = tool_result(&question_requests(&env), &format!("invalid-question-{i}"));
        assert_eq!(
            result["is_error"], true,
            "invalid input must reach the model as a tool error: {result}"
        );
    }
    common::expect_input_line_cleared(&sess, env.timeout(), "invalid questions release input");
    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}
