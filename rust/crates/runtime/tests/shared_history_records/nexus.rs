//! The same context boundaries over real mTLS clients and daemon restart.
use std::path::Path;

use runtime::fs_backend::{FsBackend, RecordLogMode};
use runtime::history::{HistoryLog, HistoryRecord};
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};

use super::live_nexus::backend as live_backend;
use super::{
    append, assistant, commit, complete_compacted_branch, finish_turn, payloads, text, wait_for,
    Fixture, ToolChild,
};

fn seed(log: &mut HistoryLog) -> Session {
    let mut prefix = Session::new();
    prefix.push_user_text("NEXUS_COMMON_PREFIX").unwrap();
    prefix.push_message(text("NEXUS_COMMON_ANSWER")).unwrap();
    commit(log, &HistoryRecord::initialize(&prefix).unwrap());
    prefix
}

#[test]
#[ignore = "real mTLS daemon; e2e/nexus-a2a/run.sh supplies the fixture"]
fn live_shared_history_preserves_contexts_while_sibling_tool_runs() {
    let fixture = Fixture::new();
    let fs = live_backend();
    let directory = format!("/agents/live-probe/history-{}", uuid::Uuid::new_v4());
    fs.create_dir_all(&directory).unwrap();
    let path = format!("{directory}/events");
    let mut a = HistoryLog::new(fs.record_log(&path, RecordLogMode::CreateNew).unwrap());
    let prefix = seed(&mut a);
    let branch_a = HistoryRecord::branch_latest();
    commit(&mut a, &branch_a);
    let context_a = branch_a.context_id();
    append(
        &mut a,
        context_a,
        &ConversationMessage::user_text("NEXUS_A_PRIVATE"),
    );
    let assistant_id = append(
        &mut a,
        context_a,
        &assistant(vec![ContentBlock::ToolUse {
            id: "nexus-call-a".into(),
            name: "fixture_tool".into(),
            input: "{}".into(),
            thought_signature: None,
        }]),
    );
    let mut tool = ToolChild::spawn(&fixture.0);
    wait_for(&fixture.0.join("tool-started"));
    let mut b = HistoryLog::new(
        live_backend()
            .record_log(&path, RecordLogMode::Existing)
            .unwrap(),
    );
    let branch_b = HistoryRecord::branch_latest_completed();
    commit(&mut b, &branch_b);
    let context_b = branch_b.context_id();
    assert_eq!(
        payloads(b.graph().context_session(context_b).unwrap().messages()),
        payloads(prefix.messages())
    );
    let (compacted, b_commit) = complete_compacted_branch(
        &mut b,
        context_b,
        "NEXUS_B_PRIVATE",
        "NEXUS_B_FINISHED",
        "NEXUS_B_COMPACTED",
        "nexus-b-trace",
    );
    let mut cold = HistoryLog::new(
        live_backend()
            .record_log(&path, RecordLogMode::Existing)
            .unwrap(),
    );
    let after_compaction = HistoryRecord::branch_latest();
    commit(&mut cold, &after_compaction);
    assert_eq!(
        payloads(
            cold.graph()
                .context_session(after_compaction.context_id())
                .unwrap()
                .messages()
        ),
        payloads(compacted.messages()),
        "a fresh Nexus context must inherit the completed compacted view"
    );
    assert!(!fixture.0.join("actual-tool-result").exists());
    std::fs::write(fixture.0.join("tool-release"), b"continue").unwrap();
    assert!(tool.0.wait().unwrap().success());
    let output = std::fs::read_to_string(fixture.0.join("actual-tool-result")).unwrap();
    let result_id = append(
        &mut a,
        context_a,
        &ConversationMessage::tool_result("nexus-call-a", "fixture_tool", output, false),
    );
    append(&mut a, context_a, &text("NEXUS_A_FINISHED"));
    let a_commit = finish_turn(&mut a, context_a, "nexus-a-trace");
    let mut idle = b.graph().context_session(context_b).unwrap();
    idle.model = Some("NEXUS_B_IDLE_METADATA".into());
    let idle_save = b.graph().prepare_metadata(context_b, &idle).unwrap();
    commit(&mut b, &idle_save);
    assert_eq!(b.graph().latest_commit(), Some(a_commit.as_str()));
    assert_eq!(
        a.graph().tool_origin(&result_id, "nexus-call-a"),
        Some(assistant_id.as_str())
    );
    let receipt = std::env::var("NEXUS_HISTORY_RECEIPT").expect("history restart receipt path");
    std::fs::write(
        receipt,
        serde_json::to_vec(&json!({
            "path":path, "sid":prefix.session_id, "a_context":context_a, "b_context":context_b,
            "a_commit":a_commit, "b_commit":b_commit, "assistant":assistant_id, "result":result_id,
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "real daemon restart; e2e/nexus-a2a/run.sh owns stop/start"]
fn live_shared_history_restores_both_contexts_after_restart() {
    let receipt = std::env::var("NEXUS_HISTORY_RECEIPT").expect("history restart receipt path");
    let before: Value = serde_json::from_slice(&std::fs::read(&receipt).unwrap()).unwrap();
    let mut log = HistoryLog::new(
        live_backend()
            .record_log(before["path"].as_str().unwrap(), RecordLogMode::Existing)
            .unwrap(),
    );
    log.refresh().unwrap();
    let a = log
        .graph()
        .completed_session(Some(before["a_commit"].as_str().unwrap()))
        .unwrap();
    assert_eq!(a.session_id, before["sid"]);
    assert_ne!(a.model.as_deref(), Some("NEXUS_B_IDLE_METADATA"));
    assert!(a.to_json().unwrap().render().contains("REAL_TOOL_A_RESULT"));
    assert!(!a.to_json().unwrap().render().contains("NEXUS_B_COMPACTED"));
    let b = log
        .graph()
        .completed_session(Some(before["b_commit"].as_str().unwrap()))
        .unwrap();
    assert_eq!(b.messages().len(), 2);
    assert!(b.to_json().unwrap().render().contains("NEXUS_B_COMPACTED"));
    assert!(!b.to_json().unwrap().render().contains("NEXUS_A_PRIVATE"));
    assert_eq!(
        log.graph()
            .tool_origin(before["result"].as_str().unwrap(), "nexus-call-a"),
        before["assistant"].as_str()
    );
    let branch = HistoryRecord::branch_latest();
    commit(&mut log, &branch);
    assert_eq!(
        log.graph()
            .context_session(branch.context_id())
            .unwrap()
            .messages(),
        a.messages()
    );
    append(
        &mut log,
        branch.context_id(),
        &ConversationMessage::user_text("AFTER_NEXUS_RESTART"),
    );
    append(&mut log, branch.context_id(), &text("POST_RESTART_ANSWER"));
    finish_turn(&mut log, branch.context_id(), "after-restart-trace");
    std::fs::write(
        Path::new(&receipt).with_extension("after-restart.json"),
        serde_json::to_vec(
            &json!({"result":"PASS", "sid":a.session_id, "retained_branches":2,
            "post_restart_commit":log.graph().latest_commit()}),
        )
        .unwrap(),
    )
    .unwrap();
}
