//! Restore interleaved same-SID history through the actual CLI and model wire.
mod common;
#[path = "support/request_dumps.rs"]
mod request_dumps;

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

use common::TestEnv;
use pty_expect::PtySession;
use runtime::fs_backend::{FsBackend, RecordLogMode, StdFsBackend};
use runtime::history::{HistoryGraph, HistoryLog, HistoryRecord, RecordOutcome};
use runtime::session_history::MessageIdentity;
use runtime::{ContentBlock, ConversationMessage, Session};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn enable_shared_history(env: &TestEnv) {
    let path = env.config_home().join("settings.json");
    let mut settings: Value = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => panic!("cannot read fixture settings: {error}"),
    };
    settings["experimental"]["sharedSessionHistory"] = json!(true);
    fs::write(path, settings.to_string()).unwrap();
}

fn shared_mock(label: &str) -> TestEnv {
    let env = TestEnv::new_mock(label);
    enable_shared_history(&env);
    env
}

fn save_seed(session: &Session, path: &std::path::Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let log = StdFsBackend
        .record_log(&path.to_string_lossy(), RecordLogMode::CreateNew)
        .unwrap();
    let mut history = HistoryLog::new(log);
    let record = HistoryRecord::initialize(session).unwrap();
    assert_eq!(
        history.commit(&record, Duration::from_secs(5)).unwrap(),
        RecordOutcome::Applied
    );
}

#[test]
fn fresh_session_uses_the_configured_committed_history_format() {
    let env = shared_mock("shared-sid-fresh-config");
    let prompt = env.prompt(
        "Calculate 2+2 and reply with 'The answer is N'.",
        "single_turn_text",
    );
    let mut cli = env.spawn_with_env(&["-p", &prompt, "--permission-mode", "read-only"], &[]);
    cli.expect("The answer is 4").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let store = runtime::SessionStore::from_cwd(env.workspace_root()).unwrap();
    let sessions = store.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    let bytes = fs::read(&sessions[0].path).unwrap();
    let graph =
        HistoryGraph::decode(&bytes).expect("fresh configured session uses committed records");
    assert!(graph.completed_session(None).unwrap().messages().len() >= 2);
}

fn identified(
    mut message: ConversationMessage,
    id: &str,
    parent: Option<&str>,
    origin: Option<&str>,
) -> ConversationMessage {
    message.history = Some(std::sync::Arc::new(MessageIdentity {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        tool_origins: origin.map_or_else(BTreeMap::new, |origin| {
            BTreeMap::from([("call_1".into(), origin.into())])
        }),
    }));
    message
}

fn tool_call() -> ConversationMessage {
    ConversationMessage::assistant(vec![ContentBlock::ToolUse {
        id: "call_1".into(),
        name: "read_file".into(),
        input: r#"{"path":"fixture.txt"}"#.into(),
        thought_signature: None,
    }])
}

#[test]
fn resume_pairs_reused_tool_ids_with_their_own_assistant_chain() {
    let env = TestEnv::new_mock("shared-sid-reused-call-id");
    let root = env.workspace_root();
    let path = root.join("interleaved.jsonl");
    let dump = root.join("requests");
    fs::create_dir_all(&dump).unwrap();
    seed_interleaved_tool_history(&path, root);
    let selected = Session::load_from_path(&path).unwrap();
    assert_eq!(selected.messages().len(), 3);
    assert_eq!(
        selected.messages()[2]
            .history
            .as_ref()
            .unwrap()
            .tool_origins["call_1"],
        "b-assistant"
    );
    let prompt = env.prompt(
        "Calculate 2+2 and reply with 'The answer is N'.",
        "single_turn_text",
    );
    let mut cli = env.spawn_with_env(
        &[
            "--resume",
            path.to_str().unwrap(),
            "-p",
            &prompt,
            "--permission-mode",
            "read-only",
        ],
        &[("SUDOCODE_DUMP_REQUESTS", dump.to_str().unwrap())],
    );
    cli.expect("The answer is 4").unwrap_or_else(|error| {
        panic!(
            "resumed model answer: {error}\n{}",
            cli.render(common::screen_contents)
        )
    });
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let mut inspected = 0;
    for entry in fs::read_dir(&dump).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_name().to_string_lossy().contains("messages") {
            continue;
        }
        let request: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let wire = request["messages"].to_string();
        assert!(
            wire.contains("B_SELECTED_RESULT"),
            "selected result missing"
        );
        assert!(
            !wire.contains("A_PRIVATE_RESULT"),
            "another context entered the model request"
        );
        inspected += 1;
    }
    assert!(inspected > 0, "actual model request must be inspected");
    let durable = fs::read_to_string(path).unwrap();
    assert!(
        durable.contains("A_PRIVATE_RESULT"),
        "the sibling branch must remain durable"
    );
    assert!(durable.contains("B_SELECTED_RESULT"));
}

fn wait_for_file(path: &std::path::Path) {
    wait_for_file_with_diagnostics(path, String::new);
}

fn wait_for_file_with_diagnostics(path: &std::path::Path, diagnostics: impl Fn() -> String) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "provider did not reach barrier {}\n{}",
            path.display(),
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What the durable transcript, request dump and both CLIs show while a
/// concurrent-context barrier is still pending.
fn concurrent_context_diagnostics(
    transcript: &std::path::Path,
    dump: &std::path::Path,
    first: &PtySession,
    second: &PtySession,
) -> String {
    let durable = fs::read_to_string(transcript).unwrap_or_default();
    format!(
        "first recorded: {}; second recorded: {}; dumped files: {}\nFirst CLI:\n{}\nSecond CLI:\n{}",
        durable.contains("FIRST_CONTEXT_PROMPT"),
        durable.contains("SECOND_CONTEXT_PROMPT"),
        fs::read_dir(dump).map(Iterator::count).unwrap_or_default(),
        first.render(common::screen_contents),
        second.render(common::screen_contents),
    )
}

fn exit_cli(env: &TestEnv, cli: &mut PtySession) {
    common::expect_input_line_cleared(cli, env.timeout(), "completed runtime accepts input");
    cli.send("/exit").unwrap();
    common::expect_input_line(cli, "/exit", env.timeout(), "exit entered");
    cli.send("\r").unwrap();
    let status = cli.expect_eof().unwrap();
    let screen = common::expect_screen(cli, |_| true, env.timeout(), "runtime exit diagnostic");
    assert_eq!(status, 0, "runtime exit failed: {screen}");
}

fn run_print_turn(env: &TestEnv, transcript: &std::path::Path, marker: &str) {
    let prompt = env.prompt(
        &format!("{marker} Calculate 2+2 and reply with 'The answer is N'."),
        "single_turn_text",
    );
    let dump = env.workspace_root().join("requests");
    let mut variables = vec![];
    if dump.is_dir() {
        variables.push(("SUDOCODE_DUMP_REQUESTS", dump.to_str().unwrap()));
    }
    let mut cli = env.spawn_with_env(
        &[
            "--resume",
            transcript.to_str().unwrap(),
            "-p",
            &prompt,
            "--permission-mode",
            "read-only",
        ],
        &variables,
    );
    cli.expect("The answer is 4").unwrap_or_else(|error| {
        panic!(
            "resumed model answer: {error:?}\n{}",
            cli.render(common::screen_contents)
        )
    });
    assert_eq!(cli.expect_eof().unwrap(), 0);
}

#[test]
fn independent_cli_processes_append_to_the_same_sid_while_both_turns_are_open() {
    run_two_cli_contexts(false, false);
}

#[test]
fn resuming_during_another_turn_inherits_the_latest_completed_turn() {
    run_two_cli_contexts(true, false);
}

#[test]
fn legacy_session_migrates_before_independent_cli_processes_resume() {
    run_two_cli_contexts(true, true);
}

fn run_two_cli_contexts(is_second_started_late: bool, is_legacy_seed: bool) {
    let env = shared_mock(if is_legacy_seed {
        "shared-sid-legacy-two-processes"
    } else if is_second_started_late {
        "shared-sid-late-runtime"
    } else {
        "shared-sid-two-processes"
    });
    let root = env.workspace_root();
    let transcript = root.join("shared.jsonl");
    seed_cli_context(root, &transcript, is_legacy_seed);
    let dump = root.join("requests");
    fs::create_dir_all(&dump).unwrap();
    let variables = [("SUDOCODE_DUMP_REQUESTS", dump.to_str().unwrap())];
    let args = [
        "--resume",
        transcript.to_str().unwrap(),
        "--permission-mode",
        "read-only",
    ];
    let mut first = env.spawn_with_env(&args, &variables);
    common::expect_input_line_cleared(&first, env.timeout(), "first runtime ready");
    let mut second = (!is_second_started_late).then(|| env.spawn_with_env(&args, &variables));
    if let Some(second) = &second {
        common::expect_input_line_cleared(second, env.timeout(), "second runtime ready");
    }
    let release_first = root.join("release-first-stream");
    let release_second = root.join("release-second-stream");
    let first_ready = root.join("first-model-connected");
    let second_ready = root.join("second-model-connected");
    let prompt = common::terminal_host::prose_prompt(
        &env,
        "FIRST_CONTEXT_DONE",
        json!({
            "start_ready": first_ready, "start_release": release_first,
            "release": release_first, "delay_ms": 0,
        }),
    );
    let prompt = format!("FIRST_CONTEXT_PROMPT {prompt}");
    first.send(&prompt).unwrap();
    common::expect_input_line(
        &first,
        "FIRST_CONTEXT_PROMPT",
        env.timeout(),
        "first prompt entered",
    );
    first.send("\r").unwrap();
    wait_for_file(&first_ready);
    let second = second.get_or_insert_with(|| env.spawn_with_env(&args, &variables));
    if is_second_started_late {
        common::expect_input_line_cleared(
            second,
            env.timeout(),
            "late runtime restored the completed prefix",
        );
    }
    // The first response remains blocked while a separate process submits the
    // same SID. No sleep or guessed response duration creates this overlap.
    let prompt = common::terminal_host::prose_prompt(
        &env,
        "SECOND_CONTEXT_DONE",
        json!({
            "start_ready": second_ready, "start_release": release_second,
            "release": release_second, "delay_ms": 0,
        }),
    );
    let prompt = format!("SECOND_CONTEXT_PROMPT {prompt}");
    second.send(&prompt).unwrap();
    common::expect_input_line(
        second,
        "SECOND_CONTEXT_PROMPT",
        env.timeout(),
        "second prompt entered",
    );
    second.send("\r").unwrap();
    wait_for_file_with_diagnostics(&second_ready, || {
        concurrent_context_diagnostics(&transcript, &dump, &first, second)
    });
    let waiting = fs::read_to_string(&transcript).unwrap();
    assert!(waiting.contains("FIRST_CONTEXT_PROMPT") && waiting.contains("SECOND_CONTEXT_PROMPT"));
    assert!(!waiting.contains("FIRST_CONTEXT_DONE") && !waiting.contains("SECOND_CONTEXT_DONE"));
    fs::write(release_second, "release B while A is pending").unwrap();
    second
        .expect("SECOND_CONTEXT_DONE")
        .expect("second concurrent response");
    common::expect_input_line_cleared(second, env.timeout(), "second response committed");
    fs::write(release_first, "release A after B completed").unwrap();
    first
        .expect("FIRST_CONTEXT_DONE")
        .expect("first concurrent response");
    common::expect_input_line_cleared(&first, env.timeout(), "first response committed");
    // A new runtime continues the latest branch before these older runtimes
    // close. An idle close must preserve the newer continuation as the head.
    run_print_turn(&env, &transcript, "FRESH_CONTEXT_PROMPT");
    for cli in [&mut first, second] {
        exit_cli(&env, cli);
    }
    assert_two_cli_contexts(&transcript, &dump, is_second_started_late, is_legacy_seed);
}

fn seed_cli_context(root: &std::path::Path, transcript: &std::path::Path, is_legacy_seed: bool) {
    let mut seed = Session::new().with_workspace_root(root);
    if is_legacy_seed {
        seed.push_user_text("LEGACY_DURABLE_PREFIX").unwrap();
        seed.push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "LEGACY_DURABLE_ANSWER".into(),
        }]))
        .unwrap();
        fs::write(transcript, seed.to_json().unwrap().render()).unwrap();
    } else {
        save_seed(&seed, transcript);
    }
}

fn assert_two_cli_contexts(
    transcript: &std::path::Path,
    dump: &std::path::Path,
    is_second_started_late: bool,
    is_legacy_seed: bool,
) {
    let durable = fs::read_to_string(transcript).unwrap();
    for expected in [
        "FIRST_CONTEXT_PROMPT",
        "SECOND_CONTEXT_PROMPT",
        "FIRST_CONTEXT_DONE",
        "SECOND_CONTEXT_DONE",
    ] {
        assert!(
            durable.contains(expected),
            "concurrent context lost: {expected}"
        );
    }
    let selected = Session::load_from_path(transcript).expect("concurrent log remains resumable");
    if is_legacy_seed {
        assert!(selected
            .messages()
            .iter()
            .flat_map(|message| &message.blocks)
            .any(|block| {
                matches!(block, ContentBlock::Text { text } if text == "LEGACY_DURABLE_PREFIX")
            }));
    }
    assert!(selected.messages().iter().flat_map(|message| &message.blocks).any(|block| {
        matches!(block, ContentBlock::Text { text } if text.contains("FRESH_CONTEXT_PROMPT"))
    }), "closing an idle older runtime changed the default resume branch");
    assert!(
        selected
            .messages()
            .iter()
            .flat_map(|message| &message.blocks)
            .any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("FIRST_CONTEXT_DONE"))
            }),
        "fresh resume must select the last completed branch, like CC"
    );
    let mut inspected = 0;
    for entry in fs::read_dir(dump).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_name().to_string_lossy().contains("messages") {
            continue;
        }
        let request: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let messages = request["messages"].to_string();
        if is_second_started_late && messages.contains("SECOND_CONTEXT_PROMPT") {
            assert!(
                !messages.contains("FIRST_CONTEXT_PROMPT"),
                "a later resume must exclude the sibling's unfinished user message"
            );
            assert!(
                !messages.contains("FIRST_CONTEXT_DONE"),
                "the sibling's later reply cannot enter an in-flight context"
            );
        } else {
            assert_ne!(
                messages.contains("FIRST_CONTEXT_PROMPT"),
                messages.contains("SECOND_CONTEXT_PROMPT"),
                "each model call must retain its own context"
            );
        }
        inspected += 1;
    }
    assert_eq!(
        inspected, 3,
        "both independent model requests and the fresh continuation must be inspected"
    );
}

fn write_gate(root: &std::path::Path) {
    fs::write(
        root.join("gate.cjs"),
        r"
const fs = require('fs');
const label = process.argv[2];
fs.writeFileSync(label + '.ready', 'tool running');
const timer = setInterval(() => {
  if (!fs.existsSync(label + '.release')) return;
  clearInterval(timer);
  fs.appendFileSync(label + '.effect', label + ' durable effect');
  process.stdout.write((label + '_OUTPUT_').repeat(6000));
}, 10);
",
    )
    .unwrap();
}

fn begin_gate(env: &TestEnv, cli: &mut PtySession, label: &str) {
    let task = if env.is_live() {
        format!("Use the Bash tool to run exactly `node gate.cjs {label}` once in the current working directory, with timeout 180000ms. This acceptance script waits for an external release file. Wait for the tool to finish; keep this call in the foreground. After it finishes, reply exactly: Concurrency batch done.")
    } else {
        format!(
            "TOOL_BATCH:{}",
            json!([{
                "id":"call_1", "name":"Bash", "input": {
                    "command":format!("node gate.cjs {label}"), "description":format!("{label} shared-session tool")
                }
            }])
        )
    };
    let prompt = env.prompt(&task, "tool_concurrency");
    cli.send(&prompt).unwrap();
    common::expect_input_line(
        cli,
        if env.is_live() {
            "Use the Bash tool"
        } else {
            "TOOL_BATCH:"
        },
        env.timeout(),
        "tool batch entered",
    );
    cli.send("\r").unwrap();
    wait_for_file(&env.workspace_root().join(format!("{label}.ready")));
}

#[test]
fn concurrent_tools_with_the_same_provider_id_keep_both_effects_and_full_outputs() {
    let env = shared_mock("shared-sid-concurrent-tools");
    run_two_context_tools(&env, false);
}

#[test]
fn restoring_during_a_native_tool_uses_the_completed_turn_and_retains_both_branches() {
    let env = shared_mock("shared-sid-completed-turn-restore");
    run_two_context_tools(&env, true);
}

#[test]
#[ignore = "requires SCODE_TEST_BACKEND=live and a funded selected provider"]
fn live_independent_runtimes_preserve_tools_and_outputs_in_one_session() {
    assert_eq!(std::env::var("SCODE_TEST_BACKEND").as_deref(), Ok("live"));
    let env = TestEnv::new("shared-sid-live-concurrent-tools");
    enable_shared_history(&env);
    run_two_context_tools(&env, true);
    reclaim_live_session_and_its_backup(&env);
}

fn reclaim_live_session_and_its_backup(env: &TestEnv) {
    let root = env.workspace_root();
    let transcript = root.join("shared.jsonl");
    let session = Session::load_from_path(&transcript).unwrap();
    let sid = &session.session_id;
    let outputs = Session::tool_results_dir_for(&transcript, sid).unwrap();
    let responses = Session::response_captures_dir_for(&transcript, sid).unwrap();
    assert!(outputs.is_dir());
    assert!(responses.is_dir());
    let mut clear = env.spawn_with_env(&["--resume", "shared.jsonl", "/clear", "--confirm"], &[]);
    clear.expect("Session cleared").unwrap();
    assert_eq!(clear.expect_eof().unwrap(), 0);
    let backup = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("shared.jsonl.before-clear-")
                && path.extension().is_some_and(|extension| extension == "bak")
        })
        .expect("the real clear command backed up the funded conversation");
    let backup_name = backup.file_name().unwrap().to_str().unwrap();
    let mut primary = env.spawn_with_env(&["--resume", "shared.jsonl"], &[]);
    common::expect_input_line_cleared(&primary, env.timeout(), "funded source restored");
    let mut restored = env.spawn_with_env(&["--resume", backup_name], &[]);
    common::expect_input_line_cleared(&restored, env.timeout(), "funded backup restored");
    let controller = Session::new().with_workspace_root(root);
    save_seed(
        &controller,
        &managed_session_path(env, &controller.session_id),
    );
    let mut deleter = env.spawn_with_env(&["--resume", &controller.session_id], &[]);
    common::expect_input_line_cleared(&deleter, env.timeout(), "funded deletion controller ready");
    deleter
        .send("/session delete shared.jsonl --force\r")
        .unwrap();
    deleter.expect("Session deleted").unwrap();
    let before = fs::read(&backup).unwrap();
    restored
        .send("DELETED_FUNDED_BACKUP_MUST_NOT_WRITE\r")
        .unwrap();
    restored.expect("session_deleted").unwrap();
    assert_eq!(fs::read(&backup).unwrap(), before);
    exit_cli(env, &mut primary);
    assert!(transcript.exists() && backup.exists());
    assert!(outputs.is_dir() && responses.is_dir());
    exit_cli(env, &mut restored);
    wait_for_reclamation(&transcript, env.timeout());
    assert!(!backup.exists() && !outputs.exists() && !responses.exists());
    for reference in ["shared.jsonl", backup_name] {
        assert_eq!(
            Session::load_from_path(root.join(reference))
                .unwrap_err()
                .to_string(),
            "session_deleted"
        );
        let mut refused = env.spawn_with_env(&["--resume", reference], &[]);
        refused.expect("session not found:").unwrap();
        assert_ne!(refused.expect_eof().unwrap(), 0);
    }
    for label in ["A", "B"] {
        assert_eq!(
            fs::read_to_string(root.join(format!("{label}.effect"))).unwrap(),
            format!("{label} durable effect")
        );
    }
    assert!(transcript.with_extension("completions.json").is_file());
    exit_cli(env, &mut deleter);
}

fn run_two_context_tools(env: &TestEnv, is_second_started_late: bool) {
    let root = env.workspace_root();
    let transcript = root.join("shared.jsonl");
    let seed = Session::new().with_workspace_root(root);
    let sid = seed.session_id.clone();
    save_seed(&seed, &transcript);
    write_gate(root);
    if is_second_started_late {
        run_print_turn(env, &transcript, "SHARED_COMPLETED_PREFIX");
    }
    let dump = root.join("requests");
    fs::create_dir_all(&dump).unwrap();
    let variables = [("SUDOCODE_DUMP_REQUESTS", dump.to_str().unwrap())];
    let args = [
        "--resume",
        transcript.to_str().unwrap(),
        "--permission-mode",
        "danger-full-access",
    ];
    let mut first = env.spawn_with_env(&args, &variables);
    common::expect_input_line_cleared(&first, env.timeout(), "first tool runtime ready");
    let mut second = (!is_second_started_late).then(|| env.spawn_with_env(&args, &variables));
    if let Some(second) = &second {
        common::expect_input_line_cleared(second, env.timeout(), "second tool runtime ready");
    }
    begin_gate(env, &mut first, "A");
    let second = second.get_or_insert_with(|| env.spawn_with_env(&args, &variables));
    common::expect_input_line_cleared(second, env.timeout(), "second tool runtime ready");
    begin_gate(env, second, "B");
    assert!(!root.join("A.effect").exists() && !root.join("B.effect").exists());
    // Finish B while A still owns an unfinished tool with the same call ID.
    fs::write(root.join("B.release"), "release B").unwrap();
    wait_for_file(&root.join("B.effect"));
    second
        .expect("Concurrency batch done.")
        .expect("B completed its actual tool roundtrip");
    assert_eq!(
        fs::read_to_string(root.join("B.effect")).unwrap(),
        "B durable effect"
    );
    assert!(!root.join("A.effect").exists());
    if is_second_started_late {
        common::expect_input_line_cleared(second, env.timeout(), "B accepts input after its turn");
        wait_for_completed_gate(env, &transcript, "B");
        run_print_turn(env, &transcript, "FRESH_WHILE_A_TOOL_OPEN");
        assert!(!root.join("A.effect").exists());
        assert_completed_tool_restore_requests(&dump);
    }
    fs::write(root.join("A.release"), "release A").unwrap();
    wait_for_file(&root.join("A.effect"));
    first
        .expect("Concurrency batch done.")
        .expect("A completed its actual tool roundtrip");
    assert_eq!(
        fs::read_to_string(root.join("A.effect")).unwrap(),
        "A durable effect"
    );
    for cli in [&mut first, second] {
        exit_cli(env, cli);
    }
    assert_gate_outputs(&transcript, &sid, env.is_mock().then_some("call_1"));
}

fn wait_for_completed_gate(env: &TestEnv, transcript: &std::path::Path, label: &str) {
    let deadline = Instant::now() + env.timeout();
    loop {
        let graph = HistoryGraph::decode(&fs::read(transcript).unwrap()).unwrap();
        let session = graph.completed_session(None).unwrap();
        let completed = session.to_json().unwrap().render();
        if completed.contains(&format!("node gate.cjs {label}"))
            && completed.contains("Concurrency batch done.")
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{label}'s turn was not durably committed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_completed_tool_restore_requests(dump: &std::path::Path) {
    let mut sibling_requests = 0;
    let mut fresh_requests = 0;
    for entry in fs::read_dir(dump).unwrap() {
        let entry = entry.unwrap();
        if !request_dumps::is_inference_request_dump(&entry.path()) {
            continue;
        }
        let request: Value = serde_json::from_slice(&fs::read(entry.path()).unwrap()).unwrap();
        let wire = request
            .get("messages")
            .or_else(|| request.get("input"))
            .expect("provider request carries its conversation")
            .to_string();
        if wire.contains("node gate.cjs B") || wire.contains("FRESH_WHILE_A_TOOL_OPEN") {
            assert!(wire.contains("SHARED_COMPLETED_PREFIX"));
            assert!(
                !wire.contains("node gate.cjs A"),
                "a new context inherited the source's running tool: {wire}"
            );
            assert!(!wire.contains("Side effects may already have occurred"));
            sibling_requests += 1;
        }
        if wire.contains("FRESH_WHILE_A_TOOL_OPEN") {
            assert!(wire.contains("node gate.cjs B"));
            assert!(wire.contains("Concurrency batch done."));
            fresh_requests += 1;
        }
    }
    assert!(
        sibling_requests >= 3,
        "inspect both B requests and the fresh restore"
    );
    assert_eq!(fresh_requests, 1);
}

#[test]
fn deleted_session_is_hidden_and_reclaimed_after_its_last_runtime_retires() {
    let env = shared_mock("shared-sid-idle-close");
    let session = Session::new().with_workspace_root(env.workspace_root());
    let transcript = managed_session_path(&env, &session.session_id);
    save_seed(&session, &transcript);
    let mut idle = env.spawn_with_env(
        &[
            "--resume",
            &session.session_id,
            "--permission-mode",
            "read-only",
        ],
        &[],
    );
    common::expect_input_line_cleared(&idle, env.timeout(), "idle runtime restored");
    run_print_turn(
        &env,
        std::path::Path::new(&session.session_id),
        "LATEST_ACTIVE_CONTEXT",
    );
    let survivor = Session::new().with_workspace_root(env.workspace_root());
    let controller = managed_session_path(&env, &survivor.session_id);
    save_seed(&survivor, &controller);
    let delete = || {
        let mut cli = env.spawn_with_env(
            &["--resume", &survivor.session_id],
            &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
        );
        common::expect_input_line_cleared(&cli, env.timeout(), "deletion controller ready");
        cli.send(&format!("/session delete {} --force\r", session.session_id))
            .unwrap();
        cli
    };
    let selected = Session::load_from_path(&transcript).unwrap();
    assert!(selected.messages().iter().flat_map(|message| &message.blocks).any(|block| {
        matches!(block, ContentBlock::Text { text } if text.contains("LATEST_ACTIVE_CONTEXT"))
    }), "idle metadata changed the latest resume context");
    let (output_dir, external) =
        seed_owned_session_payload(env.workspace_root(), &transcript, &session.session_id);
    let mut deleter = delete();
    deleter
        .expect("Session deleted")
        .expect("logical deletion succeeds while the other runtime is alive");
    common::expect_input_line_cleared(&deleter, env.timeout(), "deletion completed");
    assert!(
        transcript.exists(),
        "an admitted client still holds its log"
    );
    assert!(output_dir.join("owned-payload").exists());
    assert!(Session::load_from_path(&transcript)
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    deleter.send("/session list\r").unwrap();
    common::expect_screen_settled(
        &deleter,
        |screen| {
            screen
                .rsplit_once("Select a session")
                .is_some_and(|(_, picker)| {
                    !picker.contains(&session.session_id)
                        && picker.contains(&survivor.session_id)
                        && picker.contains("msgs=")
                })
        },
        env.timeout(),
        "the actual Session list excludes the deleted SID",
    );
    deleter.send("\x1b").unwrap();
    common::expect_input_line_cleared(&deleter, env.timeout(), "Session picker cancelled");
    let before = fs::read(&transcript).unwrap();
    idle.send("STALE_WRITE_MUST_NOT_COMMIT\r").unwrap();
    idle.expect("session_deleted")
        .expect("the old context cannot write after deletion");
    assert_eq!(fs::read(&transcript).unwrap(), before);
    let mut refused = env.spawn_with_env(&["--resume", &session.session_id], &[]);
    refused
        .expect("session_deleted")
        .expect("a new runtime cannot reopen the deleted SID");
    assert_ne!(refused.expect_eof().unwrap(), 0);
    exit_cli(&env, &mut idle);
    wait_for_reclamation(&transcript, env.timeout());
    assert!(!output_dir.exists());
    assert_eq!(
        fs::read_to_string(external.join("keep.txt")).unwrap(),
        "independent object survives"
    );
    assert!(
        transcript.with_extension("completions.json").exists(),
        "retain the deletion identity"
    );
    assert_damaged_session_picker(&env, &mut deleter, &survivor, &session);
    exit_cli(&env, &mut deleter);
}

fn seed_owned_session_payload(
    workspace: &std::path::Path,
    transcript: &std::path::Path,
    session_id: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let output_dir = Session::tool_results_dir_for(transcript, session_id).unwrap();
    fs::create_dir_all(&output_dir).unwrap();
    fs::write(output_dir.join("owned-payload"), "Session-owned payload").unwrap();
    let external = workspace.join("independent-workspace");
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("keep.txt"), "independent object survives").unwrap();
    StdFsBackend
        .link(
            &output_dir.join("workspace-link").to_string_lossy(),
            &external.to_string_lossy(),
        )
        .unwrap();
    (output_dir, external)
}

fn assert_damaged_session_picker(
    env: &TestEnv,
    deleter: &mut PtySession,
    survivor: &Session,
    session: &Session,
) {
    let damaged = Session::new().with_workspace_root(env.workspace_root());
    let damaged_path = managed_session_path(env, &damaged.session_id);
    save_seed(&damaged, &damaged_path);
    fs::write(
        damaged_path.with_extension("completions.json"),
        b"{invalid binding",
    )
    .unwrap();
    deleter.send("/session list\r").unwrap();
    common::expect_screen_settled(
        deleter,
        |screen| {
            screen
                .rsplit_once("Select a session")
                .is_some_and(|(_, picker)| {
                    picker.contains(&survivor.session_id)
                        && picker.contains(&damaged.session_id)
                        && !picker.contains(&session.session_id)
                })
        },
        env.timeout(),
        "one damaged binding must not block the healthy Session picker",
    );
    deleter.send("\x1b").unwrap();
    common::expect_input_line_cleared(deleter, env.timeout(), "damaged Session picker cancelled");
}

fn wait_for_reclamation(transcript: &std::path::Path, budget: Duration) {
    let deadline = Instant::now() + budget;
    while transcript.exists() {
        assert!(
            Instant::now() < deadline,
            "automatic Session cleanup did not finish: {}",
            transcript.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn managed_session_path(env: &TestEnv, session_id: &str) -> std::path::PathBuf {
    // Only the actual CLI processes may discover and perform cleanup.
    let workspace = fs::canonicalize(env.workspace_root()).unwrap();
    let root = workspace
        .join(".scode/sessions")
        .join(runtime::session_control::workspace_fingerprint(&workspace));
    runtime::session_control::session_transcript_path(&root, session_id)
}

#[test]
fn deleted_session_reclaims_clear_backup_after_its_runtime_retires() {
    let env = shared_mock("shared-sid-backup-retirement");
    let session = Session::new().with_workspace_root(env.workspace_root());
    let transcript = managed_session_path(&env, &session.session_id);
    save_seed(&session, &transcript);
    let mut primary = env.spawn_with_env(&["--resume", &session.session_id], &[]);
    common::expect_input_line_cleared(&primary, env.timeout(), "source runtime ready");
    let mut clear = env.spawn_with_env(
        &["--resume", &session.session_id, "/clear", "--confirm"],
        &[],
    );
    clear.expect("Session cleared").unwrap();
    assert_eq!(clear.expect_eof().unwrap(), 0);
    let backup = fs::read_dir(transcript.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("transcript.jsonl.before-clear-")
                && path.extension().is_some_and(|extension| extension == "bak")
        })
        .expect("the real /clear command published its owned backup");
    let workspace = fs::canonicalize(env.workspace_root()).unwrap();
    let reference = backup
        .strip_prefix(&workspace)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut restored = env.spawn_with_env(&["--resume", &reference], &[]);
    common::expect_input_line_cleared(&restored, env.timeout(), "backup runtime ready");
    let independent = transcript
        .parent()
        .unwrap()
        .join("transcript.jsonl.before-clear-user.bak");
    fs::write(&independent, b"independently owned file").unwrap();
    let controller = Session::new().with_workspace_root(env.workspace_root());
    save_seed(
        &controller,
        &managed_session_path(&env, &controller.session_id),
    );
    let mut deleter = env.spawn_with_env(&["--resume", &controller.session_id], &[]);
    common::expect_input_line_cleared(&deleter, env.timeout(), "delete controller ready");
    deleter
        .send(&format!("/session delete {} --force\r", session.session_id))
        .unwrap();
    deleter.expect("Session deleted").unwrap();
    let before = fs::read(&backup).unwrap();
    restored.send("BACKUP_WRITE_MUST_BE_REJECTED\r").unwrap();
    restored.expect("session_deleted").unwrap();
    assert_eq!(fs::read(&backup).unwrap(), before);
    exit_cli(&env, &mut primary);
    assert!(
        transcript.exists(),
        "the backup runtime still owns this Session"
    );
    assert!(backup.exists());
    let mut refused = env.spawn_with_env(&["--resume", &reference], &[]);
    refused.expect("session_deleted").unwrap();
    assert_ne!(refused.expect_eof().unwrap(), 0);
    exit_cli(&env, &mut restored);
    wait_for_reclamation(&transcript, env.timeout());
    assert!(!backup.exists());
    assert_eq!(fs::read(&independent).unwrap(), b"independently owned file");
    assert!(Session::load_from_path(&backup)
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    exit_cli(&env, &mut deleter);
}

#[test]
fn deleted_session_cleanup_resumes_after_the_deleter_and_old_runtime_stop() {
    cleanup_after_restart("directory");
}

#[test]
fn deleted_session_at_an_external_path_is_reclaimed_on_ordinary_restart() {
    cleanup_after_restart("external");
}

#[test]
fn deleted_session_in_the_legacy_json_layout_is_reclaimed_on_ordinary_restart() {
    cleanup_after_restart("legacy-json");
}

fn cleanup_after_restart(layout: &str) {
    let env = shared_mock(&format!("shared-sid-cleanup-restart-{layout}"));
    let session = Session::new().with_workspace_root(env.workspace_root());
    let managed = managed_session_path(&env, &session.session_id);
    let transcript = match layout {
        "directory" => managed.clone(),
        "legacy-json" => managed.parent().unwrap().with_extension("json"),
        "external" => env.workspace_root().join("exported/independent-name.jsonl"),
        _ => unreachable!(),
    };
    save_seed(&session, &transcript);
    // Managed layouts use the public SID reference. Keep canonical Windows
    // verbatim paths for filesystem I/O rather than passing them through Bash.
    let reference = if layout == "external" {
        transcript.to_string_lossy().into_owned()
    } else {
        session.session_id.clone()
    };
    let idle = env.spawn_with_env(&["--resume", &reference], &[]);
    common::expect_input_line_cleared(&idle, env.timeout(), "old runtime ready");
    let controller = env.workspace_root().join("delete-controller.jsonl");
    Session::new()
        .with_workspace_root(env.workspace_root())
        .save_to_path(&controller)
        .unwrap();
    let mut deleter = env.spawn_with_env(&["--resume", controller.to_str().unwrap()], &[]);
    common::expect_input_line_cleared(&deleter, env.timeout(), "deletion controller ready");
    deleter
        .send(&format!("/session delete {reference} --force\r"))
        .unwrap();
    deleter.expect("Session deleted").unwrap();
    assert!(transcript.exists(), "the old runtime has not retired");
    exit_cli(&env, &mut deleter);
    // Dropping the PTY terminates and reaps its real child; it does not run the
    // CLI's normal /exit final save or a manual cleanup operation.
    drop(idle);
    assert!(
        transcript.exists(),
        "the old cleanup worker exited with its process"
    );
    let mut restarted = env.spawn_with_env(&["--permission-mode", "read-only"], &[]);
    common::expect_input_line_cleared(&restarted, env.timeout(), "ordinary CLI restart");
    let deadline = Instant::now() + env.timeout();
    wait_for_reclamation(&transcript, env.timeout());
    assert!(transcript.with_extension("completions.json").exists());
    let references = managed.parent().unwrap().parent().unwrap().join(".cleanup");
    while fs::read_dir(&references).unwrap().next().is_some() {
        assert!(
            Instant::now() < deadline,
            "finished discovery references are removed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    exit_cli(&env, &mut restarted);
}

fn assert_gate_outputs(transcript: &std::path::Path, sid: &str, expected_call_id: Option<&str>) {
    let mut messages = BTreeMap::new();
    let bytes = fs::read(transcript).unwrap();
    let graph = HistoryGraph::decode(&bytes).unwrap();
    for line in std::str::from_utf8(&bytes).unwrap().lines() {
        let record: Value = serde_json::from_str(line).unwrap();
        if matches!(
            record["event"]["type"].as_str(),
            Some("commit_turn" | "projection")
        ) {
            if let Ok(session) = graph.completed_session(record["id"].as_str()) {
                for message in session.messages() {
                    let message: Value = serde_json::from_str(&message.to_json().render()).unwrap();
                    messages.insert(
                        message["history"]["id"].as_str().unwrap().to_owned(),
                        message,
                    );
                }
            }
        }
    }
    let output_dir = Session::tool_results_dir_for(transcript, sid).unwrap();
    let mut outputs = BTreeMap::new();
    for message in messages.values() {
        for block in message["blocks"].as_array().unwrap() {
            if block["type"] != "tool_result" {
                continue;
            }
            let call_id = block["tool_use_id"].as_str().unwrap();
            let origin = message["history"]["tool_origins"][call_id]
                .as_str()
                .expect("result identifies its assistant");
            let assistant = &messages[origin];
            let input = assistant["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| block["type"] == "tool_use" && block["id"] == call_id)
                .unwrap()["input"]
                .clone();
            let command: Value = match input {
                Value::String(input) => serde_json::from_str(&input).unwrap(),
                input => input,
            };
            let Some(label) = command["command"]
                .as_str()
                .and_then(|command| command.trim().strip_prefix("node gate.cjs "))
            else {
                continue;
            };
            assert!(
                matches!(label, "A" | "B"),
                "unexpected acceptance tool command"
            );
            if let Some(expected) = expected_call_id {
                assert_eq!(call_id, expected);
            }
            let marker = block["output"].as_str().unwrap();
            let output_id = marker
                .split("id=\"")
                .nth(1)
                .expect("large output is offloaded")
                .split('"')
                .next()
                .unwrap();
            let full = fs::read_to_string(output_dir.join(output_id)).unwrap();
            assert!(
                full.contains(&format!("{label}_OUTPUT_").repeat(6000)),
                "full tool output must retain every emitted byte"
            );
            let other = if label == "A" { "B" } else { "A" };
            assert!(
                !full.contains(&format!("{other}_OUTPUT_")),
                "another context overwrote the full output"
            );
            outputs.insert(label.to_owned(), output_id.to_owned());
        }
    }
    assert_eq!(outputs.len(), 2);
    assert_ne!(outputs["A"], outputs["B"]);
    Session::load_from_path(transcript).expect("tool histories remain resumable");
}

#[test]
fn repeated_provider_call_ids_in_one_runtime_keep_distinct_assistants_and_outputs() {
    let env = shared_mock("shared-sid-repeated-turn-call-id");
    let root = env.workspace_root();
    let transcript = root.join("shared.jsonl");
    let seed = Session::new().with_workspace_root(root);
    let sid = seed.session_id.clone();
    save_seed(&seed, &transcript);
    write_gate(root);
    let mut cli = env.spawn_with_env(
        &[
            "--resume",
            transcript.to_str().unwrap(),
            "--permission-mode",
            "danger-full-access",
        ],
        &[],
    );
    common::expect_input_line_cleared(&cli, env.timeout(), "runtime ready");
    for label in ["A", "B"] {
        begin_gate(&env, &mut cli, label);
        fs::write(root.join(format!("{label}.release")), "complete this turn").unwrap();
        cli.expect("Concurrency batch done.")
            .expect("actual repeated-ID tool completes");
        common::expect_input_line_cleared(&cli, env.timeout(), "turn complete");
        assert_eq!(
            fs::read_to_string(root.join(format!("{label}.effect"))).unwrap(),
            format!("{label} durable effect")
        );
    }
    cli.send("/session fork inherited-outputs\r").unwrap();
    cli.expect("Session forked").expect("fork through the CLI");
    common::expect_input_line_cleared(&cli, env.timeout(), "fork published");
    let store = runtime::SessionStore::from_cwd(root).unwrap();
    let forks: Vec<_> = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.parent_session_id.as_deref() == Some(sid.as_str()))
        .collect();
    assert_eq!(forks.len(), 1);
    let fork = &forks[0];
    let original_outputs = Session::tool_results_dir_for(&transcript, &sid).unwrap();
    let fork_outputs = Session::tool_results_dir_for(&fork.path, &fork.id).unwrap();
    let mut calls = Vec::new();
    for (index, entry) in fs::read_dir(original_outputs).unwrap().enumerate() {
        let original = entry.unwrap().path();
        let id = original.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            fs::read(&original).unwrap(),
            fs::read(fork_outputs.join(id)).unwrap()
        );
        calls.push(
            json!({"id":format!("fork-read-{index}"), "name":"read_tool_output",
            "input":{"id":id,"offset":0,"limit":256}}),
        );
    }
    assert_eq!(calls.len(), 2);
    cli.send(&format!(
        "PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}\r",
        json!(calls)
    ))
    .unwrap();
    cli.expect("Concurrency batch done.")
        .expect("fork reads inherited full outputs");
    common::expect_input_line_cleared(&cli, env.timeout(), "fork reads complete");
    exit_cli(&env, &mut cli);
    let restored = Session::load_from_path(&fork.path).unwrap();
    for label in ["A", "B"] {
        assert!(restored.messages().iter().flat_map(|m| &m.blocks).any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, output, .. }
                if tool_use_id.starts_with("fork-read-") && output.contains(&format!("{label}_OUTPUT_")))
        }), "fork failed to read {label}'s inherited full output");
    }
    assert_gate_outputs(&transcript, &sid, Some("call_1"));
}

#[test]
fn compacting_one_runtime_preserves_another_runtimes_open_tool_and_context() {
    let env = shared_mock("shared-sid-compact-open-tool");
    let root = env.workspace_root();
    let transcript = root.join("shared.jsonl");
    let mut seed = Session::new().with_workspace_root(root);
    for index in 0..32 {
        let text = format!("SEED_DECISION_{index} {}", "x".repeat(3990));
        seed.push_message(if index % 2 == 0 {
            ConversationMessage::user_text(text)
        } else {
            ConversationMessage::assistant(vec![ContentBlock::Text { text }])
        })
        .unwrap();
    }
    save_seed(&seed, &transcript);
    write_gate(root);
    let args = [
        "--resume",
        transcript.to_str().unwrap(),
        "--permission-mode",
        "danger-full-access",
    ];
    let variables = [];
    let mut compacting = env.spawn_with_env(&args, &variables);
    common::expect_input_line_cleared(&compacting, env.timeout(), "compacting runtime ready");
    let mut working = env.spawn_with_env(&args, &variables);
    common::expect_input_line_cleared(&working, env.timeout(), "working runtime ready");
    begin_gate(&env, &mut working, "B");
    let original_ids: std::collections::BTreeSet<_> = Session::load_from_path(&transcript)
        .unwrap()
        .messages()
        .iter()
        .map(|message| message.history.as_ref().unwrap().id.clone())
        .collect();
    compacting.send("/compact").unwrap();
    common::expect_input_line(&compacting, "/compact", env.timeout(), "compact entered");
    compacting.send("\r").unwrap();
    compacting
        .expect("Messages removed")
        .unwrap_or_else(|error| {
            panic!(
                "own context compacts while peer tool is open: {error}; {}",
                common::screen_tail(&compacting, 8000)
            )
        });
    common::expect_input_line_cleared(&compacting, env.timeout(), "compacted context ready");
    let compacted = Session::load_from_path(&transcript).unwrap();
    assert!(compacted.compaction.is_some());
    assert!(compacted.messages().len() < seed.messages().len());
    for message in compacted.messages().iter().skip(1) {
        assert!(
            original_ids.contains(&message.history.as_ref().unwrap().id),
            "retained message identity changed"
        );
    }
    assert!(
        !root.join("B.effect").exists(),
        "peer tool must still be open"
    );
    exit_cli(&env, &mut compacting);
    fs::write(root.join("B.release"), "complete original peer context").unwrap();
    working
        .expect("Concurrency batch done.")
        .expect("peer completes its original tool");
    exit_cli(&env, &mut working);
    let selected = Session::load_from_path(&transcript).unwrap();
    assert!(
        selected.compaction.is_none(),
        "peer inherited another runtime's compaction"
    );
    assert!(selected.messages().len() > seed.messages().len());
    let durable = fs::read_to_string(&transcript).unwrap();
    assert!(durable.contains("SEED_DECISION_0") && durable.contains("\"type\":\"projection\""));
    assert_eq!(
        fs::read_to_string(root.join("B.effect")).unwrap(),
        "B durable effect"
    );
}

#[test]
fn interrupted_final_records_recover_without_changing_committed_history() {
    for mode in ["partial-json", "partial-utf8", "complete-without-newline"] {
        let env = shared_mock(&format!("shared-sid-recover-{mode}"));
        let transcript = env.workspace_root().join("shared.jsonl");
        save_seed(
            &Session::new().with_workspace_root(env.workspace_root()),
            &transcript,
        );
        run_print_turn(&env, &transcript, "BASELINE_COMMITTED_TURN");
        run_print_turn(&env, &transcript, "BEFORE_CRASH");
        let committed = fs::read(&transcript).unwrap();
        let mut interrupted = committed.clone();
        match mode {
            "partial-json" => {
                interrupted.extend_from_slice(br#"{"type":"message","message":{"blocks":["#);
            }
            "partial-utf8" => {
                interrupted.extend_from_slice(b"{\"type\":\"message\",\"text\":\"\xe4");
            }
            _ => {
                assert_eq!(interrupted.pop(), Some(b'\n'));
            }
        }
        fs::write(&transcript, &interrupted).unwrap();
        let inspected = Session::load_from_path(&transcript).unwrap();
        assert_ne!(inspected.messages(), []);
        assert_eq!(
            fs::read(&transcript).unwrap(),
            interrupted,
            "read-only inspection changed the transcript"
        );
        run_print_turn(&env, &transcript, "AFTER_CRASH");
        let recovered = fs::read(&transcript).unwrap();
        let retained_prefix = if mode == "complete-without-newline" {
            let last_frame = committed[..committed.len() - 1]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .unwrap()
                + 1;
            &committed[..last_frame]
        } else {
            committed.as_slice()
        };
        assert!(
            recovered.starts_with(retained_prefix),
            "committed history changed: {mode}"
        );
        let recovered = String::from_utf8(recovered).unwrap();
        for line in recovered.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
        let selected = Session::load_from_path(&transcript).unwrap();
        let mut expected = vec!["BASELINE_COMMITTED_TURN", "AFTER_CRASH"];
        if mode != "complete-without-newline" {
            expected.push("BEFORE_CRASH");
        }
        for marker in expected {
            assert!(
                selected
                    .messages()
                    .iter()
                    .flat_map(|message| &message.blocks)
                    .any(|block| {
                        matches!(block, ContentBlock::Text { text } if text.contains(marker))
                    }),
                "durable context lost: {mode} / {marker}"
            );
        }
    }
}

#[test]
fn automatic_pruning_records_a_new_view_and_preserves_the_full_source_result() {
    let env = shared_mock("shared-sid-pruning");
    let config_path = env.config_home().join("sudocode.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["models"]["claude-sonnet"]["contextWindow"] = json!(32_000);
    config["models"]["claude-sonnet"]["maxOutputTokens"] = json!(8_192);
    fs::write(config_path, config.to_string()).unwrap();
    let transcript = env.workspace_root().join("shared.jsonl");
    let mut seed = Session::new().with_workspace_root(env.workspace_root());
    seed.push_user_text("Retain the FULL_RESULT_SENTINEL decisions")
        .unwrap();
    seed.push_message(tool_call()).unwrap();
    let full = format!(
        "FULL_RESULT_SENTINEL {} END_RESULT_SENTINEL",
        "x".repeat(160_000)
    );
    seed.push_message(ConversationMessage::tool_result(
        "call_1",
        "read_file",
        &full,
        false,
    ))
    .unwrap();
    save_seed(&seed, &transcript);
    run_print_turn(&env, &transcript, "CONTINUE_AFTER_PRUNING");
    let selected = Session::load_from_path(&transcript).unwrap();
    let pruned = selected.messages().iter().find(|message| message.blocks.iter().any(|block| {
        matches!(block, ContentBlock::ToolResult { output, .. } if output.contains("tool result middle pruned"))
    })).expect("selected context must retain its pruned result");
    let result_origin = &pruned.history.as_ref().unwrap().tool_origins["call_1"];
    assert!(selected.messages().iter().any(|message| {
        message
            .history
            .as_ref()
            .is_some_and(|identity| &identity.id == result_origin)
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == "call_1"))
    }));
    let raw = fs::read_to_string(&transcript).unwrap();
    let mut source_ids = Vec::new();
    for line in raw.lines() {
        let record: Value = serde_json::from_str(line).unwrap();
        for message in record["event"]["session"]["messages"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if message["blocks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|block| block["output"].as_str() == Some(&full))
            {
                source_ids.push(message["history"]["id"].as_str().unwrap().to_owned());
            }
        }
    }
    assert!(
        !source_ids.is_empty(),
        "full pre-pruning output must remain durable"
    );
    assert!(
        !source_ids.contains(&pruned.history.as_ref().unwrap().id),
        "derived payload reused the source identity"
    );
}

fn seed_interleaved_tool_history(path: &std::path::Path, root: &std::path::Path) {
    let metadata: Value = serde_json::from_str(
        &Session::new()
            .with_workspace_root(root)
            .to_json()
            .unwrap()
            .render(),
    )
    .unwrap();
    fs::write(path, format!("{}\n", json!({"type":"history_checkpoint","history_version":1,"id":"seed","parent_id":null,"meta":metadata,"messages":[]}))).unwrap();
    let messages = [
        identified(
            ConversationMessage::user_text("Read A"),
            "a-user",
            Some("seed"),
            None,
        ),
        identified(
            ConversationMessage::user_text("Read B"),
            "b-user",
            Some("seed"),
            None,
        ),
        identified(tool_call(), "a-assistant", Some("a-user"), None),
        identified(tool_call(), "b-assistant", Some("b-user"), None),
        identified(
            ConversationMessage::tool_result("call_1", "read_file", "A_PRIVATE_RESULT", false),
            "a-result",
            Some("a-assistant"),
            Some("a-assistant"),
        ),
        identified(
            ConversationMessage::tool_result("call_1", "read_file", "B_SELECTED_RESULT", false),
            "b-result",
            Some("b-assistant"),
            Some("b-assistant"),
        ),
    ];
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    for message in &messages {
        let payload: Value = serde_json::from_str(&message.to_json().render()).unwrap();
        writeln!(file, "{}", json!({"type":"message","message":payload})).unwrap();
    }
    drop(file);
}
