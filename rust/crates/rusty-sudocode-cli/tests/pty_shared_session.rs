//! Restore interleaved same-SID history through the actual CLI and model wire.
mod common;

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
    let metadata: Value = serde_json::from_str(
        &Session::new()
            .with_workspace_root(root)
            .to_json()
            .unwrap()
            .render(),
    )
    .unwrap();
    fs::write(&path, format!("{}\n", json!({"type":"history_checkpoint","history_version":1,"id":"seed","parent_id":null,"meta":metadata,"messages":[]}))).unwrap();
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
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    for message in &messages {
        let payload: Value = serde_json::from_str(&message.to_json().render()).unwrap();
        writeln!(file, "{}", json!({"type":"message","message":payload})).unwrap();
    }
    drop(file);
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
    cli.expect("The answer is 4").expect("resumed model answer");
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
    wait_for_file_with_budget(path, Duration::from_secs(20));
}

fn wait_for_file_with_budget(path: &std::path::Path, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "provider did not reach barrier {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exit_cli(env: &TestEnv, cli: &mut PtySession) {
    common::expect_input_line_cleared(cli, env.timeout(), "completed runtime accepts input");
    cli.send("/exit").unwrap();
    common::expect_input_line(cli, "/exit", env.timeout(), "exit entered");
    cli.send("\r").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
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
            cli.render(|screen| screen.contents())
        )
    });
    assert_eq!(cli.expect_eof().unwrap(), 0);
}

#[test]
fn independent_cli_processes_append_to_the_same_sid_while_both_turns_are_open() {
    run_two_cli_contexts(false, false);
}

#[test]
fn resuming_during_another_turn_inherits_its_written_prefix() {
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
            "late runtime restored the written prefix",
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
        &second,
        "SECOND_CONTEXT_PROMPT",
        env.timeout(),
        "second prompt entered",
    );
    second.send("\r").unwrap();
    wait_for_file(&second_ready);
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
                messages.contains("FIRST_CONTEXT_PROMPT"),
                "a later resume must inherit the already written user message"
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
        r#"
const fs = require('fs');
const label = process.argv[2];
fs.writeFileSync(label + '.ready', 'tool running');
const timer = setInterval(() => {
  if (!fs.existsSync(label + '.release')) return;
  clearInterval(timer);
  fs.appendFileSync(label + '.effect', label + ' durable effect');
  process.stdout.write((label + '_OUTPUT_').repeat(6000));
}, 10);
"#,
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
    // This barrier includes the provider's response, unlike later local-file
    // effects. Reuse the same budget as the other waits for this backend.
    wait_for_file_with_budget(
        &env.workspace_root().join(format!("{label}.ready")),
        env.timeout(),
    );
}

#[test]
fn concurrent_tools_with_the_same_provider_id_keep_both_effects_and_full_outputs() {
    let env = shared_mock("shared-sid-concurrent-tools");
    run_two_context_tools(&env);
}

#[test]
#[ignore = "requires SCODE_TEST_BACKEND=live and a funded selected provider"]
fn live_independent_runtimes_preserve_tools_and_outputs_in_one_session() {
    assert_eq!(std::env::var("SCODE_TEST_BACKEND").as_deref(), Ok("live"));
    let env = TestEnv::new("shared-sid-live-concurrent-tools");
    enable_shared_history(&env);
    run_two_context_tools(&env);
}

fn run_two_context_tools(env: &TestEnv) {
    let root = env.workspace_root();
    let transcript = root.join("shared.jsonl");
    let seed = Session::new().with_workspace_root(root);
    let sid = seed.session_id.clone();
    save_seed(&seed, &transcript);
    write_gate(root);
    let variables = [];
    let args = [
        "--resume",
        transcript.to_str().unwrap(),
        "--permission-mode",
        "danger-full-access",
    ];
    let mut first = env.spawn_with_env(&args, &variables);
    common::expect_input_line_cleared(&first, env.timeout(), "first tool runtime ready");
    let mut second = env.spawn_with_env(&args, &variables);
    common::expect_input_line_cleared(&second, env.timeout(), "second tool runtime ready");
    for (cli, label) in [(&mut first, "A"), (&mut second, "B")] {
        begin_gate(env, cli, label);
    }
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
    fs::write(root.join("A.release"), "release A").unwrap();
    wait_for_file(&root.join("A.effect"));
    first
        .expect("Concurrency batch done.")
        .expect("A completed its actual tool roundtrip");
    assert_eq!(
        fs::read_to_string(root.join("A.effect")).unwrap(),
        "A durable effect"
    );
    for cli in [&mut first, &mut second] {
        exit_cli(env, cli);
    }
    assert_gate_outputs(&transcript, &sid, env.is_mock().then_some("call_1"));
}

#[test]
fn idle_runtime_preserves_continuation_and_prevents_session_deletion() {
    let env = shared_mock("shared-sid-idle-close");
    let session = Session::new().with_workspace_root(env.workspace_root());
    let store = runtime::SessionStore::from_cwd(env.workspace_root()).unwrap();
    let transcript = store.create_handle(&session.session_id).path;
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
    let controller = env.workspace_root().join("delete-controller.jsonl");
    Session::new()
        .with_workspace_root(env.workspace_root())
        .save_to_path(&controller)
        .unwrap();
    let delete = || {
        let mut cli = env.spawn_with_env(&["--resume", controller.to_str().unwrap()], &[]);
        common::expect_input_line_cleared(&cli, env.timeout(), "deletion controller ready");
        cli.send(&format!("/session delete {} --force\r", session.session_id))
            .unwrap();
        cli
    };
    let mut rejected = delete();
    rejected
        .expect("session_writer_busy")
        .unwrap_or_else(|error| {
            panic!(
                "live runtime deletion: {error}\n{}",
                rejected.render(|s| s.contents())
            )
        });
    exit_cli(&env, &mut rejected);
    assert!(transcript.exists());
    exit_cli(&env, &mut idle);
    let selected = Session::load_from_path(&transcript).unwrap();
    assert!(selected.messages().iter().flat_map(|message| &message.blocks).any(|block| {
        matches!(block, ContentBlock::Text { text } if text.contains("LATEST_ACTIVE_CONTEXT"))
    }), "idle metadata changed the latest resume context");
    let mut accepted = delete();
    accepted
        .expect("Session deleted")
        .expect("deletion succeeds after the runtime exits");
    exit_cli(&env, &mut accepted);
    assert!(!transcript.exists());
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
                interrupted.extend_from_slice(br#"{"type":"message","message":{"blocks":["#)
            }
            "partial-utf8" => {
                interrupted.extend_from_slice(b"{\"type\":\"message\",\"text\":\"\xe4")
            }
            _ => {
                assert_eq!(interrupted.pop(), Some(b'\n'));
            }
        }
        fs::write(&transcript, &interrupted).unwrap();
        let inspected = Session::load_from_path(&transcript).unwrap();
        assert!(!inspected.messages().is_empty());
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
