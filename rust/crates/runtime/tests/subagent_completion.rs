//! Completion delivery and filesystem persistence contract.
use runtime::subagent_completion::CompletionJournal;
use runtime::{Session, SubagentIdentity, SubagentLifecycle, SubagentPhase};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn lifecycle(phase: SubagentPhase) -> SubagentLifecycle {
    SubagentLifecycle {
        tool_call_id: "spawn-1".into(),
        assistant_message_id: None,
        phase,
        seq: 0,
        agent: SubagentIdentity {
            agent_id: "agent-1".into(),
            name: "worker".into(),
            description: "test".into(),
            subagent_type: None,
            model: None,
            color: None,
            background: false,
        },
        status: (phase == SubagentPhase::Finished).then(|| "completed".into()),
        started_at: None,
        completed_at: None,
        raw_output: None,
        completion_notification: (phase == SubagentPhase::Finished)
            .then(|| "<task-notification>done</task-notification>".into()),
    }
}

#[test]
fn handoff_terminal_and_closed_are_all_required_in_every_order() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        for status in ["running", "backgrounded", "completed"] {
            let store = Store::new();
            let journal = store.open(true);
            journal.begin("run:1", None).unwrap();
            journal
                .register(lifecycle(SubagentPhase::Started), "child.json".into())
                .unwrap();
            for (step, event) in order.iter().enumerate() {
                match event {
                    0 => journal
                        .lifecycle(&lifecycle(SubagentPhase::Finished))
                        .unwrap(),
                    1 => journal
                        .tool_result(
                            None,
                            "spawn-1",
                            "agent_spawn",
                            &format!("{{\"status\":\"{status}\"}}"),
                        )
                        .unwrap(),
                    _ => {
                        journal.commit("run:1").unwrap();
                    }
                }
                if step < 2 {
                    assert!(journal.pending().is_empty(), "{order:?}, {status}");
                }
            }
            let expected = usize::from(status != "completed");
            assert_eq!(journal.pending().len(), expected);
            drop(journal);
            assert_eq!(store.open(false).pending().len(), expected);
        }
    }
}

#[test]
fn explicit_collection_before_publication_suppresses_even_before_finished_event() {
    for terminal_first in [true, false] {
        for output in [
            r#"{"status":"completed","agentId":"agent-1"}"#,
            "<task-notification>done</task-notification>",
        ] {
            let store = Store::new();
            let journal = store.open(true);
            Store::seed_source(&journal);
            if terminal_first {
                journal
                    .lifecycle(&lifecycle(SubagentPhase::Finished))
                    .unwrap();
            }
            journal
                .tool_result(None, "read-1", "pid_output", output)
                .unwrap();
            if !terminal_first {
                journal
                    .lifecycle(&lifecycle(SubagentPhase::Finished))
                    .unwrap();
            }
            journal.commit("run:1").unwrap();
            assert_eq!(
                journal.pending(),
                [] as [runtime::subagent_completion::Completion; 0]
            );
            drop(journal);
            assert_eq!(
                store.open(false).pending(),
                [] as [runtime::subagent_completion::Completion; 0]
            );
        }
    }
}

#[test]
fn unrelated_tool_output_cannot_consume_completion() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    journal
        .tool_result(
            None,
            "read-1",
            "read_file",
            r#"{"status":"completed","agentId":"agent-1"}"#,
        )
        .unwrap();
    journal
        .lifecycle(&lifecycle(SubagentPhase::Finished))
        .unwrap();
    journal.commit("run:1").unwrap();
    let before = journal.pending();
    assert_eq!(before.len(), 1);
    drop(journal);
    assert_eq!(store.open(false).pending(), before);
}

struct Store {
    dir: PathBuf,
    session: Session,
}
impl Store {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "scode-completion-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session::new().with_persistence_path(dir.join("session.jsonl"));
        session
            .save_to_path(session.persistence_path().unwrap())
            .unwrap();
        Self { dir, session }
    }
    fn open(&self, create: bool) -> Arc<CompletionJournal> {
        CompletionJournal::open(&self.session, &self.dir, create).unwrap()
    }
    fn seed_source(journal: &CompletionJournal) {
        journal.begin("run:1", None).unwrap();
        journal
            .register(lifecycle(SubagentPhase::Started), "child.json".into())
            .unwrap();
        journal
            .tool_result(None, "spawn-1", "agent_spawn", r#"{"status":"running"}"#)
            .unwrap();
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn frozen_delivery_replays_identically_until_ack_is_committed() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    journal
        .lifecycle(&lifecycle(SubagentPhase::Finished))
        .unwrap();
    assert_eq!(
        journal.pending(),
        [] as [runtime::subagent_completion::Completion; 0]
    );
    journal.commit("run:1").unwrap();
    let payload = journal.pending();
    assert_eq!(payload.len(), 1);
    drop(journal); // crash before receiving/storing the controller ACK
    let journal = store.open(false);
    assert_eq!(journal.pending(), payload);
    journal
        .tool_result(
            None,
            "read-1",
            "pid_output",
            r#"{"status":"completed","agentId":"agent-1"}"#,
        )
        .unwrap();
    assert_eq!(journal.pending(), payload); // publication is the cutoff
    journal
        .acknowledge(&payload[0].completion_id, "stored")
        .unwrap();
    drop(journal);
    let journal = store.open(false);
    assert_eq!(
        journal.pending(),
        [] as [runtime::subagent_completion::Completion; 0]
    );
    assert_eq!(journal.status().settled, 1);
}

#[test]
fn late_earlier_spawn_cannot_overtake_unacknowledged_completion() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    let mut second = lifecycle(SubagentPhase::Started);
    second.tool_call_id = "spawn-2".into();
    second.agent.agent_id = "agent-2".into();
    journal
        .register(second.clone(), "child-2.json".into())
        .unwrap();
    journal
        .tool_result(None, "spawn-2", "agent_spawn", r#"{"status":"running"}"#)
        .unwrap();
    journal.commit("run:1").unwrap();
    second.phase = SubagentPhase::Finished;
    second.status = Some("completed".into());
    second.completion_notification = Some("second child finished first".into());
    journal.lifecycle(&second).unwrap();
    let first_delivery = journal.pending()[0].clone();
    assert_eq!(first_delivery.agent_id, "agent-2");
    journal
        .lifecycle(&lifecycle(SubagentPhase::Finished))
        .unwrap();
    assert_eq!(journal.pending()[0], first_delivery);
    drop(journal);
    let journal = store.open(false);
    assert_eq!(journal.pending()[0], first_delivery);
    journal
        .acknowledge(&first_delivery.completion_id, "stored")
        .unwrap();
    assert_eq!(journal.pending().len(), 1);
    assert_eq!(journal.pending()[0].agent_id, "agent-1");
}

#[test]
fn terminal_manifest_recovers_a_lost_finished_event_without_an_engine() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    let context = journal.commit("run:1").unwrap();
    drop(journal);
    let journal = store.open(false);
    let status = journal
        .reconcile(|path, source| {
            assert_eq!(path, "child.json");
            assert_eq!(source.agent.agent_id, "agent-1");
            Ok(Some(lifecycle(SubagentPhase::Finished)))
        })
        .unwrap();
    assert_eq!(status.pending, 1);
    assert_eq!(journal.context().unwrap(), Some(context));
}

#[test]
fn crash_before_parent_commit_is_blocked_even_with_terminal_manifest() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    drop(journal);
    let journal = store.open(false);
    let status = journal
        .reconcile(|_, _| panic!("unconfirmed parent must not publish child result"))
        .unwrap();
    assert_eq!(status.blocked, vec!["source_context_unconfirmed"]);
    assert_eq!(status.pending, 0);
    assert_eq!(journal.context().unwrap(), None);
    journal.begin("human:2", None).unwrap();
    journal.commit("human:2").unwrap();
    assert_eq!(
        journal.pending(),
        [] as [runtime::subagent_completion::Completion; 0]
    );
}

#[test]
fn missing_terminal_is_unknown_and_does_not_restart_work() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    journal.commit("run:1").unwrap();
    let status = journal.reconcile(|_, _| Ok(None)).unwrap();
    assert_eq!(status.blocked, vec!["execution_unconfirmed"]);
    assert_eq!(
        journal.pending(),
        [] as [runtime::subagent_completion::Completion; 0]
    );
    journal
        .lifecycle(&lifecycle(SubagentPhase::Finished))
        .unwrap();
    assert_eq!(journal.pending().len(), 1); // new actual evidence may unblock
}

#[test]
fn expected_context_is_checked_before_next_input_and_survives_reopen() {
    let store = Store::new();
    let journal = store.open(true);
    journal.begin("run:1", None).unwrap();
    let context = journal.commit("run:1").unwrap();
    drop(journal);
    let journal = store.open(false);
    let mut wrong = context.clone();
    wrong.revision += 1;
    assert!(journal.begin("run:2", Some(&wrong)).is_err());
    assert_eq!(journal.context().unwrap(), Some(context.clone()));
    journal.begin("run:2", Some(&context)).unwrap();
    let next = journal.commit("run:2").unwrap();
    assert_eq!(next.revision, context.revision + 1);
    assert_eq!(next.last_trace_id, "run:2");
    assert!(journal.begin("run:3", Some(&context)).is_err());
}

#[test]
fn transcript_change_and_wrong_journal_identity_are_rejected() {
    let store = Store::new();
    assert!(CompletionJournal::open(&store.session, &store.dir, false).is_err());
    let journal = store.open(true);
    journal.begin("run:1", None).unwrap();
    let context = journal.commit("run:1").unwrap();
    assert!(CompletionJournal::open(&store.session, &store.dir.join("different"), false).is_err());
    let path = store.session.persistence_path().unwrap();
    let bytes = std::fs::read_to_string(path).unwrap();
    let mut records: Vec<serde_json::Value> = bytes
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for record in &mut records {
        if record["type"] == "history_checkpoint" {
            record["meta"]["model"] = "externally-changed".into();
        } else if record["type"] == "session_meta" {
            record["model"] = "externally-changed".into();
        }
    }
    let changed = records
        .iter()
        .map(|record| serde_json::to_string(record).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(changed.trim(), bytes.trim());
    std::fs::write(path, changed + "\n").unwrap();
    assert!(journal.context().is_err());
    assert!(journal.begin("run:2", Some(&context)).is_err());
}

#[test]
fn failed_ack_write_does_not_remove_pending_delivery() {
    let store = Store::new();
    let journal = store.open(true);
    Store::seed_source(&journal);
    journal
        .lifecycle(&lifecycle(SubagentPhase::Finished))
        .unwrap();
    journal.commit("run:1").unwrap();
    let before = journal.pending();
    let path = store
        .session
        .persistence_path()
        .unwrap()
        .with_extension("completions.json");
    let backup = path.with_extension("saved");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap(); // atomic replacement fails
    assert!(journal
        .acknowledge(&before[0].completion_id, "stored")
        .is_err());
    assert_eq!(journal.pending(), before);
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(backup, &path).unwrap();
    drop(journal);
    assert_eq!(store.open(false).pending(), before);
}
