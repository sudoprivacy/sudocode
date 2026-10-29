//! Queue three inputs during a real blocked tool, release it, and verify one
//! combined provider request and a persisted answer. The provider is controlled
//! because this checks client batching, not a model's choice of tools.
mod common;
#[path = "support/wire_requests.rs"]
mod wire_requests;

use common::TestEnv;
use runtime::{ContentBlock, MessageRole, Session};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};

#[inline]
fn shell_path(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''")
    )
}

#[test]
fn three_queued_inputs_flush_as_one_combined_downstream_request() {
    let env = TestEnv::new_mock("batched-flush-request-count");
    let started = env.workspace_root().join("tool-started");
    let release = env.workspace_root().join("release-tool");
    let batch = env.workspace_root().join("tool-batch.json");
    // Keep the original 30-second safety bound, but finish as soon as all three
    // inputs have actually been queued. A missing release fails the tool.
    let command = format!(
        "printf running > {}; deadline=$(($(date +%s) + 30)); \
         while [ ! -f {gate} ] && [ \"$(date +%s)\" -lt \"$deadline\" ]; do sleep 0.05; done; \
         test -f {gate} || exit 9; printf QUEUE_TOOL_FINISHED",
        shell_path(&started),
        gate = shell_path(&release),
    );
    fs::write(
        &batch,
        json!([{
            "id":"queue-gate", "name":"Bash", "input":{
                "command":command, "description":"Wait for queued inputs"
            }
        }])
        .to_string(),
    )
    .unwrap();
    let prompt = env.prompt(
        &format!("TOOL_BATCH_FILE:{}", json!(batch.to_string_lossy())),
        "tool_concurrency",
    );
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.resize(48, u16::try_from(prompt.chars().count() * 2 + 8).unwrap())
        .unwrap();
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "initial input");
    let before = common::turn_status_marker(&sess);
    let request_baseline = env.captured_message_count();
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_input_line(&sess, &prompt, common::DEFAULT_TIMEOUT, "tool prompt");
    sess.send("\r").unwrap();
    // An echoed command is not evidence that the child started. Its fresh file is.
    assert_eq!(
        common::read_file_with_retry(
            &started,
            u32::try_from(common::DEFAULT_TIMEOUT.as_millis() / 50).unwrap(),
            Duration::from_millis(50)
        )
        .as_deref(),
        Some("running"),
        "the real Bash tool must be waiting before input is queued",
    );
    let markers = ["QUEUED_B", "QUEUED_C", "QUEUED_D"];
    for marker in markers {
        let input = env.prompt(marker, "single_turn_text");
        sess.send(&format!("{input}\t")).unwrap();
        // The active spinner need not settle. This receipt is emitted only
        // after the submit event queues the draft and clears the input.
        let receipt = format!("↳ queued: {input}");
        common::expect_screen(
            &sess,
            |screen| common::screen_contains(screen, &receipt),
            common::DEFAULT_TIMEOUT,
            "input accepted into queue before the next draft",
        );
    }
    fs::write(&release, "go").unwrap();
    sess.set_default_timeout(common::at_least(Duration::from_secs(50)));
    // Only the queued turn uses this scenario; the tool turn answers differently.
    sess.expect("The answer is 4")
        .expect("the queued batch must receive an answer");
    common::expect_turn_complete_after(
        &sess,
        &before,
        common::at_least(Duration::from_secs(50)),
        "queued answer completed",
    );
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    assert_eq!(sess.expect_eof().expect("clean exit"), 0);

    assert_batched_response(&env, request_baseline, markers);
}

#[inline]
fn assert_batched_response(env: &TestEnv, request_baseline: usize, markers: [&str; 3]) {
    let requests: Vec<Value> = env
        .captured_message_bodies()
        .iter()
        .skip(request_baseline)
        .map(|body| serde_json::from_str(body).unwrap())
        .collect();
    assert_eq!(
        requests.len(),
        3,
        "tool call, tool result, then exactly one queued batch"
    );
    let results = wire_requests::request_tool_blocks(&requests[1]);
    let completed = results
        .iter()
        .find(|block| {
            block["type"] == "tool_result"
                && block["content"].to_string().contains("QUEUE_TOOL_FINISHED")
        })
        .expect("the actual tool must finish naturally before the batch");
    assert_ne!(completed["is_error"], true);
    let last = requests[2]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    let content = last["content"].to_string();
    for marker in markers {
        assert_eq!(
            content.matches(marker).count(),
            1,
            "each queued input must occur once in the same final user message: {content}"
        );
    }
    let transcript = common::find_session_transcript(env.workspace_root()).unwrap();
    let session = Session::load_from_path(transcript).unwrap();
    let answer = session
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::Assistant)
        .unwrap();
    assert!(
        answer.blocks.iter().any(|block| matches!(block,
        ContentBlock::Text { text } if text.contains("The answer is 4"))),
        "the batch answer must be persisted, not just echoed input"
    );
}
