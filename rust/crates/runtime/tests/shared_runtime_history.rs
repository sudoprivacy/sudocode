//! Actual runtime turns, a native child tool, concurrent resume and cold replay.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use runtime::fs_backend::{FsBackend, StdFsBackend};
use runtime::history::HistoryGraph;
use runtime::workspace_root::WorkspaceRootScope;
use runtime::{
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, ConversationMessage,
    ConversationRuntime, PermissionMode, PermissionPolicy, RuntimeError, Session,
    StaticToolExecutor, SystemPrompt, ToolError,
};

const CASE: &str = "runtime_resume_forks_completed_turn_while_native_tool_runs";
const CHILD: &str = "SCODE_SHARED_RUNTIME_CHILD";
const RECEIPT: &str = "NEXUS_RUNTIME_HISTORY_RECEIPT";
const WAIT: Duration = Duration::from_secs(30);
type Requests = Arc<Mutex<Vec<Vec<ConversationMessage>>>>;

#[path = "common/live_nexus.rs"]
mod live_nexus;

fn backend(nexus: bool) -> Arc<dyn FsBackend> {
    if !nexus {
        return Arc::new(StdFsBackend);
    }
    Arc::new(live_nexus::backend())
}

fn load(path: &Path, nexus: bool) -> Session {
    let fs = backend(nexus);
    Session::load_from_path_with(fs.as_ref(), path)
        .unwrap()
        .with_fs_backend(fs)
}

struct ScriptedClient {
    responses: VecDeque<Vec<AssistantEvent>>,
    requests: Requests,
}

#[async_trait]
impl ApiClient for ScriptedClient {
    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        self.requests.lock().unwrap().push(request.messages);
        let events = self
            .responses
            .pop_front()
            .expect("unexpected model request");
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}

fn answer(text: &str) -> Vec<AssistantEvent> {
    vec![
        AssistantEvent::TextDelta(text.into()),
        AssistantEvent::MessageStop,
    ]
}

fn runtime(
    mut session: Session,
    responses: Vec<Vec<AssistantEvent>>,
    tools: StaticToolExecutor,
) -> (
    ConversationRuntime<ScriptedClient, StaticToolExecutor>,
    Requests,
) {
    session.activate_persistence().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let client = ScriptedClient {
        responses: responses.into(),
        requests: Arc::clone(&requests),
    };
    (
        ConversationRuntime::new(
            session,
            client,
            tools,
            PermissionPolicy::new(PermissionMode::Allow),
            SystemPrompt::default(),
        ),
        requests,
    )
}

fn transcript(messages: &[ConversationMessage]) -> String {
    messages
        .iter()
        .map(|message| message.to_json().render())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + WAIT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timeout waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Retained runtime history evidence: {}", self.0.display());
        } else {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

struct RunningTurn {
    root: PathBuf,
    thread: Option<thread::JoinHandle<Session>>,
}

#[test]
fn legacy_transcript_retains_exclusive_writer_admission() {
    let root = std::env::temp_dir().join(format!("scode-shared-runtime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture(root.clone());
    let path = root.join("legacy.jsonl");
    let seed = Session::new().with_workspace_root(&root);
    std::fs::write(&path, seed.to_json().unwrap().render()).unwrap();
    let mut first = Session::load_from_path(&path).unwrap();
    first.activate_persistence().unwrap();
    assert!(!first.supports_concurrent_resume().unwrap());
    let before = std::fs::read(&path).unwrap();
    let mut second = Session::load_from_path(&path).unwrap();
    assert!(second
        .activate_persistence()
        .unwrap_err()
        .to_string()
        .contains("session_writer_busy"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    first.close_persistence();
    second.activate_persistence().unwrap();
    assert!(!second.supports_concurrent_resume().unwrap());
    second.close_persistence();
    drop(fixture);
}
impl RunningTurn {
    fn finish(mut self) -> Session {
        std::fs::write(self.root.join("release"), b"release").unwrap();
        self.thread.take().unwrap().join().unwrap()
    }
}
impl Drop for RunningTurn {
    fn drop(&mut self) {
        let _ = std::fs::write(self.root.join("release"), b"release");
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn runtime_resume_forks_completed_turn_while_native_tool_runs() {
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        std::fs::write(root.join("tool-pid"), std::process::id().to_string()).unwrap();
        wait_for(&root.join("release"));
        std::fs::write(root.join("actual-result"), "A_ACTUAL_NATIVE_TOOL_RESULT").unwrap();
        return;
    }
    exercise_runtime_history(false);
}

#[test]
#[ignore = "requires the real pinned Nexus daemon and mTLS credentials"]
fn live_runtime_resume_preserves_completed_fork_and_native_tool_result() {
    exercise_runtime_history(true);
}

fn exercise_runtime_history(nexus: bool) {
    std::env::set_var("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1");
    let root = std::env::temp_dir().join(format!("scode-shared-runtime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture(root.clone());
    let _workspace = WorkspaceRootScope::enter(&root);
    let path = if nexus {
        PathBuf::from(format!(
            "/agents/live-probe/runtime-{}/session.jsonl",
            uuid::Uuid::new_v4()
        ))
    } else {
        root.join("session.jsonl")
    };
    let session = Session::new()
        .with_workspace_root(&root)
        .with_persistence_path(&path)
        .with_fs_backend(backend(nexus));
    let sid = session.session_id.clone();
    session.initialize_persistence().unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut warm, _) = runtime(
        session,
        vec![answer("COMMON_COMPLETED_ANSWER")],
        StaticToolExecutor::new(),
    );
    executor
        .block_on(warm.run_turn("COMMON_COMPLETED_PROMPT", None, None))
        .unwrap();
    drop(warm);

    // Capture B before A starts, then activate it after A's tool is already live.
    let stale_b = load(&path, nexus);
    let thread_root = root.clone();
    let thread_path = path.clone();
    let thread = thread::spawn(move || {
        let _workspace = WorkspaceRootScope::enter(&thread_root);
        let child_root = thread_root.clone();
        let tools = StaticToolExecutor::new().register("bash", move |_| {
            let status = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", CASE, "--nocapture"])
                .env(CHILD, &child_root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|error| ToolError::new(error.to_string()))?
                .wait()
                .map_err(|error| ToolError::new(error.to_string()))?;
            if !status.success() {
                return Err(ToolError::new("native tool child failed"));
            }
            std::fs::read_to_string(child_root.join("actual-result"))
                .map_err(|error| ToolError::new(error.to_string()))
        });
        let events = vec![
            AssistantEvent::ToolUse {
                id: "provider-reused-call-id".into(),
                name: "bash".into(),
                input: "{}".into(),
                thought_signature: None,
            },
            AssistantEvent::MessageStop,
        ];
        let (mut a, requests) = runtime(
            load(&thread_path, nexus),
            vec![events, answer("A_COMPLETED_AFTER_REAL_RESULT")],
            tools,
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor
            .block_on(a.run_turn("A_LIVE_TOOL_PROMPT", None, None))
            .unwrap();
        let requests = requests.lock().unwrap();
        assert!(transcript(requests.last().unwrap()).contains("A_ACTUAL_NATIVE_TOOL_RESULT"));
        assert!(!transcript(requests.last().unwrap()).contains("B_PRIVATE_ANSWER"));
        a.session().clone()
    });
    let running = RunningTurn {
        root: root.clone(),
        thread: Some(thread),
    };
    wait_for(&root.join("tool-pid"));
    assert!(!root.join("actual-result").exists());
    let loaded = load(&path, nexus);
    assert!(!transcript(loaded.messages()).contains("A_LIVE_TOOL_PROMPT"));
    let (mut b, requests) = runtime(
        stale_b,
        vec![answer("B_PRIVATE_ANSWER")],
        StaticToolExecutor::new(),
    );
    executor
        .block_on(b.run_turn("B_PRIVATE_PROMPT", None, None))
        .unwrap();
    assert!(!transcript(&requests.lock().unwrap()[0]).contains("A_LIVE_TOOL_PROMPT"));
    assert!(
        !root.join("actual-result").exists(),
        "B must finish while A is still running"
    );
    assert_eq!(b.session().session_id, sid);

    let mut compacted =
        b.session()
            .clone()
            .with_messages(vec![b.session().messages().last().unwrap().clone()]);
    compacted.record_compaction("B_COMPACTED_VIEW", 3);
    compacted.rewrite_persisted().unwrap();
    let b_view = load(&path, nexus);
    assert_eq!(b_view.messages().len(), 1);
    assert!(transcript(b_view.messages()).contains("B_PRIVATE_ANSWER"));
    drop(compacted);
    drop(b);

    let a = running.finish();
    assert_eq!(a.session_id, sid);
    let restored = load(&path, nexus);
    let text = transcript(restored.messages());
    assert!(text.contains("A_ACTUAL_NATIVE_TOOL_RESULT"));
    assert!(!text.contains("B_PRIVATE_ANSWER"));
    let contents = backend(nexus).read(&path.to_string_lossy()).unwrap();
    let graph = HistoryGraph::decode(&contents).unwrap();
    let records: Vec<serde_json::Value> = String::from_utf8(contents)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .filter(|record| record["event"]["type"] == "commit_turn")
            .count()
            >= 3
    );
    assert!(records
        .iter()
        .filter(|record| record["event"]["type"] == "projection")
        .any(|record| {
            let id = record["id"].as_str().unwrap();
            graph.completed_session(Some(id)).is_ok_and(|session| {
                session.messages().len() == 1
                    && transcript(session.messages()).contains("B_PRIVATE_ANSWER")
            })
        }));
    if nexus {
        let b_commit = records
            .iter()
            .find(|record| record["event"]["type"] == "projection")
            .unwrap()["id"]
            .clone();
        let receipt = serde_json::json!({"path":path, "sid":sid, "b_commit":b_commit,
            "a_messages":transcript(restored.messages()), "result":"PASS"});
        std::fs::write(
            std::env::var(RECEIPT).expect("runtime receipt path"),
            receipt.to_string(),
        )
        .unwrap();
    }
    drop(a);
    drop(executor);
    drop(fixture);
}

#[test]
#[ignore = "requires the same real Nexus daemon restarted with its durable data"]
fn live_runtime_history_restores_both_branches_after_daemon_restart() {
    let path = PathBuf::from(std::env::var(RECEIPT).expect("runtime receipt path"));
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let transcript_path = Path::new(receipt["path"].as_str().unwrap());
    let restored = load(transcript_path, true);
    assert_eq!(restored.session_id, receipt["sid"].as_str().unwrap());
    assert_eq!(
        transcript(restored.messages()),
        receipt["a_messages"].as_str().unwrap()
    );
    let bytes = backend(true)
        .read(&transcript_path.to_string_lossy())
        .unwrap();
    let graph = HistoryGraph::decode(&bytes).unwrap();
    let b = graph
        .completed_session(receipt["b_commit"].as_str())
        .unwrap();
    assert_eq!(b.messages().len(), 1);
    assert!(transcript(b.messages()).contains("B_PRIVATE_ANSWER"));
    assert!(!transcript(b.messages()).contains("A_ACTUAL_NATIVE_TOOL_RESULT"));
    let proof = serde_json::json!({"result":"PASS", "retained_runtime_branches":2,
        "sid":receipt["sid"], "real_tool_result":true, "b_compacted_view":true});
    std::fs::write(path.with_extension("after-restart.json"), proof.to_string()).unwrap();
}
