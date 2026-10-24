//! Storage integration before the hosts admit concurrent shared-SID runtimes.
//! Two real writer processes and a live child tool use the production record log.
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use command_group::{CommandGroup, GroupChild};
use runtime::fs_backend::{FsBackend, RecordLog, RecordLogMode, StdFsBackend};
use runtime::history::{HistoryEvent, HistoryLog, HistoryRecord, RecordOutcome};
use runtime::{ContentBlock, ConversationMessage, MessageRole, Session};
use serde_json::{json, Value};

fn with_selected_messages(session: Session, messages: Vec<ConversationMessage>) -> Session {
    session.with_messages(messages)
}

fn payloads(messages: &[ConversationMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut value: Value = serde_json::from_str(&message.to_json().render()).unwrap();
            value.as_object_mut().unwrap().remove("history");
            value
        })
        .collect()
}

const ROOT: &str = "SCODE_SHARED_HISTORY_FIXTURE";
const MODE: &str = "SCODE_SHARED_HISTORY_WORKER";
const WAIT: Duration = Duration::from_secs(20);

#[path = "shared_history_records/nexus.rs"]
mod nexus;

#[path = "common/live_nexus.rs"]
mod live_nexus;

#[cfg(target_os = "linux")]
#[path = "shared_history_records/flush.rs"]
mod flush;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("scode-shared-records-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn log(&self, mode: RecordLogMode) -> Arc<dyn RecordLog> {
        StdFsBackend
            .record_log(&self.0.join("history.jsonl").to_string_lossy(), mode)
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "Retained shared history failure evidence: {}",
                self.0.display()
            );
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Worker(GroupChild);
impl Worker {
    fn spawn(root: &Path, mode: &str) -> Self {
        let log = File::create(root.join(format!("{mode}.log"))).unwrap();
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "completed_forks_survive_live_tools_compaction_and_restart",
                    "--nocapture",
                ])
                .env(ROOT, root)
                .env(MODE, mode)
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .group_spawn()
                .unwrap(),
        )
    }
    fn finish(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "writer failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "writer did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct ToolChild(Child);
impl ToolChild {
    fn spawn(root: &Path) -> Self {
        // Inherit the writer's process group/job; keep the native child handle.
        Self(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "completed_forks_survive_live_tools_compaction_and_restart",
                    "--nocapture",
                ])
                .env(ROOT, root)
                .env(MODE, "tool")
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}
impl Drop for ToolChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn assistant(blocks: Vec<ContentBlock>) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        blocks,
        usage: None,
        model: None,
        duration_ms: None,
        history: None,
    }
}
fn text(text: &str) -> ConversationMessage {
    assistant(vec![ContentBlock::Text { text: text.into() }])
}
fn commit(log: &mut HistoryLog, record: &HistoryRecord) {
    assert_eq!(log.commit(record, WAIT).unwrap(), RecordOutcome::Applied);
}
fn append(log: &mut HistoryLog, context: &str, message: &ConversationMessage) -> String {
    let record = log.graph().prepare_message(context, message).unwrap();
    let id = record.message_id().unwrap().to_owned();
    commit(log, &record);
    id
}
fn finish_turn(log: &mut HistoryLog, context: &str, trace: &str) -> String {
    let record = HistoryRecord::new(
        context.into(),
        log.graph().revision(context).map(str::to_owned),
        HistoryEvent::CommitTurn {
            trace_id: trace.into(),
        },
    );
    commit(log, &record);
    record.id().to_owned()
}
fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture stage missing: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_worker(root: &Path, mode: &str) {
    if mode == "tool" {
        std::fs::write(root.join("tool-started"), std::process::id().to_string()).unwrap();
        wait_for(&root.join("tool-release"));
        std::fs::write(root.join("actual-tool-result"), b"REAL_TOOL_A_RESULT").unwrap();
        return;
    }
    assert_eq!(mode, "writer-a");
    let raw = StdFsBackend
        .record_log(
            &root.join("history.jsonl").to_string_lossy(),
            RecordLogMode::Existing,
        )
        .unwrap();
    let mut log = HistoryLog::new(raw);
    let branch = HistoryRecord::branch_latest();
    commit(&mut log, &branch);
    let context = branch.context_id();
    append(
        &mut log,
        context,
        &ConversationMessage::user_text("A_PRIVATE_TURN"),
    );
    let call = assistant(vec![ContentBlock::ToolUse {
        id: "call-a".into(),
        name: "fixture_tool".into(),
        input: "{}".into(),
        thought_signature: None,
    }]);
    let assistant_id = append(&mut log, context, &call);
    let mut tool = ToolChild::spawn(root);
    assert!(tool.0.wait().unwrap().success());
    let result = std::fs::read_to_string(root.join("actual-tool-result")).unwrap();
    let result_id = append(
        &mut log,
        context,
        &ConversationMessage::tool_result("call-a", "fixture_tool", result, false),
    );
    append(&mut log, context, &text("A_FINISHED"));
    let completed = finish_turn(&mut log, context, "trace-a");
    std::fs::write(root.join("a-receipt.json"), serde_json::to_vec(&json!({"context":context, "assistant":assistant_id, "result":result_id, "commit":completed})).unwrap()).unwrap();
}

#[test]
fn completed_forks_survive_live_tools_compaction_and_restart() {
    if let Ok(root) = std::env::var(ROOT) {
        run_worker(Path::new(&root), &std::env::var(MODE).unwrap());
        return;
    }
    let fixture = Fixture::new();
    let mut prefix = Session::new();
    prefix.push_user_text("COMMON_PREFIX").unwrap();
    prefix.push_message(text("COMMON_ANSWER")).unwrap();
    let mut initial = HistoryLog::new(fixture.log(RecordLogMode::CreateNew));
    let seed = HistoryRecord::initialize(&prefix).unwrap();
    commit(&mut initial, &seed);
    let prefix_ids = initial.graph().message_ids(seed.context_id()).unwrap();
    assert_eq!(
        prefix_ids,
        prefix
            .messages()
            .iter()
            .map(|message| { message.history.as_ref().expect("seed identity").id.clone() })
            .collect::<Vec<_>>()
    );
    let mut writer = Worker::spawn(&fixture.0, "writer-a");
    wait_for(&fixture.0.join("tool-started"));
    assert!(!fixture.0.join("actual-tool-result").exists());

    let mut b = HistoryLog::new(fixture.log(RecordLogMode::Existing));
    let branch = HistoryRecord::branch_latest();
    commit(&mut b, &branch);
    let context = branch.context_id();
    assert_eq!(
        payloads(b.graph().context_session(context).unwrap().messages()),
        payloads(prefix.messages())
    );
    assert_eq!(b.graph().message_ids(context).unwrap(), prefix_ids);
    append(
        &mut b,
        context,
        &ConversationMessage::user_text("B_PRIVATE_TURN"),
    );
    let discarded_answer = append(&mut b, context, &text("B_FINISHED"));
    let b_answer = append(&mut b, context, &text("B_FINISHED"));
    finish_turn(&mut b, context, "trace-b");
    let mut compacted = b.graph().context_session(context).unwrap();
    compacted = with_selected_messages(
        compacted,
        vec![
            ConversationMessage::user_text("B_COMPACTED"),
            text("B_FINISHED"),
        ],
    );
    compacted.record_compaction("B summary", 3);
    let candidate = b
        .graph()
        .prepare_projection(
            context,
            b.graph().revision(context).unwrap(),
            &compacted,
            &[None, Some(&b_answer)],
        )
        .unwrap();
    commit(&mut b, &candidate);
    assert_eq!(
        b.graph().message_ids(context).unwrap().last(),
        Some(&b_answer)
    );
    assert!(!b
        .graph()
        .message_ids(context)
        .unwrap()
        .contains(&discarded_answer));
    let b_commit = candidate.id().to_owned();
    let mut cold = HistoryLog::new(fixture.log(RecordLogMode::Existing));
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
        "a new runtime must see the compacted completed view while A stays in flight",
    );
    assert!(
        !fixture.0.join("actual-tool-result").exists(),
        "A's real tool must remain in flight during B's compaction"
    );

    std::fs::write(fixture.0.join("tool-release"), b"continue").unwrap();
    writer.finish();
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("a-receipt.json")).unwrap()).unwrap();
    let mut metadata: Value = serde_json::from_str(
        &b.graph()
            .context_session(context)
            .unwrap()
            .to_json()
            .unwrap()
            .render(),
    )
    .unwrap();
    metadata.as_object_mut().unwrap().remove("messages");
    metadata["model"] = Value::String("B_IDLE_METADATA".into());
    let idle_save = HistoryRecord::new(
        context.into(),
        Some(b.graph().revision(context).unwrap().into()),
        HistoryEvent::Metadata { metadata },
    );
    commit(&mut b, &idle_save);
    drop(initial);
    drop(b);
    let mut restored = HistoryLog::new(fixture.log(RecordLogMode::Existing));
    restored.refresh().unwrap();
    let graph = restored.graph();
    assert_eq!(
        payloads(graph.completed_session(Some(&b_commit)).unwrap().messages()),
        payloads(compacted.messages())
    );
    let a = graph.completed_session(None).unwrap();
    assert_ne!(a.model.as_deref(), Some("B_IDLE_METADATA"));
    assert_eq!(a.session_id, prefix.session_id);
    assert_eq!(payloads(&a.messages()[..2]), payloads(prefix.messages()));
    let projected_ids: Vec<_> = a
        .messages()
        .iter()
        .map(|message| {
            message
                .history
                .as_ref()
                .expect("restored message identity")
                .id
                .clone()
        })
        .collect();
    assert_eq!(
        projected_ids,
        graph
            .message_ids(receipt["context"].as_str().unwrap())
            .unwrap()
    );
    let result_identity = a
        .messages()
        .iter()
        .filter_map(|message| message.history.as_ref())
        .find(|identity| identity.id == receipt["result"].as_str().unwrap())
        .expect("restored tool result identity");
    assert_eq!(
        result_identity
            .tool_origins
            .get("call-a")
            .map(String::as_str),
        receipt["assistant"].as_str()
    );
    assert!(a.to_json().unwrap().render().contains("REAL_TOOL_A_RESULT"));
    assert!(!a.to_json().unwrap().render().contains("B_PRIVATE_TURN"));
    assert_eq!(
        graph.tool_origin(receipt["result"].as_str().unwrap(), "call-a"),
        receipt["assistant"].as_str()
    );
    assert_eq!(graph.latest_commit(), receipt["commit"].as_str());

    let stale = HistoryRecord::new(
        receipt["context"].as_str().unwrap().into(),
        Some(receipt["commit"].as_str().unwrap().into()),
        HistoryEvent::AssistantUpdate {
            message_id: receipt["assistant"].as_str().unwrap().into(),
            message: serde_json::from_str(&text("OVERWRITE_COMMITTED").to_json().render()).unwrap(),
        },
    );
    assert!(matches!(
        restored.commit(&stale, WAIT).unwrap(),
        RecordOutcome::Rejected(_)
    ));
    assert_eq!(
        payloads(restored.graph().completed_session(None).unwrap().messages()),
        payloads(a.messages())
    );
}

struct AmbiguousAppend {
    inner: Arc<dyn RecordLog>,
    fail_once: AtomicBool,
}
impl RecordLog for AmbiguousAppend {
    fn append(&self, record: &[u8]) -> io::Result<u64> {
        let after = self.inner.append(record)?;
        if self.fail_once.swap(false, Ordering::SeqCst) {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture lost append response after durable commit",
            ))
        } else {
            Ok(after)
        }
    }
    fn read(&self, cursor: u64) -> io::Result<(Vec<u8>, u64)> {
        self.inner.read(cursor)
    }
}

#[test]
fn uncertain_native_commit_retries_preserve_one_record_identity() {
    let fixture = Fixture::new();
    let raw = fixture.log(RecordLogMode::CreateNew);
    let mut log = HistoryLog::new(Arc::new(AmbiguousAppend {
        inner: raw,
        fail_once: AtomicBool::new(true),
    }));
    let seed = HistoryRecord::initialize(&Session::new()).unwrap();
    commit(&mut log, &seed);
    let size = std::fs::metadata(fixture.0.join("history.jsonl"))
        .unwrap()
        .len();
    commit(&mut log, &seed);
    assert_eq!(
        std::fs::metadata(fixture.0.join("history.jsonl"))
            .unwrap()
            .len(),
        size
    );
    let mut changed = serde_json::to_value(&seed).unwrap();
    changed["timestamp_ms"] = Value::from(0);
    let changed: HistoryRecord = serde_json::from_value(changed).unwrap();
    assert!(log.commit(&changed, WAIT).is_err());
    assert_eq!(
        std::fs::metadata(fixture.0.join("history.jsonl"))
            .unwrap()
            .len(),
        size
    );
}

#[test]
fn invalid_tool_association_is_rejected_without_poisoning_shared_history() {
    let fixture = Fixture::new();
    let mut log = HistoryLog::new(fixture.log(RecordLogMode::CreateNew));
    let seed = HistoryRecord::initialize(&Session::new()).unwrap();
    commit(&mut log, &seed);
    let branch = HistoryRecord::branch_latest();
    commit(&mut log, &branch);
    let context = branch.context_id();
    let orphan = log
        .graph()
        .prepare_message(
            context,
            &ConversationMessage::tool_result(
                "sibling-call",
                "fixture_tool",
                "must not enter B",
                false,
            ),
        )
        .unwrap();
    assert_eq!(
        log.commit(&orphan, WAIT).unwrap(),
        RecordOutcome::Rejected("tool_association_invalid".into())
    );
    append(
        &mut log,
        context,
        &ConversationMessage::user_text("GOOD_AFTER_REJECTION"),
    );
    append(&mut log, context, &text("GOOD_ANSWER"));
    finish_turn(&mut log, context, "good-trace");
    drop(log);
    let mut restored = HistoryLog::new(fixture.log(RecordLogMode::Existing));
    restored.refresh().unwrap();
    let session = restored.graph().completed_session(None).unwrap();
    assert_eq!(session.messages().len(), 2);
    assert!(!session
        .to_json()
        .unwrap()
        .render()
        .contains("must not enter B"));
}

/// Expose an older committed native prefix, then delay visibility of our own
/// successful append. Byte cursors here emulate lag, not Nexus frame cursors.
struct LaggingReader {
    inner: Arc<dyn RecordLog>,
    visible: AtomicU64,
    pending_reads: AtomicUsize,
}
impl RecordLog for LaggingReader {
    fn append(&self, record: &[u8]) -> io::Result<u64> {
        let after = self.inner.append(record)?;
        self.pending_reads.store(3, Ordering::SeqCst);
        Ok(after)
    }
    fn read(&self, cursor: u64) -> io::Result<(Vec<u8>, u64)> {
        if self
            .pending_reads
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            == Ok(1)
        {
            self.visible.store(u64::MAX, Ordering::SeqCst);
        }
        let end = self.visible.load(Ordering::SeqCst);
        if cursor >= end {
            return Ok((Vec::new(), cursor));
        }
        let (mut bytes, next) = self.inner.read(cursor)?;
        let next = next.min(end);
        bytes.truncate(usize::try_from(next - cursor).unwrap());
        Ok((bytes, next))
    }
}

#[test]
fn stale_reader_opens_latest_completed_turn_at_its_own_commit_position() {
    let fixture = Fixture::new();
    let raw = fixture.log(RecordLogMode::CreateNew);
    let mut a = HistoryLog::new(Arc::clone(&raw));
    commit(&mut a, &HistoryRecord::initialize(&Session::new()).unwrap());
    let prefix = std::fs::metadata(fixture.0.join("history.jsonl"))
        .unwrap()
        .len();
    let mut b = HistoryLog::new(Arc::new(LaggingReader {
        inner: fixture.log(RecordLogMode::Existing),
        visible: AtomicU64::new(prefix),
        pending_reads: AtomicUsize::new(0),
    }));
    b.refresh().unwrap();
    let branch_a = HistoryRecord::branch_latest();
    commit(&mut a, &branch_a);
    append(&mut a, branch_a.context_id(), &text("NEW_COMPLETED_PREFIX"));
    let a_commit = finish_turn(&mut a, branch_a.context_id(), "new-completed-turn");
    assert_ne!(b.graph().latest_commit(), Some(a_commit.as_str()));
    let branch_b = HistoryRecord::branch_latest();
    commit(&mut b, &branch_b);
    assert_eq!(
        b.graph()
            .context_session(branch_b.context_id())
            .unwrap()
            .messages(),
        a.graph()
            .completed_session(Some(&a_commit))
            .unwrap()
            .messages()
    );
    assert_eq!(
        b.graph().message_ids(branch_b.context_id()).unwrap(),
        a.graph().message_ids(branch_a.context_id()).unwrap()
    );
}
