#![cfg(feature = "mailbox")]

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

#[path = "../../engine-host/tests/common/mod.rs"]
mod common;
mod managed_harness;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::{
    agent_workspace, make_desc, mount_agent_world, provision_stream_transcript, send_prompt,
    user_ctx, wait_for_agent_reply,
};
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::Kernel;
use managed_harness::spawn_managed_agent;
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

/// The turn each test drives. The `PARITY_SCENARIO:` prefix selects the mock's
/// script, so the prompt and the script it triggers are named together.
const READ_THEN_REPLY: &str =
    "Read fixture.txt and tell me what it says. PARITY_SCENARIO:cohost_read_then_reply";
const SHELL_PWD: &str = "Run pwd and tell me where your shell is. PARITY_SCENARIO:cohost_shell_pwd";

/// One co-hosted agent at a time in this binary. What the harness configures is
/// process-global — the scripted model's base URL lives in one
/// `SUDO_CODE_CONFIG_HOME`, and the runtime build reads and creates state under
/// it — so two overlapping spawns race over the same directories (`failed to
/// build the agent runtime: NotFound` under the default parallel runner, green
/// with `--test-threads=1`, which is the shape of a harness that only looks
/// fine).
static SERIAL: Mutex<()> = Mutex::new(());

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
                        "baseUrl": "nexus:///model",
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
        std::fs::write(
            config_home.path().join("AGENTS.md"),
            "HOST_CONFIG_INSTRUCTIONS_MUST_STAY_PRIVATE",
        )
        .unwrap();
        std::env::set_var("SUDO_CODE_CONFIG_HOME", config_home.path());
        Harness {
            runtime,
            service,
            _config_home: config_home,
        }
    })
}

struct ModelWrites {
    count: Arc<AtomicUsize>,
    agent: String,
}
impl kernel::core::dispatch::NativeInterceptHook for ModelWrites {
    fn name(&self) -> &'static str {
        "model-egress-observer"
    }
    fn mutating_path_suffixes(&self) -> &'static [&'static str] {
        &[".prompt", ".reply"]
    }
    fn on_pre(
        &self,
        ctx: &kernel::core::dispatch::HookContext,
    ) -> Result<kernel::core::dispatch::HookOutcome, String> {
        if let kernel::core::dispatch::HookContext::Write(w) = ctx {
            if !w.path.starts_with("/model/") {
                return Ok(kernel::core::dispatch::HookOutcome::Pass);
            }
            assert_eq!(w.identity.user_id, "test-owner");
            assert_eq!(w.identity.agent_id, self.agent);
            if w.path.ends_with(".prompt") {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(kernel::core::dispatch::HookOutcome::Pass)
    }
}

/// Drive one co-hosted turn and report what came back, and how many times the
/// model was asked to produce it.
///
/// `transcript_is_stream` is whether the pair's transcript is the DT_STREAM a
/// federated daemon provides, or the DT_REG a conversation degrades to when that
/// stream cannot be created; an agent has to behave the same on both, so the
/// shape is a parameter rather than a second copy of this. `prompt` is a
/// parameter for the same reason: a second scenario is a different question for
/// the same agent, not a different harness.
fn run_cohost_turn(
    pid: &str,
    agent_id: &str,
    transcript_is_stream: bool,
    prompt: &str,
) -> (String, usize, Arc<dyn FsBackend>) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let harness = harness();
    // The scripted model is shared by every test in this binary, so what this
    // turn cost is a DELTA. Counting the total made the assertion depend on how
    // many tests ran before it — green alone, red in the suite.
    let asked_before = harness.requests_seen();
    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);
    let _model = common::mount_model(
        &kernel,
        "anthropic",
        &harness.service.base_url(),
        "test-cohost-key",
    );
    let model_writes = Arc::new(AtomicUsize::new(0));
    let hook = kernel
        .enlist_hook_only_service("model-egress-observer")
        .expect("model observer");
    kernel.register_service_hook(
        &hook,
        Box::new(ModelWrites {
            count: Arc::clone(&model_writes),
            agent: agent_id.to_string(),
        }),
    );
    let desc = make_desc(pid, agent_id, MODEL);

    // The user's side of the VFS: plants the fixture and provisions the
    // conversation through a real `Mailbox`, so this exercises the production
    // provisioning path rather than planting entries by hand.
    // The user's descriptor, planted the way the trusted service plants one,
    // so the test's writes carry the same authority a real host's would.
    let user_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &AgentDescriptor {
            pid: format!("pid-{USER}"),
            name: USER.to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..AgentDescriptor::default()
        },
        "/".to_string(),
    ));
    let workspace = agent_workspace(pid);
    user_fs
        .create_dir_all(&workspace)
        .expect("plant the agent's workspace");
    user_fs
        .write(&format!("{workspace}/fixture.txt"), FIXTURE_BODY.as_bytes())
        .expect("plant the fixture the model will read");

    user_fs
        .write(
            &format!("{workspace}/AGENTS.md"),
            b"VFS_WORKSPACE_INSTRUCTIONS_8142",
        )
        .unwrap();

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
    })
    .expect("this host can run an agent: the mock test supplies a config home");

    let ctx = user_ctx();
    send_prompt(&user_mb, agent_id, prompt);

    let reply = wait_for_agent_reply(
        &kernel,
        &transcript,
        &ctx,
        agent_id,
        Duration::from_secs(60),
    );
    // A send tool can deliver the reply before its turn finishes. Wait for the
    // durable receive acknowledgement before terminating the host.
    if reply.is_some() {
        let receiver = Mailbox::daemon_absolute(Arc::clone(&user_fs), agent_id.into());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !receiver
            .read_position(USER)
            .unwrap()
            .is_some_and(|offset| offset > 0)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "turn did not acknowledge peer input"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    handle.abort_signal.abort();
    let _ = handle.join.join();

    // A prompt write schedules the provider asynchronously. Aborting the agent
    // after its mailbox reply can race that last HTTP dispatch, even though the
    // agent thread has joined. Wait for the writes already recorded by the hook
    // to reach the mock before comparing the two sides of the transport.
    let expected = model_writes.load(Ordering::SeqCst);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut asked = harness.requests_seen() - asked_before;
    while asked < expected && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        asked = harness.requests_seen() - asked_before;
    }
    assert_eq!(
        expected, asked,
        "every upstream model call must cross the session filesystem"
    );
    let requests = harness
        .runtime
        .block_on(harness.service.captured_requests());
    assert!(requests.len() > asked_before, "model received no request");
    for request in &requests[asked_before..] {
        let body: serde_json::Value = serde_json::from_str(&request.raw_body).unwrap();
        let system = body["system"].to_string();
        assert!(
            system.contains("VFS_WORKSPACE_INSTRUCTIONS_8142"),
            "workspace instructions missing"
        );
        assert!(
            !system.contains("HOST_CONFIG_INSTRUCTIONS_MUST_STAY_PRIVATE"),
            "host instructions leaked"
        );
        assert!(system.contains(&workspace), "workspace missing");
        assert!(system.contains("Nexus virtual filesystem (POSIX paths)"));
        assert!(
            system.contains(&format!("/agents/{agent_id}/memory")),
            "memory uses wrong namespace"
        );
    }
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

/// Delivery ADVANCED the agent's durable read position.
///
/// This replaces a turn-count ceiling, and the reason is worth keeping. Delivery is
/// at-least-once by contract — the read position is one offset for a whole batch, so
/// a batch the consumer did not finish is read again — and one scripted turn costs
/// the model three calls (read, send, closing text). The total is therefore a
/// property of how fast the runner is, not of the code: `asked <= 15` passed on
/// Windows and Linux and failed on macOS at 16. A threshold tuned to one machine's
/// number is a test that goes red on a slower machine for a reason that has nothing
/// to do with the behaviour under test.
///
/// The storm this test exists for had a STRUCTURAL signature instead. The DT_REG
/// fallback carries no `stream_next_offset`, so the durable read position never
/// advanced and the same envelope was claimed 1030 times in a minute. A position past
/// zero is the negation of exactly that: it says the consumer COMMITTED what it read,
/// which is the one thing a storm cannot do. It costs no wall-clock and cannot drift
/// with a runner's speed.
///
/// The count is still PRINTED — it is the first number worth seeing when this fails,
/// and a regression that re-delivered while advancing would show up there, visible in
/// the log rather than encoded as a guess.
fn assert_delivery_advanced_the_cursor(fs: &Arc<dyn FsBackend>, agent_id: &str, asked: usize) {
    let agent = Mailbox::daemon_absolute(Arc::clone(fs), agent_id.to_string());
    let position = agent
        .read_position(USER)
        .expect("the agent's read register should be readable");
    eprintln!("[delivery] the model was asked {asked} time(s); read position {position:?}");
    assert!(
        matches!(position, Some(offset) if offset > 0),
        "the agent's durable read position must advance past the envelope it answered          — a position that never moves is the re-claim storm; got {position:?}"
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
    let (body, asked, fs) = run_cohost_turn("cohost-mock-1", "scode-mock", true, READ_THEN_REPLY);
    assert!(
        body.contains(FIXTURE_BODY),
        "the reply should carry what the agent read out of its workspace; got: {body}"
    );
    assert_delivery_advanced_the_cursor(&fs, "scode-mock", asked);
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
    let (body, asked, fs) =
        run_cohost_turn("cohost-mock-2", "scode-mock-jsonl", false, READ_THEN_REPLY);
    assert!(
        body.contains(FIXTURE_BODY),
        "a byte-addressed transcript should carry the same reply; got: {body}"
    );
    assert_delivery_advanced_the_cursor(&fs, "scode-mock-jsonl", asked);
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
    let (_body, _asked, fs) =
        run_cohost_turn("cohost-mock-3", "scode-mock-session", true, READ_THEN_REPLY);

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

/// A co-hosted agent is not offered crons this daemon will never fire.
///
/// The scode scheduler is a CLI process (`scode cron daemon`, or OS cron calling
/// `scode cron tick`); nexusd does not tick `crons.json`. So a `CronCreate` here
/// would persist an entry that either never fires or — if a ticker happens to
/// share this machine's config home — fires later as a standalone CLI run under
/// a different identity, in a host directory. Both are worse than a refusal.
///
/// Both halves are asserted, because hiding the specs only stops a model that
/// reads the list; one that remembers the name from training calls it anyway.
#[test]
fn a_cohost_is_not_offered_crons_this_daemon_will_never_fire() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _harness = harness();
    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);

    // Through the production spawn path: the declaration is the co-host's own
    // fact about itself, so asserting it after a real spawn is what proves it is
    // made at all.
    let handle = spawn_managed_agent(
        Arc::clone(&kernel),
        make_desc("pid-cron", "cron-agent", MODEL),
        |_, _| {},
    )
    .expect("this host can run an agent: the mock test supplies a config home");
    assert!(
        tools::cron_tools_disabled(),
        "spawning a co-hosted agent should declare that this host fires no crons"
    );
    assert!(
        !tools::mvp_tool_specs()
            .iter()
            .any(|spec| spec.name.starts_with("Cron")),
        "the cron tools should not be advertised to a co-hosted agent"
    );
    let refused = tools::execute_tool("CronList", &serde_json::json!({}))
        .expect_err("a cron call must be refused, not answered");
    assert!(
        refused.contains("nexusd"),
        "the refusal should say which host will not fire it; got: {refused}"
    );

    handle.abort_signal.abort();
    let _ = handle.join.join();
}

/// A co-hosted agent's shell runs in a directory of its OWN.
///
/// The scope is thread-local on the loop thread, so no test thread can read it —
/// but the agent can be asked. It runs `pwd` and reports the answer, and the
/// answer has to be its own directory rather than the daemon's, which is what
/// `current_workspace_root()` falls through to when no scope is entered. That
/// default was shared by every co-hosted agent on the daemon and pointed at the
/// daemon's own git repository, so `git status` — and the git-context hook, and
/// the stale-branch check — answered about the daemon.
///
/// Matched loosely on purpose: `sh -lc pwd` prints an MSYS path on Windows
/// (`/c/Users/...`) and a canonicalised one on macOS, and the claim here is about
/// WHICH directory, not how the platform spells it.
#[test]
fn a_cohosted_agents_shell_runs_in_its_own_directory() {
    let agent_id = "scode-mock-shell";
    let (body, _asked, _fs) = run_cohost_turn("cohost-mock-4", agent_id, true, SHELL_PWD);
    let reported = body.replace('\\', "/");
    assert!(
        reported.contains(&format!("agents/{agent_id}/shell")),
        "the agent should report its own shell root; got {reported}"
    );
}

#[test]
fn a_cohost_subagent_uses_the_same_model_mount_and_identity() {
    let (body, asked, _fs) = run_cohost_turn(
        "cohost-delegate",
        "scode-mock-delegate",
        true,
        "Delegate a calculation and send me the result. PARITY_SCENARIO:cohost_delegate",
    );
    assert!(
        body.contains("203"),
        "the child must return its calculation, got {body}"
    );
    assert!(asked >= 3, "both parent and child must reach the provider");
}

fn managed_call(
    kernel: &Kernel,
    method: &str,
    request: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let ctx = kernel::kernel::OperationContext::new("test-owner", "root", true, None, true);
    let response = kernel
        .dispatch_rust_call(
            "managed_agent",
            method,
            request.to_string().as_bytes(),
            &ctx,
        )
        .expect("managed service installed")
        .map_err(|e| format!("{e:?}"))?;
    Ok(serde_json::from_slice(&response).unwrap())
}

fn wait_idle(kernel: &Kernel, pid: &serde_json::Value) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = managed_call(
            kernel,
            "get_session_v1",
            serde_json::json!({"session_id":pid}),
        )
        .unwrap();
        if state["state"] == "ready" {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent did not become ready: {state}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn stop_session(kernel: &Kernel, pid: &serde_json::Value, sid: &str) {
    managed_call(
        kernel,
        "cancel_v1",
        serde_json::json!({"session_id":pid,"mode":"session"}),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let path = format!("/sessions/{sid}");
    loop {
        if let Some(lock) = kernel.sys_lock(&path, "", 1, 5, "test").unwrap() {
            kernel.sys_unlock(&path, &lock, false).unwrap();
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "old worker still owns the session"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn await_new_reply(
    mailbox: &Mailbox,
    agent: &str,
    cursor: &mut u64,
    kernel: &Kernel,
    pid: &serde_json::Value,
    phase: &str,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let (messages, next) = mailbox.poll_conversation(agent, *cursor, 100).unwrap();
        *cursor = next;
        if messages.iter().any(|m| m.from == agent) {
            return;
        }
    }
    let state = managed_call(
        kernel,
        "get_session_v1",
        serde_json::json!({"session_id":pid}),
    );
    panic!("no new reply during {phase}; cursor={cursor}; session={state:?}");
}

/// Observe the replacement receiver's actual read before releasing the old
/// lease. Content still goes through the same kernel and backing store.
struct ReaderHandoffStore {
    inner: runtime::test_support::MemObjectStore,
    observed: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

impl kernel::abc::object_store::ObjectStore for ReaderHandoffStore {
    fn name(&self) -> &str {
        "reader-handoff"
    }

    fn write_content(
        &self,
        content: &[u8],
        content_id: &str,
        ctx: &kernel::kernel::OperationContext,
        offset: u64,
    ) -> Result<kernel::abc::object_store::WriteResult, kernel::abc::object_store::StorageError>
    {
        self.inner.write_content(content, content_id, ctx, offset)
    }

    fn read_content(
        &self,
        content_id: &str,
        ctx: &kernel::kernel::OperationContext,
    ) -> Result<Vec<u8>, kernel::abc::object_store::StorageError> {
        let bytes = self.inner.read_content(content_id, ctx)?;
        if serde_json::from_slice::<runtime::mailbox::ReaderRegister>(&bytes)
            .is_ok_and(|register| register.holder == "draining-test-receiver")
        {
            if let Some(observed) = self.observed.lock().unwrap().take() {
                let _ = observed.send(());
            }
        }
        Ok(bytes)
    }
}

#[test]
fn managed_rpc_restores_history_on_a_new_pid_and_keeps_writing_the_same_vfs_session() {
    use serde_json::json;
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let harness = harness();
    let kernel = Arc::new(Kernel::new());
    mount_agent_world(&kernel);
    let (reader_observed_tx, reader_observed_rx) = std::sync::mpsc::channel();
    kernel.vfs_router_arc().add_mount(
        "/conversations",
        "root",
        Some(Arc::new(ReaderHandoffStore {
            inner: runtime::test_support::MemObjectStore::default(),
            observed: Mutex::new(Some(reader_observed_tx)),
        })),
        false,
    );
    let _model_storage = common::mount_model(
        &kernel,
        "anthropic",
        &harness.service.base_url(),
        "test-cohost-key",
    );
    managed_agent::install_managed_agent_with_spawn(
        &kernel,
        Arc::new(engine_acp::managed_agent::SudoCodeSpawnAdapter),
    )
    .unwrap();
    let agent = "resume-agent";
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &AgentDescriptor {
            pid: format!("pid-{agent}"),
            name: agent.to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..AgentDescriptor::default()
        },
        "/".to_string(),
    ));
    let mb = Arc::new(Mailbox::daemon_absolute(Arc::clone(&fs), USER.to_string()));
    let transcript = InboxConvention::new(String::new()).transcript_path(USER, agent);
    provision_stream_transcript(&kernel, &transcript);
    mb.ensure_conversation(agent).unwrap();
    let mut request =
        json!({"agent_id":agent,"owner_id":"test-owner","zone_id":"root","model":MODEL});
    provision_stream_transcript(
        &kernel,
        &a2a::conversation_transcript_path(&a2a::conversation_id(agent, "test-owner")),
    );
    let first = managed_call(&kernel, "start_session_v1", request.clone()).unwrap();
    let first_controller =
        managed_harness::attach_controller(Arc::clone(&kernel), &first, false).unwrap();
    let sid = first["durable_session_id"].as_str().unwrap();
    assert_ne!(first["session_id"], sid);
    let path = format!("/sessions/{sid}/transcript.jsonl");
    assert!(
        fs.exists(&path).unwrap(),
        "even an empty session is durable before start returns"
    );
    request["resume_session_id"] = json!(sid);
    let busy = managed_call(&kernel, "start_session_v1", request.clone()).unwrap_err();
    assert!(busy.contains("still running"), "{busy}");
    let mut cursor = 0;
    (mb.sender())(
        agent,
        "Remember BEFORE_RESTART_8317. PARITY_SCENARIO:cohost_reply",
    )
    .unwrap();
    await_new_reply(
        &mb,
        agent,
        &mut cursor,
        &kernel,
        &first["session_id"],
        "first turn",
    );
    wait_idle(&kernel, &first["session_id"]);
    stop_session(&kernel, &first["session_id"], sid);
    drop(first_controller);
    let before = runtime::Session::load_from_path_with(&*fs, &path).unwrap();
    assert!(!before.messages.is_empty());
    let before_bytes = fs.read_to_string(&path).unwrap();
    // The bare kernel has no raft WAL and initially degrades to a regular
    // file. Put the saved records in a native stream to exercise production's
    // framing on restore: a snapshot rewrite here would duplicate every turn.
    fs.delete(&path).unwrap();
    provision_stream_transcript(&kernel, &path);
    assert!(fs.is_append_stream(&path).unwrap());
    for line in before_bytes.lines() {
        fs.append(&path, format!("{line}\n").as_bytes()).unwrap();
    }

    for (key, value) in [
        ("agent_id", json!("another-agent")),
        ("owner_id", json!("another-owner")),
        ("zone_id", json!("another-zone")),
        (
            "repos",
            json!([{"alias":"repo","host_path":"/repos/different"}]),
        ),
    ] {
        let mut wrong = request.clone();
        wrong[key] = value;
        assert!(
            managed_call(&kernel, "start_session_v1", wrong).is_err(),
            "accepted changed {key}"
        );
        assert_eq!(fs.read_to_string(&path).unwrap(), before_bytes);
    }
    // Model an old receiver finishing its cleanup just after the replacement
    // first checks its lease. A read handshake, rather than a sleep, forces
    // that ordering; the replacement must notice the explicit early release.
    let reader_path = InboxConvention::new(String::new()).reader_path(USER, agent, agent);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let released = loop {
        let register: runtime::mailbox::ReaderRegister =
            serde_json::from_slice(&fs.read(&reader_path).unwrap()).unwrap();
        if register.holder.is_empty() {
            break register;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "old reader did not release"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut held = released.clone();
    held.holder = "draining-test-receiver".into();
    // Outlive the reply budget: only observing early release can pass this.
    held.lease_expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    fs.write_atomic(&reader_path, &serde_json::to_vec(&held).unwrap())
        .unwrap();
    let release_fs = Arc::clone(&fs);
    let release_reader = std::thread::spawn(move || {
        reader_observed_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("replacement must read the held reader lease");
        release_fs
            .write_atomic(&reader_path, &serde_json::to_vec(&released).unwrap())
            .unwrap();
    });
    let captured_before = harness.requests_seen();
    let second = managed_call(&kernel, "start_session_v1", request.clone()).unwrap();
    let second_controller =
        managed_harness::attach_controller(Arc::clone(&kernel), &second, true).unwrap();
    release_reader
        .join()
        .expect("the previous reader releases its lease");
    assert_ne!(second["session_id"], first["session_id"]);
    assert_eq!(second["durable_session_id"], sid);
    (mb.sender())(
        agent,
        "Continue after restart. PARITY_SCENARIO:cohost_reply",
    )
    .unwrap();
    await_new_reply(
        &mb,
        agent,
        &mut cursor,
        &kernel,
        &second["session_id"],
        "resumed turn",
    );
    wait_idle(&kernel, &second["session_id"]);
    stop_session(&kernel, &second["session_id"], sid);
    drop(second_controller);
    let after = runtime::Session::load_from_path_with(&*fs, &path).unwrap();
    assert_eq!(after.session_id, before.session_id);
    assert!(after.messages.len() > before.messages.len());
    assert_eq!(
        &after.messages[..before.messages.len()],
        before.messages.as_slice()
    );
    let requests = harness
        .runtime
        .block_on(harness.service.captured_requests());
    assert!(
        requests[captured_before..]
            .iter()
            .any(|r| r.raw_body.contains("BEFORE_RESTART_8317")),
        "the resumed model must receive the old conversation"
    );
    assert_eq!(fs.readdir("/sessions").unwrap().len(), 1);
    // Store listing and forks must also use this backend, rather than reading
    // or writing the daemon's host disk.
    let store = runtime::session_control::SessionStore::from_cwd_with(
        after.workspace_root().unwrap(),
        Arc::clone(&fs),
    )
    .unwrap()
    .with_identity(after.identity.clone().unwrap());
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    let loaded = store.load_session(sid).unwrap().session;
    let fork = store.fork_session(&loaded, None).unwrap();
    assert!(fs.exists(&fork.handle.path.to_string_lossy()).unwrap());
    // Legacy data remains readable locally but lacks the ownership proof a
    // hosted restore requires. Do not silently adopt or replace it.
    fs.delete(&path).unwrap();
    let mut legacy = after;
    legacy.identity = None;
    legacy
        .with_persistence_path(&path)
        .with_fs_backend(Arc::clone(&fs))
        .save_to_path(&path)
        .unwrap();
    assert!(managed_call(&kernel, "start_session_v1", request)
        .unwrap_err()
        .contains("without identity"));
}
