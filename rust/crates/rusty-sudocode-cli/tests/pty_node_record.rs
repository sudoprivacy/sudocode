//! A machine's node record decides where a session's peers are.
//!
//! `scode` reads `<home>/.nexus/run/node.json` to find this machine's Nexus
//! node. No record means the disk namespace. A present but unavailable record,
//! credential or node keeps the node namespace: ordinary startup and resume
//! still work, while A2A reports its actual error and the same receiver retries.
//!
//! Real CLI processes exercise both cases, including persisted history and a
//! real failed send. No case may silently select a disk mailbox.

mod common;
#[cfg(unix)]
#[path = "support/freeze.rs"]
mod freeze;
#[path = "support/node_home.rs"]
mod node_home;

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::TestEnv;

/// A home under the test's workspace holding `record` as its node record.
fn home_with_record(env: &TestEnv, record: &str) -> PathBuf {
    let home = env.workspace_root().join("node-home");
    let run = home.join(".nexus").join("run");
    std::fs::create_dir_all(&run).expect("create the node record directory");
    std::fs::write(run.join("node.json"), record).expect("write node.json");
    home
}

/// An absolute `agents_dir` that exists but holds no credential.
fn empty_agents_dir(env: &TestEnv) -> PathBuf {
    let dir = env.workspace_root().join("node-agents");
    std::fs::create_dir_all(&dir).expect("create the agents directory");
    dir
}

fn record(endpoint: &str, agents_dir: &Path) -> String {
    serde_json::json!({
        "version": 1,
        "endpoint": endpoint,
        "agents_dir": agents_dir.to_string_lossy(),
        "role": "founder",
        "zone": "sharedzone",
    })
    .to_string()
}

#[test]
fn unusable_node_configuration_leaves_the_session_usable_and_a2a_explicitly_unavailable() {
    type Case = (&'static str, fn(&TestEnv) -> String, &'static str);
    let cases: [Case; 6] = [
        (
            "malformed",
            |_| "{not json".to_string(),
            "parse node record",
        ),
        (
            "unsupported version",
            |env| {
                let mut value: serde_json::Value =
                    serde_json::from_str(&record("127.0.0.1:1", &empty_agents_dir(env))).unwrap();
                value["version"] = serde_json::json!(2);
                value.to_string()
            },
            "this scode reads version 1",
        ),
        (
            "endpoint with a scheme",
            |env| record("https://127.0.0.1:1", &empty_agents_dir(env)),
            "endpoint must be host:port without a scheme",
        ),
        (
            "relative agents_dir",
            |_| record("127.0.0.1:1", Path::new("relative/agents")),
            "agents_dir must be an absolute path",
        ),
        (
            "no credential for this agent",
            |env| record("127.0.0.1:1", &empty_agents_dir(env)),
            "ask the node owner to mint",
        ),
        (
            "credential minted for another agent",
            |env| {
                let agents = empty_agents_dir(env);
                let bundle = agents.join(SESSION_AGENT);
                std::fs::create_dir_all(&bundle).unwrap();
                std::fs::write(
                    bundle.join("credential.json"),
                    r#"{"version":1,"agent":"someone-else","server_name":"nexus-node","ca":"ca.pem","cert":"agent.pem","key":"agent-key.pem"}"#,
                )
                .unwrap();
                record("127.0.0.1:1", &agents)
            },
            "is for agent 'someone-else'",
        ),
    ];
    for (label, make_record, expected) in cases {
        let env = TestEnv::new("node-record");
        node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
        std::fs::write(
            env.config_home().join("runtime-profile.json"),
            r#"{"schemaVersion":1,"mode":"shared"}"#,
        )
        .unwrap();
        let home = home_with_record(&env, &make_record(&env));
        ordinary_task_with_unavailable_a2a(&env, &home, label, expected);
    }
}

fn ordinary_task_with_unavailable_a2a(env: &TestEnv, home: &Path, label: &str, expected: &str) {
    let home = home.to_string_lossy();
    let services = env.workspace_root().join("unexpected-services");
    let services_env = services.to_string_lossy();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("HOME", home.as_ref()),
            ("USERPROFILE", home.as_ref()),
            ("SCODE_SERVICE_ROOT", services_env.as_ref()),
            ("PAGER", ""),
        ],
    );
    session.set_default_timeout(common::at_least(Duration::from_secs(30)));
    common::expect_input_line_cleared(&session, Duration::from_secs(30), label);
    session.send("/status\r").expect("inspect A2A status");
    // /status is rendered chrome. ConPTY can wrap a long credential path
    // between words in the cause, and redraws need not follow a stream cursor.
    // Require both facts on the actual screen; preserve the full cause check
    // and report that screen if either fact is missing.
    common::expect_screen(
        &session,
        |screen| {
            common::screen_contains(screen, "node (unavailable:")
                && common::screen_contains(screen, expected)
        },
        Duration::from_secs(30),
        &format!("[{label}] selected node namespace and cause {expected:?}"),
    );
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "status done");
    let prompt = env.prompt(
        "Reply with exactly NODE_CONFIG_TASK_OK.",
        "single_turn_text",
    );
    let reply = if env.is_mock() {
        "The answer is 4"
    } else {
        "NODE_CONFIG_TASK_OK"
    };
    session
        .send(&format!("{prompt}\r"))
        .expect("ordinary task while A2A unavailable");
    session.expect(reply).expect("ordinary task completes");
    wait_for_saved_reply(env, reply);
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "ordinary task done");
    session.send("/exit\r").expect("normal exit");
    assert_eq!(
        session.expect_eof().expect("session exits normally"),
        0,
        "[{label}]"
    );
    assert_no_disk_namespace(env);
    assert!(
        !services.exists(),
        "[{label}] selected Box must not be replaced"
    );
}

/// Both renderers must start and restore a real completed turn without a node.
#[test]
fn an_offline_node_does_not_block_startup_or_resume() {
    startup_and_resume_with_credential(true);
}

#[test]
fn an_unminted_agent_can_start_and_resume_without_a_disk_fallback() {
    startup_and_resume_with_credential(false);
}

fn startup_and_resume_with_credential(has_credential: bool) {
    for mode in ["off", "queue"] {
        let env = TestEnv::new("offline-node-resume");
        node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
        let agents = empty_agents_dir(&env);
        if has_credential {
            node_home::write_untrusted_credential(&agents, SESSION_AGENT);
        }
        let home = home_with_record(&env, &record("127.0.0.1:1", &agents));
        let home = home.to_string_lossy();
        let log = env.workspace_root().join("receiver-diagnostics.jsonl");
        let log_text = log.to_string_lossy();
        // A saved multi-agent choice must keep this Box rather than provision a
        // second service when the configured one is temporarily unreachable.
        seed_shared_runtime_profile(&env);
        let services = env.workspace_root().join("unexpected-services");
        let services_env = services.to_string_lossy();
        let vars = [
            ("HOME", home.as_ref()),
            ("USERPROFILE", home.as_ref()),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", mode),
            ("SCODE_LOG_PATH", log_text.as_ref()),
            ("SCODE_SERVICE_ROOT", services_env.as_ref()),
            ("PAGER", ""),
        ];
        let prompt = env.prompt("Reply with exactly OFFLINE_NODE_REPLY.", "single_turn_text");
        let reply = if env.is_mock() {
            "The answer is 4"
        } else {
            "OFFLINE_NODE_REPLY"
        };
        let mut fresh = env.spawn_with_env(&["--permission-mode", "read-only"], &vars);
        fresh.set_default_timeout(common::at_least(Duration::from_secs(30)));
        common::expect_input_line_cleared(
            &fresh,
            Duration::from_secs(30),
            &format!("offline {mode} fresh"),
        );
        expect_receiver_diagnostic(&mut fresh, receiver_failure_message(has_credential));
        check_draft(&mut fresh);
        fresh
            .send(&format!("{prompt}\r"))
            .expect("send ordinary prompt");
        fresh
            .expect(reply)
            .expect("ordinary turn completes without Nexus");
        wait_for_saved_reply(&env, reply);
        common::expect_input_line_cleared(
            &fresh,
            Duration::from_secs(30),
            &format!("offline {mode} turn done"),
        );
        // Startup/model input must be usable before the receiver finishes IO.
        // Keep this live process open until its actual background failure is
        // logged; Windows can complete the fast mock turn before connect fails.
        wait_for_receiver_diagnostic(&log, has_credential);
        common::expect_input_line_cleared(
            &fresh,
            Duration::from_secs(30),
            &format!("offline {mode} diagnostic shown"),
        );
        fresh.send("/exit\r").expect("exit fresh session");
        assert_eq!(fresh.expect_eof().expect("fresh session exits"), 0);

        let path = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
            .expect("completed turn persisted");
        let mut resumed = env.spawn_with_env(
            &[
                "--resume",
                path.to_str().unwrap(),
                "--permission-mode",
                "read-only",
            ],
            &vars,
        );
        resumed.set_default_timeout(common::at_least(Duration::from_secs(30)));
        resumed
            .expect(reply)
            .expect("restored reply from original transcript");
        common::expect_input_line_cleared(
            &resumed,
            Duration::from_secs(30),
            &format!("offline {mode} resume"),
        );
        expect_receiver_diagnostic(&mut resumed, receiver_failure_message(has_credential));
        check_draft(&mut resumed);
        resumed.send("/exit\r").expect("exit resumed session");
        assert_eq!(resumed.expect_eof().expect("resumed session exits"), 0);
        assert_no_disk_namespace(&env);
        assert!(
            !services.exists(),
            "offline Box must not start another Nexus"
        );
    }
}

/// This is an observation phase after the original startup/turn readiness
/// assertions, not an increased budget for those assertions. A node operation
/// has its own transport deadline; do not exit the child before observing it.
fn wait_for_receiver_diagnostic(log: &Path, has_credential: bool) {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let records: Vec<serde_json::Value> = std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["attributes"]["namespace"] == "background_retry")
            .collect();
        if records.iter().any(|record| {
            let props = &record["attributes"];
            props["subject"] == SESSION_AGENT
                && props["scope"] == "presence announcement"
                && props["error"].as_str().is_some_and(|error| {
                    if has_credential {
                        error.contains("announce") && error.contains("connect")
                    } else {
                        error.contains("ask the node owner to mint")
                    }
                })
        }) {
            return;
        }
        assert!(Instant::now() < deadline,
            "no presence diagnostic while the original CLI stayed alive; background records: {records:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read-only status commands observe the real failed connection. A bounded
/// wait permits the background RPC to complete without a scheduling sleep.
fn expect_receiver_diagnostic(session: &mut pty_expect::PtySession, cause: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let screen = read_receiver_status(session);
        if screen.contains("Last RX error")
            && common::screen_contains(&screen.to_ascii_lowercase(), cause)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "receiver failure {cause:?} was not available in status: {screen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read_receiver_status(session: &mut pty_expect::PtySession) -> String {
    session
        .send("/status")
        .expect("type receiver status command");
    common::expect_screen_settled(
        session,
        |screen| common::input_line_of(screen) == "/status",
        Duration::from_secs(30),
        "receiver status command reached the input",
    );
    session.send("\r").expect("inspect receiver status");
    let screen = common::expect_screen_settled(
        session,
        |screen| {
            screen.contains("Peers")
                && screen.contains('❯')
                && common::input_line_of(screen).is_empty()
        },
        Duration::from_secs(30),
        "receiver status returns to an empty prompt",
    );
    assert!(
        !screen.contains("[nexus-a2a]"),
        "background receiver wrote directly into the terminal: {screen}"
    );
    screen
}

/// Background retries must leave the editable line usable in both renderers.
fn check_draft(session: &mut pty_expect::PtySession) {
    let draft = "OFFLINE_DRAFT_中文";
    session.send(draft).expect("type while node is offline");
    common::expect_screen_settled(
        session,
        |screen| common::input_line_of(screen) == draft,
        Duration::from_secs(30),
        "offline receiver does not corrupt the draft",
    );
    session.send("\x15").expect("clear offline draft");
    common::expect_input_line_cleared(session, Duration::from_secs(30), "offline draft cleared");
}

/// Reaching the prompt must not disguise a node failure as a local delivery.
#[test]
fn an_offline_node_send_fails_on_the_node_without_creating_a_disk_inbox() {
    send_fails_without_disk(true, "connect");
}

#[test]
fn an_unminted_agents_send_reports_the_missing_bundle_and_never_writes_to_disk() {
    send_fails_without_disk(false, "ask the node owner to mint");
}

#[test]
fn the_original_receiver_retries_a_new_bundle_without_a_restart_or_tool_call() {
    let env = TestEnv::new_mock("pending-node-receiver");
    node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
    let agents = empty_agents_dir(&env);
    let home = home_with_record(&env, &record("127.0.0.1:1", &agents));
    let home = home.to_string_lossy();
    let log = env.workspace_root().join("receiver-diagnostics.jsonl");
    let log_text = log.to_string_lossy();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "workspace-write"],
        &[
            ("HOME", home.as_ref()),
            ("USERPROFILE", home.as_ref()),
            ("PAGER", ""),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SCODE_LOG_PATH", log_text.as_ref()),
        ],
    );
    session.set_default_timeout(common::at_least(Duration::from_secs(30)));
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "pending receiver");
    wait_for_receiver_diagnostic(&log, false);

    // No tool call or second CLI process triggers resolution. The original
    // receiver must pick up the new files and reach the real offline transport.
    node_home::write_untrusted_credential(&agents, SESSION_AGENT);
    wait_for_receiver_diagnostic(&log, true);
    session.send("/status\r").expect("inspect the same binding");
    session
        .expect("node 127.0.0.1:1")
        .expect("receiver resolved the selected node binding");
    session
        .expect("unavailable; ordinary session remains usable")
        .expect("offline is not falsely reported as another receiver");
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "resolved binding");

    let prompt = env.prompt(
        "Use send once to message team-lead: NEW_NODE_OPERATION. Report its error; do not retry.",
        "unified_send_roundtrip",
    );
    session.send(&format!("{prompt}\r")).expect("one new send");
    wait_for_saved_reply(&env, "unified send roundtrip complete");
    common::expect_input_line_cleared(&session, Duration::from_secs(30), "node send done");
    session.send("/exit\r").expect("normal exit");
    assert_eq!(session.expect_eof().expect("original process exits"), 0);
    assert_send_error(&env, "connect");
    assert_no_disk_namespace(&env);
}

fn send_fails_without_disk(has_credential: bool, expected: &str) {
    let env = TestEnv::new("offline-node-send");
    node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
    let agents = empty_agents_dir(&env);
    if has_credential {
        node_home::write_untrusted_credential(&agents, SESSION_AGENT);
    }
    let home = home_with_record(&env, &record("127.0.0.1:1", &agents));
    let home = home.to_string_lossy();
    let prompt = env.prompt(
        "Use send once to message team-lead: OFFLINE_NODE_SEND. Report its error; do not retry.",
        "unified_send_roundtrip",
    );
    let mut session = env.spawn_with_env(
        &["--print", "--permission-mode", "workspace-write", &prompt],
        &[("HOME", home.as_ref()), ("USERPROFILE", home.as_ref())],
    );
    session.set_default_timeout(common::at_least(Duration::from_secs(30)));
    assert_eq!(session.expect_eof().expect("ordinary task finishes"), 0);
    assert_send_error(&env, expected);
    assert_no_disk_namespace(&env);
}

fn assert_send_error(env: &TestEnv, expected: &str) {
    let path = common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        .expect("send result persisted");
    let saved = runtime::Session::load_from_path(&path).expect("load actual CLI history");
    assert_eq!(
        saved
            .workspace_root()
            .map(|path| path.canonicalize().unwrap()),
        Some(env.workspace_root().canonicalize().unwrap()),
        "the actual child session must use the private test workspace"
    );
    let results: Vec<_> = saved
        .messages()
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|block| {
            if let runtime::ContentBlock::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            } = block
            {
                (tool_name == "send").then_some((output, is_error))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        results.len(),
        1,
        "exactly one real send result: {results:?}"
    );
    assert!(
        *results[0].1,
        "offline send must not report success: {results:?}"
    );
    assert!(
        results[0].0.contains(expected),
        "must report the selected node's actual error {expected:?}: {results:?}"
    );
}

/// Do not accept the user's echoed prompt as evidence of a model reply.
fn wait_for_saved_reply(env: &TestEnv, expected: &str) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(path) =
            common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        {
            if let Ok(saved) = runtime::Session::load_from_path(&path) {
                let replied = saved.messages().iter().any(|message| {
                    message.role == runtime::MessageRole::Assistant
                        && message.blocks.iter().any(|block| {
                            matches!(block, runtime::ContentBlock::Text { text } if text.contains(expected))
                        })
                });
                if replied {
                    return path;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no durable assistant reply containing {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_no_disk_namespace(env: &TestEnv) {
    node_home::assert_no_disk_mailboxes_below(env.workspace_root());
    for root in [
        runtime::mailbox::local_pair_root_in(env.config_home()),
        env.workspace_root().join(".sudocode-inbox"),
        env.workspace_root().join("conversations"),
    ] {
        assert!(
            !root.exists(),
            "unexpected disk namespace at {}",
            root.display()
        );
    }
}

/// The agent name every case's session answers to.
const SESSION_AGENT: &str = "node-record-probe";

/// Hold the real transport open until a draft is visible, then fail it.
/// A subsequent connection exercises retries while input is active. Editing
/// and the status report must remain usable as the transport failure arrives.
#[test]
fn receiver_failure_during_typing_keeps_the_draft_intact() {
    for mode in ["off", "queue"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let env = TestEnv::new_mock("offline-node-draft");
        node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
        let agents = empty_agents_dir(&env);
        node_home::write_untrusted_credential(&agents, SESSION_AGENT);
        let endpoint = listener.local_addr().unwrap().to_string();
        let home = home_with_record(&env, &record(&endpoint, &agents));
        let home = home.to_string_lossy();
        let mut session = env.spawn_with_env(
            &["--permission-mode", "read-only"],
            &[
                ("HOME", home.as_ref()),
                ("USERPROFILE", home.as_ref()),
                ("SUDOCODE_INTERRUPT_QUEUE_MODE", mode),
                ("PAGER", ""),
            ],
        );
        let first = accept_connection(&listener);
        common::expect_input_line_cleared(
            &session,
            Duration::from_secs(30),
            "prompt while the node transport is pending",
        );
        let draft = "DRAFT_BEFORE_NODE_FAILURE_中文";
        session.send(draft).unwrap();
        common::expect_screen_settled(
            &session,
            |screen| common::input_line_of(screen) == draft,
            Duration::from_secs(30),
            "draft before the transport failure",
        );
        drop(first);
        let next = accept_connection(&listener);
        common::expect_screen_settled(
            &session,
            |screen| common::input_line_of(screen) == draft,
            Duration::from_secs(30),
            "draft after the receiver handled the transport failure",
        );
        drop(next);
        drop(listener);
        session.send("\x15").unwrap();
        common::expect_input_line_cleared(
            &session,
            Duration::from_secs(30),
            "draft clears after node failure",
        );
        expect_receiver_diagnostic(&mut session, "connect");
        session.send("/exit\r").unwrap();
        assert_eq!(session.expect_eof().unwrap(), 0);
        assert_no_disk_namespace(&env);
        assert_eq!(env.captured_message_count(), 0);
    }
}

fn accept_connection(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "receiver never connected");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept node transport: {error}"),
        }
    }
}

/// Pausing a real node receiver lets its remote lease expire without killing
/// its terminal. A peer takes over before the original process resumes.
#[cfg(unix)]
#[test]
#[ignore = "requires a real Nexus daemon and a dedicated receiver credential"]
fn live_node_lease_loss_preserves_editable_draft() {
    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("test daemon endpoint");
    let bundle =
        std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL").expect("dedicated receiver credential");
    let credential = runtime::nexus_mailbox::AgentCredential::load(&bundle)
        .expect("load the receiver's own mTLS credential");
    for mode in ["off", "queue"] {
        let env = TestEnv::new_mock("node-lease-draft");
        node_home::pin_agent_name(env.workspace_root(), &credential.agent);
        let home = node_home::NodeHome::new(
            "lease-draft",
            &endpoint,
            &credential.agent,
            Path::new(&bundle),
        );
        let home_env = home.env();
        let vars: Vec<_> = home_env
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .chain([("SUDOCODE_INTERRUPT_QUEUE_MODE", mode), ("PAGER", "")])
            .collect();
        let mut first = env.spawn_with_env(&["--permission-mode", "read-only"], &vars);
        common::expect_input_line_cleared(
            &first,
            Duration::from_secs(30),
            "original node receiver REPL starts",
        );
        expect_receiver_state(&mut first, "receiving", Duration::from_secs(30));
        let draft = "DRAFT_BEFORE_LEASE_LOSS_中文";
        first.send(draft).unwrap();
        common::expect_screen_settled(
            &first,
            |screen| common::input_line_of(screen) == draft,
            Duration::from_secs(30),
            "draft is visible before the receiver stops renewing",
        );
        let frozen = freeze::freeze(first.process_id().expect("live scode process"))
            .expect("freeze the original lease holder");
        let mut second = env.spawn_with_env(&["--permission-mode", "read-only"], &vars);
        common::expect_input_line_cleared(
            &second,
            Duration::from_secs(30),
            "replacement node receiver REPL starts",
        );
        expect_receiver_state(&mut second, "receiving", Duration::from_secs(90));
        frozen.resume().expect("resume the former lease holder");
        common::expect_screen_settled(
            &first,
            |screen| common::input_line_of(screen) == draft,
            Duration::from_secs(30),
            "original draft survives the receiver handoff",
        );
        first.send("\x15").unwrap();
        common::expect_input_line_cleared(
            &first,
            Duration::from_secs(30),
            "draft clears after the lease handoff",
        );
        expect_receiver_diagnostic(&mut first, "receiver seat lost");
        expect_receiver_state(&mut first, "standing by", Duration::from_secs(30));
        check_draft(&mut first);
        first.send("/exit\r").unwrap();
        assert_eq!(first.expect_eof().unwrap(), 0);
        expect_receiver_state(&mut second, "receiving", Duration::from_secs(30));
        second.send("/exit\r").unwrap();
        assert_eq!(second.expect_eof().unwrap(), 0);
        assert_no_disk_namespace(&env);
        assert_eq!(env.captured_message_count(), 0);
    }
}

#[cfg(unix)]
fn expect_receiver_state(session: &mut pty_expect::PtySession, state: &str, budget: Duration) {
    let needle = format!("Receiver         {state}");
    let deadline = Instant::now() + budget;
    loop {
        let screen = read_receiver_status(session);
        if screen.contains(&needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "receiver never became {state}: {screen}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn seed_shared_runtime_profile(env: &TestEnv) {
    std::fs::write(
        env.config_home().join("runtime-profile.json"),
        r#"{"schemaVersion":1,"mode":"shared"}"#,
    )
    .unwrap();
}

fn receiver_failure_message(has_credential: bool) -> &'static str {
    if has_credential {
        "connect"
    } else {
        "ask the node owner to mint"
    }
}
