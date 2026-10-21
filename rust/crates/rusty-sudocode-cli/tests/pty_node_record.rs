//! A machine's node record decides where a session's peers are.
//!
//! `scode` reads `<home>/.nexus/run/node.json` to find this machine's Nexus
//! node. No record means the disk namespace. A record that is present but
//! unusable means the machine has joined a node the session cannot reach, so
//! the session must stop and say why: quietly using the disk namespace instead
//! would deliver every message to an inbox no peer on the node ever reads.
//!
//! Each case here starts a real `scode` whose home holds one broken record, or
//! a good record for a node that does not answer, and checks two things: the
//! specific cause is reported, and the disk namespace was never created.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

use std::path::{Path, PathBuf};
use std::time::Duration;

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
    let cases: [Case; 7] = [
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
        (
            "node does not answer",
            |env| {
                let agents = empty_agents_dir(env);
                node_home::write_unusable_credential(&agents, SESSION_AGENT);
                // Port 1 on loopback: nothing listens, so the dial is refused.
                record("127.0.0.1:1", &agents)
            },
            "is not usable as 'node-record-probe': reading /agents failed",
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

/// The agent name every case's session answers to.
const SESSION_AGENT: &str = "node-record-probe";
