//! A machine's node record decides where a session's peers are.
//!
//! `scode` reads `<home>/.nexus/run/node.json` to find this machine's Nexus
//! node. No record means the disk namespace. A valid record and credential
//! keep the node namespace even while that node is offline: ordinary startup
//! and resume still work, while a send reports its actual node error. An
//! record or credential that cannot be loaded is still a startup error in
//! this slice. A loadable but untrusted certificate is a different condition.
//!
//! Real CLI processes exercise both cases, including persisted history and a
//! real failed send. No case may silently select a disk mailbox.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

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
fn an_unusable_node_record_stops_the_session_and_never_falls_back_to_disk() {
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
        let home = home_with_record(&env, &make_record(&env));
        let home = home.to_string_lossy().into_owned();
        let mut session = env.spawn_with_env(
            &["--print", "hello"],
            &[("HOME", home.as_str()), ("USERPROFILE", home.as_str())],
        );
        session.set_default_timeout(Duration::from_secs(30));
        session.expect(expected).unwrap_or_else(|e| {
            panic!("[{label}] the session must name the cause {expected:?}: {e}")
        });
        let code = session
            .expect_eof()
            .unwrap_or_else(|e| panic!("[{label}] the session must stop: {e}"));
        assert_ne!(
            code, 0,
            "[{label}] an unusable node record must fail the session"
        );
        let pair_root = runtime::mailbox::local_pair_root_in(env.config_home());
        assert!(
            !pair_root.exists(),
            "[{label}] the session fell back to the disk namespace at {}",
            pair_root.display()
        );
    }
}

/// Both renderers must start and restore a real completed turn without a node.
#[test]
fn an_offline_node_does_not_block_startup_or_resume() {
    for mode in ["off", "queue"] {
        let env = TestEnv::new("offline-node-resume");
        node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
        let agents = empty_agents_dir(&env);
        node_home::write_untrusted_credential(&agents, SESSION_AGENT);
        let home = home_with_record(&env, &record("127.0.0.1:1", &agents));
        let home = home.to_string_lossy();
        let vars = [
            ("HOME", home.as_ref()),
            ("USERPROFILE", home.as_ref()),
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", mode),
        ];
        let prompt = env.prompt("Reply with exactly OFFLINE_NODE_REPLY.", "single_turn_text");
        let reply = if env.is_mock() {
            "The answer is 4"
        } else {
            "OFFLINE_NODE_REPLY"
        };
        let mut fresh = env.spawn_with_env(&["--permission-mode", "read-only"], &vars);
        fresh.set_default_timeout(common::at_least(Duration::from_secs(30)));
        common::expect_input_line_cleared(&fresh, Duration::from_secs(30), "offline fresh");
        fresh
            .send(&format!("{prompt}\r"))
            .expect("send ordinary prompt");
        fresh
            .expect(reply)
            .expect("ordinary turn completes without Nexus");
        wait_for_saved_reply(&env, reply);
        common::expect_input_line_cleared(&fresh, Duration::from_secs(30), "offline turn done");
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
        common::expect_input_line_cleared(&resumed, Duration::from_secs(30), "offline resume");
        resumed.send("/exit\r").expect("exit resumed session");
        assert_eq!(resumed.expect_eof().expect("resumed session exits"), 0);
        assert_no_disk_namespace(&env);
    }
}

/// Reaching the prompt must not disguise a node failure as a local delivery.
#[test]
fn an_offline_node_send_fails_on_the_node_without_creating_a_disk_inbox() {
    let env = TestEnv::new("offline-node-send");
    node_home::pin_agent_name(env.workspace_root(), SESSION_AGENT);
    let agents = empty_agents_dir(&env);
    node_home::write_untrusted_credential(&agents, SESSION_AGENT);
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
        .messages
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
        results[0].0.contains("connect"),
        "must reach the node transport: {results:?}"
    );
    assert_no_disk_namespace(&env);
}

/// Do not accept the user's echoed prompt as evidence of a model reply.
fn wait_for_saved_reply(env: &TestEnv, expected: &str) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(path) =
            common::find_session_transcript(&env.workspace_root().join(".scode/sessions"))
        {
            if let Ok(saved) = runtime::Session::load_from_path(&path) {
                let replied = saved.messages.iter().any(|message| {
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
