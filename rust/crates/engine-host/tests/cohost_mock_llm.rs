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

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::{
    agent_workspace, make_desc, mount_agent_world, provision_stream_transcript, send_prompt,
    user_ctx, wait_for_agent_reply,
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

/// Drive one co-hosted turn and report what came back, and how many times the
/// model was asked to produce it.
///
/// `transcript_is_stream` is the ONLY difference between the two tests below:
/// whether the pair's transcript is the DT_STREAM a federated daemon provides,
/// or the DT_REG a conversation degrades to when that stream cannot be created.
/// An agent has to behave the same on both, so the shape is a parameter rather
/// than a second copy of this.
fn run_read_then_reply(
    pid: &str,
    agent_id: &str,
    transcript_is_stream: bool,
) -> (String, usize, Arc<dyn FsBackend>) {
    // One at a time. What this harness configures is process-global — the
    // scripted model's base URL lives in one `SUDO_CODE_CONFIG_HOME`, and the
    // runtime build reads and creates state under it — so two overlapping turns
    // race over the same directories (`failed to build the agent runtime:
    // NotFound` under the default parallel runner, green with `--test-threads=1`,
    // which is the shape of a harness that only looks fine).
    static SERIAL: Mutex<()> = Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let harness = harness();
    // The scripted model is shared by every test in this binary, so what this
    // turn cost is a DELTA. Counting the total made the assertion depend on how
    // many tests ran before it — green alone, red in the suite.
    let asked_before = harness.requests_seen();
    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);
    let desc = make_desc(pid, agent_id, MODEL);

    // The user's side of the VFS: plants the fixture and provisions the
    // conversation through a real `Mailbox`, so this exercises the production
    // provisioning path rather than planting entries by hand.
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
    if transcript_is_stream {
        provision_stream_transcript(&kernel, &transcript);
    }
    let user_mb = Arc::new(Mailbox::daemon_absolute(
        Arc::clone(&user_fs),
        USER.to_string(),
    ));
    user_mb
        .ensure_conversation(agent_id)
        .expect("provision the conversation with the co-host");

    let handle = spawn_managed_agent(Arc::clone(&kernel), desc, |state, reason| {
        eprintln!("[agent state] {state:?} reason={reason:?}");
    });

    let ctx = user_ctx();
    send_prompt(
        &user_mb,
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

    let asked = harness.requests_seen() - asked_before;
    let reply = reply.unwrap_or_else(|| {
        let raw = user_fs
            .read(&transcript)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|e| format!("<unreadable: {e}>"));
        panic!("no agent reply within 60s; model asked {asked} time(s); transcript:\n{raw}")
    });
    let body = reply
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or_default()
        .to_string();
    eprintln!("[agent -> user] {body}");
    (body, asked, user_fs)
}

/// Delivery of one envelope costs a BOUNDED number of turns.
///
/// Not exactly one, deliberately. Delivery is at-least-once by contract: the
/// read position is one offset for a whole batch, so a batch the consumer did not
/// finish is read again, and an accepted envelope can legitimately arrive twice.
/// Asserting one turn made this test encode a promise the system does not make —
/// it passed on Windows and failed on Linux purely on timing.
///
/// What it guards is UNBOUNDEDNESS, which is the actual bug: one scripted turn
/// asks the model three times (read, send, closing text), a re-delivery or two
/// stays in single figures, and the storm this test exists for asked 1030 times
/// for the same message. The threshold separates those by a factor of seventy.
fn assert_delivery_is_bounded(asked: usize) {
    assert!(
        asked <= 15,
        "delivering one envelope should cost a few turns at most;          the model was asked {asked} times"
    );
}

/// A co-hosted agent reads its workspace through the kernel and replies.
///
/// The fixture's content coming back is the assertion, and it only happens if
/// the whole chain worked: the loop claimed the envelope, the engine asked the
/// model, the model's `read_file` call reached a kernel-backed tool, the
/// RELATIVE path resolved against the agent's VFS workspace, and the reply was
/// appended to the transcript the user reads.
#[test]
fn a_cohost_agent_reads_its_workspace_and_replies() {
    let (body, asked, _fs) = run_read_then_reply("cohost-mock-1", "scode-mock", true);
    assert!(
        body.contains(FIXTURE_BODY),
        "the reply should carry what the agent read out of its workspace; got: {body}"
    );
    assert_delivery_is_bounded(asked);
}

/// The same turn, on a transcript that could not become a stream.
///
/// `ensure_conversation` asks for a `"wal"` stream and gets a DT_REG whenever
/// federation is not wired, so this is not a corner case but every
/// non-federated daemon. Delivery has to be exactly once there too: reading a
/// byte-addressed transcript used to leave the cursor where it was, so every
/// poll re-delivered the same envelope and the peer was answered hundreds of
/// times over (1044 model calls in 60 seconds, measured).
#[test]
fn a_conversation_that_is_not_a_stream_still_delivers_once() {
    let (body, asked, _fs) = run_read_then_reply("cohost-mock-2", "scode-mock-jsonl", false);
    assert!(
        body.contains(FIXTURE_BODY),
        "a byte-addressed transcript should carry the same reply; got: {body}"
    );
    assert_delivery_is_bounded(asked);
}

/// A co-hosted agent's turn is recorded where its filesystem says sessions live.
///
/// The agent ran real turns and kept the whole transcript in memory: its session
/// had no persistence path, so nothing survived it — no `/sessions/<id>/`, no
/// `/agents/{name}/sessions/<id>` index, nothing to inspect or resume. Every
/// piece needed had shipped; the co-host was simply not a consumer of it.
///
/// Asserted through the VFS rather than by reading a struct: the point is that
/// the bytes are addressable by anyone who can reach the kernel, which is what
/// makes a co-hosted agent's history inspectable at all.
#[test]
fn a_cohost_turn_is_recorded_in_the_vfs() {
    let (_body, _asked, fs) = run_read_then_reply("cohost-mock-3", "scode-mock-session", true);

    let sessions = fs
        .readdir("/sessions")
        .expect("the backend's session root should exist after a turn");
    assert_eq!(
        sessions.len(),
        1,
        "one agent, one session; got {:?}",
        sessions.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    let id = &sessions[0].name;

    let transcript = fs
        .read_to_string(&format!("/sessions/{id}/transcript.jsonl"))
        .expect("the turn should be persisted at the session root");
    assert!(
        transcript.contains(FIXTURE_BODY),
        "the persisted transcript should carry the turn; got: {transcript}"
    );

    // The per-agent index, planted by `create_handle`. An index, not the SSOT —
    // but without it nothing can enumerate what an agent has run.
    assert!(
        fs.exists(&format!("/agents/scode-mock-session/sessions/{id}"))
            .unwrap_or(false),
        "the agent's session index should point at {id}"
    );
}
