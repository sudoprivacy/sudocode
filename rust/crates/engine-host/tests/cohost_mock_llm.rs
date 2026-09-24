//! The co-host, proven on a scripted model — no secrets, no network.
//!
//! `cohost_live_llm` is the real thing and is `#[ignore]`d for it, so until now
//! NOTHING automatic ran a co-hosted agent: nexus's duet workflow no-ops on a
//! missing secret, the A2A live job skips its LLM half for the same reason, and
//! `cargo test` skipped the ignored test. The co-host was verified by compiling.
//!
//! This closes that. The model is `mock-anthropic-service`, which answers a
//! scripted tool call and then a scripted reply, so the agent's whole turn —
//! engine, tool registry, kernel-backed file tools, mailbox — runs on loopback
//! and lands in CI.
//!
//! It is also the equivalence proof the two hosts were missing. The scenario
//! (`read_file_roundtrip`) is one the CLI's own PTY suite runs: a relative
//! `fixture.txt` read and echoed back. Same script, same engine, one host on a
//! kernel of its own and one on the daemon's — so a behaviour proven on either
//! side is a statement about the engine rather than about a host.

mod common;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::{
    agent_workspace, make_desc, mount_agent_world, provision_stream_transcript, user_ctx,
    wait_for_agent_reply, write_prompt,
};
use engine_host::managed_agent::spawn_managed_agent;
use kernel::kernel::Kernel;
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::{FsBackend, KernelFsBackend};

/// Model alias the scripted config defines. The mock does not care which model
/// is asked for; the config has to know it, exactly as in production.
const MODEL: &str = "claude-sonnet";

/// What the workspace fixture says, and so what the agent's reply must carry.
const FIXTURE_BODY: &str = "alpha parity line";

/// The peer the scripted reply is addressed to.
///
/// Taken from the mock rather than chosen here: nothing on this side picks the
/// agent's reply for it, so the name the script sends to and the name this test
/// watches have to be the same one.
const USER: &str = mock_anthropic_service::COHOST_REPLY_TO;

/// The scripted model, and the configuration that points the agent at it.
///
/// Both live for the whole test binary: the base URL is baked into the
/// `sudocode.json` this writes, and `SUDO_CODE_CONFIG_HOME` is process-wide, so
/// a per-test service would leave one test's config naming another's port.
struct Harness {
    runtime: tokio::runtime::Runtime,
    service: mock_anthropic_service::MockAnthropicService,
    _config_home: tempfile::TempDir,
}

impl Harness {
    /// What the scripted model was actually asked, for a failure to report.
    ///
    /// The difference between "the agent never took the message" and "the turn
    /// ran and the answer did not come back" is the first thing worth knowing,
    /// and without this a timeout says neither.
    fn requests_seen(&self) -> usize {
        self.runtime
            .block_on(self.service.captured_requests())
            .len()
    }
}

fn harness() -> &'static Harness {
    static HARNESS: OnceLock<Harness> = OnceLock::new();
    HARNESS.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("mock service runtime");
        let service = runtime
            .block_on(mock_anthropic_service::MockAnthropicService::spawn())
            .expect("mock anthropic service");
        let config_home = tempfile::Builder::new()
            .prefix("cohost-mock-config-")
            .tempdir()
            .expect("config home");
        // ONE auth mode, deliberately. A co-hosted agent has no `--auth` flag —
        // it resolves the mode from what the config offers, in the order
        // subscription → proxy → api-key — so a config carrying all three sends
        // the turn down the subscription OAuth path and out to the real API
        // (observed: `401 Invalid bearer token`). Offering only `api-key` pins
        // it on the one whose base URL this config gets to choose, which is the
        // same mode the PTY harness pins with `--auth api-key`.
        let config = serde_json::json!({
            "auth_modes": {
                "api-key": {
                    "anthropic": {
                        "baseUrl": service.base_url(),
                        "apiKey": "test-cohost-key",
                    }
                }
            },
            "models": {
                MODEL: {
                    "alias": MODEL,
                    "name": "Scripted Sonnet",
                    "input": ["text"],
                    "providers": {
                        "api-key": { "provider": "anthropic", "model": "claude-sonnet-4-6" }
                    }
                }
            }
        });
        std::fs::write(
            config_home.path().join("sudocode.json"),
            serde_json::to_vec_pretty(&config).expect("config serializes"),
        )
        .expect("write the scripted sudocode.json");
        std::env::set_var("SUDO_CODE_CONFIG_HOME", config_home.path());
        Harness {
            runtime,
            service,
            _config_home: config_home,
        }
    })
}

/// A co-hosted agent reads its workspace through the kernel and replies.
///
/// The assertion is the fixture's content coming back in the agent's reply,
/// which only happens if the whole chain worked: the loop claimed the envelope,
/// the engine asked the model, the model's `read_file` call reached a
/// kernel-backed tool, the RELATIVE path resolved against the agent's VFS
/// workspace, and the reply was appended to the transcript the user reads.
#[test]
fn a_cohost_agent_reads_its_workspace_and_replies() {
    let harness = harness();
    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);

    let pid = "cohost-mock-1";
    let agent_id = "scode-mock";
    let desc = make_desc(pid, agent_id, MODEL);

    // The user's side of the VFS: plants the fixture and provisions the
    // conversation through a real `Mailbox`, so the test exercises the
    // production provisioning path rather than planting entries by hand.
    let user_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        USER,
        "/".to_string(),
    ));
    let workspace = agent_workspace(pid);
    user_fs
        .create_dir_all(&workspace)
        .expect("plant the agent's workspace");
    user_fs
        .write(&format!("{workspace}/fixture.txt"), FIXTURE_BODY.as_bytes())
        .expect("plant the fixture the model will read");

    let transcript = InboxConvention::new(String::new()).transcript_path(USER, agent_id);
    provision_stream_transcript(&kernel, &transcript);
    let user_mb = Mailbox::daemon_absolute(Arc::clone(&user_fs), USER.to_string());
    user_mb
        .ensure_conversation(agent_id)
        .expect("provision the conversation with the co-host");

    let handle = spawn_managed_agent(Arc::clone(&kernel), desc, |state, reason| {
        eprintln!("[agent state] {state:?} reason={reason:?}");
    });

    let ctx = user_ctx();
    write_prompt(
        &kernel,
        &transcript,
        &ctx,
        USER,
        agent_id,
        "Read fixture.txt and tell me what it says. PARITY_SCENARIO:cohost_read_then_reply",
    );

    let reply = wait_for_agent_reply(
        &kernel,
        &transcript,
        &ctx,
        agent_id,
        Duration::from_secs(60),
    );
    handle.abort_signal.abort();
    let _ = handle.join.join();

    let reply = reply.unwrap_or_else(|| {
        let raw = user_fs
            .read(&transcript)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|e| format!("<unreadable: {e}>"));
        panic!(
            "no agent reply within 60s
  scripted model saw {} request(s)
  transcript:
{raw}",
            harness.requests_seen()
        )
    });
    let body = reply
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default();
    eprintln!("[agent → user] {body}");
    assert!(
        body.contains(FIXTURE_BODY),
        "the reply should carry what the agent read out of its workspace; got: {body}"
    );

    // One envelope is one turn. The scripted turn asks the model three times
    // (read, send, then the closing text), and a fourth means the loop claimed
    // the same envelope again — the re-reply storm that ships as a peer being
    // answered over and over. Caught here once already: a transcript that could
    // not advance a read position drove 1044 requests in 60 seconds.
    let asked = harness.requests_seen();
    assert!(
        asked <= 6,
        "one envelope should cost one turn; the model was asked {asked} times"
    );
}
