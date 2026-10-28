//! Actual runtime turns, a native child tool, concurrent resume and cold replay.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
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

const RECEIPT: &str = "NEXUS_RUNTIME_HISTORY_RECEIPT";
const WAIT: Duration = Duration::from_secs(30);
const PREFIX_TURNS: usize = 36;
type Requests = Arc<Mutex<Vec<Vec<ConversationMessage>>>>;

#[path = "common/live_nexus.rs"]
mod live_nexus;

#[path = "common/native_tool.rs"]
mod native_tool;

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

const DATA_DIR_CASE: &str = "explicit_data_dir_reclaims_external_session_without_workspace_writes";
const DATA_DIR_CHILD: &str = "SCODE_DATA_DIR_CLEANUP_CHILD";
const DATA_DIR_ROOT: &str = "SCODE_DATA_DIR_CLEANUP_ROOT";

struct CleanupClient(std::process::Child);

impl Drop for CleanupClient {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn cleanup_client(root: &Path, role: &str) -> CleanupClient {
    CleanupClient(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", DATA_DIR_CASE, "--nocapture", "--test-threads=1"])
            .env(DATA_DIR_CHILD, role)
            .env(DATA_DIR_ROOT, root)
            .spawn()
            .unwrap(),
    )
}

#[test]
fn explicit_data_dir_reclaims_external_session_without_workspace_writes() {
    std::env::set_var("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1");
    if let Ok(role) = std::env::var(DATA_DIR_CHILD) {
        data_dir_client(
            &PathBuf::from(std::env::var_os(DATA_DIR_ROOT).unwrap()),
            &role,
        );
        return;
    }
    for deletion in ["delete-journal", "delete-store"] {
        let root = std::env::temp_dir().join(format!("scode-data-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir(root.join("external")).unwrap();
        let _fixture = Fixture(root.clone());
        let workspace = root.join("workspace");
        let path = root.join("external/transcript.jsonl");
        let seed = Session::new().with_workspace_root(&workspace);
        std::fs::write(&path, seed.to_json().unwrap().render()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let mut owner = cleanup_client(&root, "owner");
        wait_for(&root.join("ready"));
        let mut deleter = cleanup_client(&root, deletion);
        assert!(deleter.0.wait().unwrap().success());
        assert!(root.join("deleted").exists());
        assert!(path.exists(), "a live writer still owns the transcript");
        assert!(!workspace.join(".scode").exists());
        let references: Vec<_> = std::fs::read_dir(root.join("data/sessions"))
            .unwrap()
            .map(|entry| entry.unwrap().path().join(".cleanup"))
            .collect();
        assert_eq!(references.len(), 1);
        assert_eq!(std::fs::read_dir(&references[0]).unwrap().count(), 1);
        // Abrupt process death releases its native locks. No process remains
        // that could run the deleter's in-memory scheduler.
        owner.0.kill().unwrap();
        owner.0.wait().unwrap();
        assert!(path.exists());
        let mut restarted = cleanup_client(&root, "restart");
        assert!(restarted.0.wait().unwrap().success());
        assert!(!path.exists());
        assert!(path.with_extension("completions.json").exists());
        assert_eq!(std::fs::read_dir(&references[0]).unwrap().count(), 0);
        assert!(!workspace.join(".scode").exists());
    }
}

fn data_dir_client(root: &Path, role: &str) {
    let workspace = root.join("workspace");
    #[cfg(unix)]
    assert!(
        std::fs::write(workspace.join("must-stay-read-only"), b"probe").is_err(),
        "the actual test process must respect workspace write permissions"
    );
    let store = runtime::SessionStore::from_data_dir(root.join("data"), &workspace).unwrap();
    let path = root.join("external/transcript.jsonl");
    if role == "restart" {
        let deadline = Instant::now() + WAIT;
        while path.exists()
            || std::fs::read_dir(store.sessions_dir().join(".cleanup"))
                .unwrap()
                .next()
                .is_some()
        {
            assert!(
                Instant::now() < deadline,
                "ordinary startup did not reclaim the external Session"
            );
            thread::sleep(Duration::from_millis(10));
        }
        return;
    }
    if role == "delete-store" {
        store.delete_session(&path).unwrap();
    } else {
        let mut session = store.load_session(path.to_str().unwrap()).unwrap().session;
        session.activate_persistence().unwrap();
        let journal =
            runtime::subagent_completion::CompletionJournal::open(&session, &workspace, true)
                .unwrap();
        if role == "owner" {
            std::fs::write(root.join("ready"), b"ready").unwrap();
            wait_for(&root.join("stop"));
            drop(journal);
            return;
        }
        assert_eq!(role, "delete-journal");
        journal
            .purge(&journal.storage_ref(), "data-dir-delete")
            .unwrap();
    }
    std::fs::write(root.join("deleted"), b"deleted").unwrap();
}

#[test]
fn legacy_migration_excludes_old_writers_preserves_archives_and_keeps_one_session() {
    std::env::set_var("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1");
    let root = std::env::temp_dir().join(format!("scode-shared-runtime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture(root.clone());
    let store = runtime::session_control::SessionStore::from_cwd(&root).unwrap();
    let seed = Session::new().with_workspace_root(&root);
    let path = store
        .sessions_dir()
        .join(format!("{}.jsonl", seed.session_id));
    std::fs::write(&path, seed.to_json().unwrap().render()).unwrap();
    let legacy_writer =
        runtime::session_writer::SessionWriter::acquire(Arc::new(StdFsBackend), &path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut second = Session::load_from_path(&path).unwrap();
    assert!(second
        .activate_persistence()
        .unwrap_err()
        .to_string()
        .contains("session_writer_busy"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(legacy_archives(store.sessions_dir()), Vec::<PathBuf>::new());
    drop(legacy_writer);
    second.activate_persistence().unwrap();
    assert!(second.supports_concurrent_resume().unwrap());
    let archives = legacy_archives(store.sessions_dir());
    assert_eq!(archives.len(), 1);
    assert_eq!(std::fs::read(&archives[0]).unwrap(), before);
    let listed = store.list_sessions().unwrap();
    assert_eq!(listed.len(), 1, "the archive is not another session");
    assert_eq!(listed[0].id, seed.session_id);
    second.close_persistence();

    std::fs::write(&path, &before).unwrap();
    let mut repeated = Session::load_from_path(&path).unwrap();
    repeated.activate_persistence().unwrap();
    repeated.close_persistence();
    assert_eq!(legacy_archives(store.sessions_dir()), archives);
    assert_eq!(std::fs::read(&archives[0]).unwrap(), before);

    assert_rejected_migrations_preserve_source(&path, &before, &archives, seed);
    drop(fixture);
}

fn assert_rejected_migrations_preserve_source(
    path: &Path,
    before: &[u8],
    archives: &[PathBuf],
    seed: Session,
) {
    let directory = path.parent().unwrap();
    std::fs::write(path, before).unwrap();
    std::fs::write(&archives[0], b"changed archive").unwrap();
    let mut retry = Session::load_from_path(path).unwrap();
    assert!(retry
        .activate_persistence()
        .unwrap_err()
        .to_string()
        .contains("legacy session archive contents changed"));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(std::fs::read(&archives[0]).unwrap(), b"changed archive");

    let mut invalid: serde_json::Value =
        serde_json::from_str(&seed.to_json().unwrap().render()).unwrap();
    let orphan = ConversationMessage::tool_result("missing-call", "read_file", "result", false);
    invalid["messages"] =
        serde_json::json!([
            serde_json::from_str::<serde_json::Value>(&orphan.to_json().render()).unwrap()
        ]);
    let bytes = invalid.to_string();
    std::fs::write(path, &bytes).unwrap();
    assert!(Session::load_from_path(path)
        .and_then(|mut session| session.activate_persistence())
        .is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
    assert_eq!(legacy_archives(directory).as_slice(), archives);

    let mut deleted = seed;
    deleted.push_user_text("DELETED_LEGACY_SOURCE").unwrap();
    let bytes = deleted.to_json().unwrap().render();
    std::fs::write(path, &bytes).unwrap();
    let cwd = deleted.workspace_root().unwrap();
    let binding = runtime::subagent_completion::CompletionJournal::probe(
        &StdFsBackend,
        path,
        &deleted.session_id,
        cwd,
    )
    .unwrap();
    runtime::subagent_completion::CompletionJournal::purge_stored(
        std::sync::Arc::new(StdFsBackend),
        path,
        &deleted.session_id,
        cwd,
        binding["storageRef"].as_str().unwrap(),
        "legacy-migration-deleted",
        None,
    )
    .unwrap();
    assert!(Session::load_from_path(path)
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    assert!(!path.exists());
    assert!(
        legacy_archives(directory).is_empty(),
        "owned migration backups retire with their Session"
    );
}

fn legacy_archives(directory: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".before-history-upgrade-")
        })
        .collect()
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
fn runtime_resume_inherits_completed_turn_while_native_tool_runs() {
    exercise_runtime_history(false, false);
}

#[test]
fn legacy_runtime_resume_inherits_completed_turn_while_native_tool_runs() {
    exercise_runtime_history(false, true);
}

#[test]
fn refreshing_a_live_context_preserves_its_writer_and_excludes_sibling_history() {
    std::env::set_var("SUDOCODE_EXPERIMENT_SHARED_SESSION_HISTORY", "1");
    let root = std::env::temp_dir().join(format!("scode-refresh-context-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture(root.clone());
    let path = root.join("history.jsonl");
    let store_root = root.join("configured-session-store");
    let seed = Session::new()
        .with_workspace_root(&root)
        .with_persistence_path(&path);
    seed.initialize_persistence().unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut a, requests) = runtime(
        load(&path, false).with_session_store_root(&store_root),
        vec![answer("A_BEFORE_REFRESH"), answer("A_AFTER_REFRESH")],
        StaticToolExecutor::new(),
    );
    executor
        .block_on(a.run_turn("A_INITIAL_PROMPT", None, None))
        .unwrap();
    // Keep the actual journal/snapshot writer alive across the source refresh.
    let snapshot = a.session().clone();
    let writer = snapshot.completion_writer().unwrap();
    let (mut b, _) = runtime(
        load(&path, false),
        vec![answer("B_PRIVATE_AFTER_A")],
        StaticToolExecutor::new(),
    );
    executor
        .block_on(b.run_turn("B_PRIVATE_PROMPT", None, None))
        .unwrap();
    assert!(transcript(load(&path, false).messages()).contains("B_PRIVATE_AFTER_A"));
    a.refresh_compaction_source().unwrap();
    assert_eq!(a.session().session_store_root(), Some(store_root.as_path()));
    assert!(Arc::ptr_eq(
        &writer,
        &a.session().completion_writer().unwrap()
    ));
    let refreshed = transcript(a.session().messages());
    assert!(refreshed.contains("A_BEFORE_REFRESH"));
    assert!(!refreshed.contains("B_PRIVATE_AFTER_A"));
    executor
        .block_on(a.run_turn("A_CONTINUES_AFTER_REFRESH", None, None))
        .unwrap();
    assert!(!transcript(requests.lock().unwrap().last().unwrap()).contains("B_PRIVATE_AFTER_A"));
    let restored = load(&path, false);
    assert!(transcript(restored.messages()).contains("A_AFTER_REFRESH"));
    assert!(!transcript(restored.messages()).contains("B_PRIVATE_AFTER_A"));
    let journal =
        runtime::subagent_completion::CompletionJournal::open(a.session(), &root, true).unwrap();
    journal
        .purge(&journal.storage_ref(), "delete-after-refresh")
        .unwrap();
    assert!(store_root.join(".cleanup").is_dir());
    assert_eq!(
        a.refresh_compaction_source().unwrap_err().to_string(),
        "session_deleted"
    );
    assert_eq!(
        a.ensure_session_persistence_allowed()
            .unwrap_err()
            .to_string(),
        "session_deleted"
    );
    drop(journal);
    drop((snapshot, writer, a, b, seed, restored));
    drop(executor);
    drop(fixture);
}

#[test]
#[ignore = "requires the real pinned Nexus daemon and mTLS credentials"]
fn live_runtime_resume_inherits_completed_turn_and_keeps_native_tool_result() {
    exercise_runtime_history(true, false);
}

fn exercise_runtime_history(nexus: bool, legacy: bool) {
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
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let sid = seed_runtime_history(&root, &path, nexus, legacy, &executor);
    // Capture B before A starts, then activate it after A's tool is already live.
    let stale_b = load(&path, nexus);
    let running = start_native_tool_turn(&root, &path, nexus);
    wait_for(&root.join("tool-pid"));
    assert!(!root.join("actual-result").exists());
    let loaded = load(&path, nexus);
    assert!(!transcript(loaded.messages()).contains("A_LIVE_TOOL_PROMPT"));
    assert!(transcript(loaded.messages()).contains("COMMON_COMPLETED_ANSWER"));
    assert!(!loaded
        .messages()
        .iter()
        .any(|message| message.blocks.iter().any(|block| matches!(
            block,
            runtime::ContentBlock::ToolResult { is_error: true, .. }
        ))));
    let (mut b, requests) = runtime(
        stale_b,
        vec![answer("B_PRIVATE_ANSWER")],
        StaticToolExecutor::new(),
    );
    executor
        .block_on(b.run_turn("B_PRIVATE_PROMPT", None, None))
        .unwrap();
    assert!(!transcript(&requests.lock().unwrap()[0]).contains("A_LIVE_TOOL_PROMPT"));
    assert!(transcript(&requests.lock().unwrap()[0]).contains("COMMON_COMPLETED_ANSWER"));
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
    assert_runtime_replay(&path, nexus, &sid);
    drop(a);
    drop(executor);
    drop(fixture);
}

fn seed_runtime_history(
    root: &Path,
    path: &Path,
    nexus: bool,
    legacy: bool,
    executor: &tokio::runtime::Runtime,
) -> String {
    let mut session = Session::new().with_workspace_root(root);
    for index in 0..PREFIX_TURNS {
        session
            .push_user_text(format!("PREFIX_PROMPT_{index}"))
            .unwrap();
        session
            .push_message(ConversationMessage::assistant(vec![
                runtime::ContentBlock::Text {
                    text: format!("PREFIX_ANSWER_{index}"),
                },
            ]))
            .unwrap();
    }
    let legacy_bytes = legacy.then(|| session.to_json().unwrap().render().into_bytes());
    if let Some(bytes) = &legacy_bytes {
        std::fs::write(path, bytes).unwrap();
    }
    let session = session
        .with_persistence_path(path)
        .with_fs_backend(backend(nexus));
    let sid = session.session_id.clone();
    if !legacy {
        session.initialize_persistence().unwrap();
    }
    let (mut warm, _) = runtime(
        session,
        vec![answer("COMMON_COMPLETED_ANSWER")],
        StaticToolExecutor::new(),
    );
    executor
        .block_on(warm.run_turn("COMMON_COMPLETED_PROMPT", None, None))
        .unwrap();
    drop(warm);

    if let Some(bytes) = legacy_bytes {
        let archives = legacy_archives(root);
        assert_eq!(archives.len(), 1, "one immutable source archive");
        assert_eq!(std::fs::read(&archives[0]).unwrap(), bytes);
    }

    sid
}

fn start_native_tool_turn(root: &Path, path: &Path, nexus: bool) -> RunningTurn {
    let thread_root = root.to_path_buf();
    let thread_path = path.to_path_buf();
    let thread = thread::spawn(move || {
        let _workspace = WorkspaceRootScope::enter(&thread_root);
        let child_root = thread_root.clone();
        let tools = StaticToolExecutor::new().register("bash", move |_| {
            let status = native_tool::NativeTool::spawn(
                &child_root.join("tool-pid"),
                &child_root.join("release"),
                &child_root.join("actual-result"),
                "A_ACTUAL_NATIVE_TOOL_RESULT",
                WAIT,
            )
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
    RunningTurn {
        root: root.to_path_buf(),
        thread: Some(thread),
    }
}

fn assert_runtime_replay(path: &Path, nexus: bool, sid: &str) {
    let restored = load(path, nexus);
    let text = transcript(restored.messages());
    for index in 0..PREFIX_TURNS {
        assert!(text.contains(&format!("PREFIX_PROMPT_{index}")));
        assert!(text.contains(&format!("PREFIX_ANSWER_{index}")));
    }
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
        !records.iter().any(|record| {
            record["event"]["type"] == "projection"
                && record["event"]["messages"]
                    .as_array()
                    .is_some_and(|messages| {
                        messages.iter().any(|entry| {
                            entry["message"]["blocks"].as_array().is_some_and(|blocks| {
                                blocks.iter().any(|block| {
                                    block["type"] == "tool_result" && block["is_error"] == true
                                })
                            })
                        })
                    })
        }),
        "a completed-turn restore must not synthesize a result for the sibling's running tool"
    );
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
            .find(|record| {
                record["event"]["type"] == "projection"
                    && graph
                        .completed_session(record["id"].as_str())
                        .is_ok_and(|view| {
                            view.messages().len() == 1
                                && transcript(view.messages()).contains("B_PRIVATE_ANSWER")
                        })
            })
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
