//! Live E2E: a nexus-A2A peer message reaches an ACP client.
//!
//! The send half of standalone A2A has worked since #508 — `scode` advertises
//! a `send_message` tool and writes to the peer's replicated `DT_STREAM` inbox.
//! The receive half was only ever wired into the interactive REPL, which
//! printed `📨 A2A from <peer>` to the terminal. Anything driving `scode acp`
//! programmatically — which is what an agent does, and the reason the ACP cut
//! exists — saw nothing at all. A duet between two such agents could send and
//! never receive.
//!
//! This proves the other half over a real daemon and a real `DT_STREAM`: a peer
//! writes into the agent's inbox and the ACP client is told, out of band, with
//! no turn in flight.
//!
//! Ignored by default — it needs a running `nexusd-cluster`, which no unit test
//! can provide:
//!
//! ```text
//! nexusd-cluster serve-local --port 21777 --data-dir <d> --identity-dir <i>
//! NEXUS_A2A_TEST_ENDPOINT=127.0.0.1:21777 \
//!   cargo test -p rusty-sudocode-cli --test acp_a2a_receive -- --ignored --nocapture
//! ```
//!
//! Cert-only, like production: both the spawned receiver and the test sender
//! dial with a minted credential (`NEXUS_A2A_TEST_RECEIVER_CREDENTIAL` /
//! `NEXUS_A2A_TEST_CREDENTIAL`). The node stamps `from` from the cert SAN, so
//! the assertion pins the sender by the name its credential carries.

#[path = "support/node_home.rs"]
mod node_home;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_vfs_client::NexusVfsClient;
use runtime::agent_mailbox::MailboxEnvelope;

/// The unified mailbox for `agent` — the same transport a running agent uses.
///
/// These call sites used to reach a second implementation that lived beside
/// `Mailbox` and duplicated it. Production had already moved, so exercising the
/// copy proved nothing about what ships.
fn mailbox(client: &Arc<NexusVfsClient>, agent: &str, auth: &str) -> Mailbox {
    // The mTLS dial is the server-verified credential, so this is the
    // transport-verified construction a running agent uses.
    Mailbox::over_nexus_mtls(Arc::clone(client), agent, auth, "root")
}

fn send_to(
    client: &Arc<NexusVfsClient>,
    from_agent: &str,
    to_agent: &str,
    body: &str,
    auth: &str,
) -> Result<(), String> {
    mailbox(client, from_agent, auth).send(MailboxEnvelope {
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: from_agent.to_string(),
        to: to_agent.to_string(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: String::new(),
        request_id: None,
    })
}

use runtime::mailbox::Mailbox;
use serde_json::{json, Value};

/// Dial the test daemon with a minted credential, exactly as production does.
///
/// Cert-only: `NEXUS_A2A_TEST_CREDENTIAL` points at the bundle dir
/// `nexusd-cluster auth mint` wrote for the test sender. There is no plaintext
/// path, so the test dials the same way a shipped `scode` does.
fn dial_test_daemon(endpoint: &str) -> Arc<NexusVfsClient> {
    let credential = std::env::var("NEXUS_A2A_TEST_CREDENTIAL")
        .expect("set NEXUS_A2A_TEST_CREDENTIAL=<minted bundle dir>");
    let credential = runtime::nexus_mailbox::AgentCredential::load(&credential)
        .expect("load the test credential bundle");
    runtime::nexus_mailbox::Config {
        endpoint: endpoint.to_string(),
        agent: credential.agent,
        tls: credential.tls,
    }
    .connect()
    .expect("dial the daemon")
}

/// The agent this `scode acp` answers to, and the peer that writes to it.
const SELF_AGENT: &str = "acp-receiver-probe";
const PEER_AGENT: &str = "acp-receiver-peer";

/// How long to wait for the notification after the peer writes. Generous: the
/// receiver parks on a blocking tail read, so the wake is event-driven and
/// normally immediate — this is the budget for a loaded machine, not for a
/// poll interval.
const NOTIFICATION_BUDGET: Duration = Duration::from_secs(30);

struct AcpStdio {
    child: Child,
    stdin: ChildStdin,
    /// Lines pumped off stdout by a reader thread.
    ///
    /// Reading inline would make every budget in this file a lie: a blocking
    /// `read_line` on a stream that never produces another byte cannot notice
    /// its own deadline, so a failing assertion presents as a hang and the test
    /// has to be killed by hand. The pump turns "nothing arrived" into a
    /// timeout, which is what a test is supposed to report.
    lines: Receiver<String>,
    next_id: u64,
    /// The node record the child attached through; outlives the child.
    _node_home: node_home::NodeHome,
}

impl AcpStdio {
    fn request(&mut self, method: &str, params: &Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush request");
        loop {
            let msg = self.read_message();
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                return msg;
            }
        }
    }

    fn read_message(&mut self) -> Value {
        self.read_message_within(NOTIFICATION_BUDGET)
            .unwrap_or_else(|| panic!("no line from scode acp within {NOTIFICATION_BUDGET:?}"))
    }

    /// The next line, or `None` if the stream stayed silent for `budget`.
    fn read_message_within(&mut self, budget: Duration) -> Option<Value> {
        match self.lines.recv_timeout(budget) {
            Ok(line) => Some(
                serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("non-JSON line from acp: {e}: {line}")),
            ),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("scode acp closed its stdout unexpectedly")
            }
        }
    }

    /// Read notifications until one satisfies `matches`, or the budget expires.
    ///
    /// Waits on the transport rather than issuing requests to sweep for
    /// notifications: what is under test is precisely whether the server can
    /// speak to the client with nothing in flight.
    fn await_notification(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        self.try_await_notification(matches, NOTIFICATION_BUDGET)
            .unwrap_or_else(|seen| {
                panic!("no matching notification within {NOTIFICATION_BUDGET:?}; saw {seen:#?}")
            })
    }

    /// `Ok(notification)` or `Err(everything that did arrive)` — so a test can
    /// assert that something did NOT show up without the absence being a hang.
    fn try_await_notification(
        &mut self,
        matches: impl Fn(&Value) -> bool,
        budget: Duration,
    ) -> Result<Value, Vec<Value>> {
        let deadline = Instant::now() + budget;
        let mut seen = Vec::new();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.read_message_within(remaining) {
                Some(msg) if msg.get("method").is_some() && matches(&msg) => return Ok(msg),
                Some(msg) => seen.push(msg),
                None => break,
            }
        }
        Err(seen)
    }
}

impl Drop for AcpStdio {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A successful close must retire the real receiver worker before the client
/// can terminate its CLI. Merely observing an empty agent's startup response
/// misses this branch: the receiver may not have claimed its seat yet.
#[test]
#[ignore = "requires a running nexusd-cluster and a dedicated receiver credential"]
fn live_session_close_releases_an_acquired_receiver_seat_before_reply() {
    use runtime::fs_backend::{FsBackend, NexusVfsFsBackend};

    let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("test daemon endpoint");
    let bundle = std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL")
        .expect("dedicated lifecycle receiver credential");
    let credential = runtime::nexus_mailbox::AgentCredential::load(&bundle)
        .expect("load lifecycle receiver credential");
    let agent = credential.agent.clone();
    let client = runtime::nexus_mailbox::Config {
        endpoint: endpoint.clone(),
        agent: agent.clone(),
        tls: credential.tls,
    }
    .connect()
    .expect("connect using the receiver's own mTLS identity");
    let backend = NexusVfsFsBackend::from_arc_mtls(client, String::new(), "root");
    let path = format!("/agents/{agent}/receiver.lock");
    let workspace = tempfile::tempdir().expect("lifecycle workspace");
    let config_home = tempfile::tempdir().expect("lifecycle config home");
    let binary = std::env::var_os("SCODE_RECEIVER_TEST_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_scode").into());
    let mut acp = spawn_acp_from_binary(
        &binary,
        &endpoint,
        &agent,
        "unused-peer",
        &workspace,
        &config_home,
    );
    let initialized = acp.request(
        "initialize",
        &json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    assert!(initialized.get("result").is_some(), "{initialized}");
    let opened_at = Instant::now();
    let opened = acp.request(
        "session/new",
        &json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
    );
    let sid = opened["result"]["sessionId"]
        .as_str()
        .expect("new SID")
        .to_string();
    let second = acp.request(
        "session/new",
        &json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
    );
    let second_sid = second["result"]["sessionId"]
        .as_str()
        .expect("second SID")
        .to_string();
    await_real_receiver_seat(&backend, &path);
    let first_close = acp.request("session/close", &json!({"sessionId": sid}));
    assert!(first_close.get("result").is_some(), "{first_close}");
    assert!(
        backend
            .try_lock(&path, 30_000)
            .expect("seat after first close")
            .is_none(),
        "closing one session must preserve its agent's other live session"
    );
    let last_close = acp.request("session/close", &json!({"sessionId": second_sid}));
    assert!(last_close.get("result").is_some(), "{last_close}");
    assert!(
        opened_at.elapsed() < Duration::from_secs(15),
        "the close proof must precede the receiver's original 30-second TTL"
    );
    let probe = backend
        .try_lock(&path, 30_000)
        .expect("seat after last close");
    assert!(
        probe.is_some(),
        "close replied before its acquired receiver seat was released"
    );
    drop(probe);

    // Re-entry on this connection exercises the retained retirement handle.
    let loaded = acp.request(
        "session/load",
        &json!({"sessionId": second_sid, "cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
    );
    assert!(loaded.get("result").is_some(), "{loaded}");
    await_real_receiver_seat(&backend, &path);
    let closed = acp.request("session/close", &json!({"sessionId": second_sid}));
    assert!(closed.get("result").is_some(), "{closed}");
    let probe = backend
        .try_lock(&path, 30_000)
        .expect("seat after re-entry close");
    assert!(probe.is_some(), "re-entered receiver was not joined");
    drop(probe);
}

/// Start `scode acp` wired to `endpoint` as `agent`, with an isolated config
/// home so the durable read cursor a test writes cannot leak into another test
/// or into the developer's own.
fn spawn_acp(
    endpoint: &str,
    agent: &str,
    peer: &str,
    workspace: &tempfile::TempDir,
    config_home: &tempfile::TempDir,
) -> AcpStdio {
    spawn_acp_from_binary(
        std::ffi::OsStr::new(env!("CARGO_BIN_EXE_scode")),
        endpoint,
        agent,
        peer,
        workspace,
        config_home,
    )
}

fn spawn_acp_from_binary(
    binary: &std::ffi::OsStr,
    endpoint: &str,
    agent: &str,
    _peer: &str,
    workspace: &tempfile::TempDir,
    config_home: &tempfile::TempDir,
) -> AcpStdio {
    // An isolated config home needs a config: the point of isolating it is the
    // read cursor, not to test scode's behaviour without providers.
    std::fs::write(
        config_home.path().join("sudocode.json"),
        runtime::SAMPLE_SUDOCODE_JSON,
    )
    .expect("seed the isolated config home");

    // The receiver attaches through a node record in its own home, as `agent`,
    // with its OWN minted credential: `agent` is the name that bundle carries.
    let receiver_credential = std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL")
        .unwrap_or_else(|_| panic!("set NEXUS_A2A_TEST_RECEIVER_CREDENTIAL=<bundle for {agent}>"));
    let node_home = node_home::NodeHome::new(
        "acp-receiver",
        endpoint,
        agent,
        std::path::Path::new(&receiver_credential),
    );
    node_home::pin_agent_name(workspace.path(), agent);
    let mut cmd = Command::new(binary);
    cmd.arg("acp")
        .current_dir(workspace.path())
        .env("SUDO_CODE_CONFIG_HOME", config_home.path())
        .envs(node_home.env())
        .env("SUDOCODE_INTERRUPT_QUEUE_MODE", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().expect("spawn scode acp");
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    AcpStdio {
        child,
        stdin,
        lines,
        next_id: 0,
        _node_home: node_home,
    }
}

#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT"]
fn a2a_peer_message_reaches_an_acp_client() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");

    // Both inboxes exist before anything polls: `scode` self-provisions its own,
    // but the peer's has to be there for a reply to have somewhere to go, and
    // provisioning after the receiver seeks to tail would race it.
    // The dial a running `scode` performs, through the one constructor that
    // owns it. Cert-only, like production: the test dials with a minted
    // credential and the node stamps `from` from its cert SAN.
    let client = dial_test_daemon(&endpoint);
    // One call provisions BOTH sides: a conversation is indexed under each
    // agent, so there is no separate peer inbox left to create.
    mailbox(&client, SELF_AGENT, "")
        .ensure_conversation(PEER_AGENT)
        .expect("provision the conversation");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let config_home = tempfile::tempdir().expect("temp config home");
    let mut acp = spawn_acp(&endpoint, SELF_AGENT, PEER_AGENT, &workspace, &config_home);

    let init = acp.request(
        "initialize",
        &json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    assert!(init.get("result").is_some(), "initialize failed: {init}");

    // The receiver starts with the first session — before one exists there is
    // nobody to notify.
    let new_session = acp.request(
        "session/new",
        &json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
    );
    let session_id = new_session["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session/new returned no sessionId: {new_session}"))
        .to_string();

    // The peer writes. Nothing is in flight: no prompt, no turn.
    let body = "ping from the peer over a real dt_stream";
    send_to(&client, PEER_AGENT, SELF_AGENT, body, "").expect("peer writes to the agent inbox");

    let notification = acp.await_notification(|msg| {
        msg["method"] == "session/update"
            && msg["params"]["update"]["sessionUpdate"] == "user_message_chunk"
    });

    assert_eq!(
        notification["params"]["sessionId"], session_id,
        "the update belongs to the live session: {notification}"
    );
    let text = notification["params"]["update"]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("update carries text content: {notification}"));
    assert!(
        text.contains(body),
        "the peer's message body reaches the client: {text}"
    );
    assert!(
        text.contains(PEER_AGENT),
        "and names who sent it, so a reply can be addressed: {text}"
    );
    assert_eq!(
        notification["params"]["_meta"]["sudocode"]["a2a"]["from"], PEER_AGENT,
        "the sender is also structured, so a client can tell peer mail from a \
         human typing rather than parsing the prose: {notification}"
    );
}

/// A message that arrives while nothing is listening is still delivered to the
/// next receiver.
///
/// The receiver used to seek to the tail on every start, which made delivery
/// depend on the receiver happening to be running at the moment of the send.
/// Two agents handing off asynchronously then lose mail that the sender was
/// told was delivered and that is sitting durably in the stream — observed live
/// in the Win↔Mac duet, where a reply landed while the peer was between
/// processes and no later reader ever looked back at it.
#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT"]
fn a_message_sent_while_offline_is_delivered_on_the_next_start() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    // A fresh identity per run, so "has this client ever read?" is unambiguous.
    let agent = format!("offline-probe-{}", std::process::id());
    let peer = format!("{agent}-peer");

    // The dial a running `scode` performs, through the one constructor that
    // owns it. Cert-only, like production: the test dials with a minted
    // credential and the node stamps `from` from its cert SAN.
    let client = dial_test_daemon(&endpoint);
    // One call provisions BOTH sides: a conversation is indexed under each
    // agent, so there is no separate peer inbox left to create.
    mailbox(&client, &agent, "")
        .ensure_conversation(&peer)
        .expect("provision the conversation");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let config_home = tempfile::tempdir().expect("temp config home");

    // First run: establishes the cursor, then goes away.
    let cursor_file = config_home.path().join(format!("a2a-cursor-{agent}"));
    {
        let mut acp = spawn_acp(&endpoint, &agent, &peer, &workspace, &config_home);
        acp.request(
            "initialize",
            &json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        acp.request(
            "session/new",
            &json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
        );
        // Wait for the cursor to actually exist before killing the process.
        // The premise of this test is "a client that HAS read this inbox
        // before"; without the wait it would sometimes assert that against a
        // client that was killed mid-handshake, which is a different scenario
        // and would fail for the wrong reason.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cursor_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            cursor_file.exists(),
            "the first run should record a read cursor at {}",
            cursor_file.display()
        );
    } // dropped: the receiver is gone

    // The peer writes into a mailbox nobody is watching.
    let body = "sent while the receiver was down";
    send_to(&client, &peer, &agent, body, "").expect("peer writes while nothing listens");

    // Second run: must pick up where the first left off.
    let mut acp = spawn_acp(&endpoint, &agent, &peer, &workspace, &config_home);
    acp.request(
        "initialize",
        &json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    acp.request(
        "session/new",
        &json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
    );
    let notification = acp.await_notification(|msg| {
        msg["method"] == "session/update"
            && msg["params"]["update"]["sessionUpdate"] == "user_message_chunk"
    });
    let text = notification["params"]["update"]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("update carries text: {notification}"));
    assert!(
        text.contains(body),
        "a message sent while offline must survive to the next start: {text}"
    );
}

/// A receiver that has never read this inbox does not replay its history.
///
/// The other direction, and the reason the fix is a resumed cursor rather than
/// "always start from zero": an agent that was never party to a conversation
/// should not wake up and answer all of it. That is the re-reply storm the
/// co-host path had to fix once already.
#[test]
#[ignore = "requires a running nexusd-cluster; set NEXUS_A2A_TEST_ENDPOINT"]
fn a_first_time_receiver_does_not_replay_history() {
    let endpoint =
        std::env::var("NEXUS_A2A_TEST_ENDPOINT").expect("set NEXUS_A2A_TEST_ENDPOINT=host:port");
    let agent = format!("virgin-probe-{}", std::process::id());
    let peer = format!("{agent}-peer");

    // The dial a running `scode` performs, through the one constructor that
    // owns it. Cert-only, like production: the test dials with a minted
    // credential and the node stamps `from` from its cert SAN.
    let client = dial_test_daemon(&endpoint);
    // One call provisions BOTH sides: a conversation is indexed under each
    // agent, so there is no separate peer inbox left to create.
    mailbox(&client, &agent, "")
        .ensure_conversation(&peer)
        .expect("provision the conversation");

    // History accumulates before this client has ever existed.
    send_to(&client, &peer, &agent, "ancient history", "").expect("write history");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let config_home = tempfile::tempdir().expect("temp config home");
    let mut acp = spawn_acp(&endpoint, &agent, &peer, &workspace, &config_home);
    acp.request(
        "initialize",
        &json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    acp.request(
        "session/new",
        &json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
    );

    // A live message proves the receiver is working, and its arrival is the
    // point at which "history was not replayed" becomes a fact rather than a
    // race with a slow delivery.
    let live = "sent after the receiver came up";
    std::thread::sleep(Duration::from_millis(500));
    send_to(&client, &peer, &agent, live, "").expect("write a live message");
    let notification = acp.await_notification(|msg| {
        msg["method"] == "session/update"
            && msg["params"]["update"]["sessionUpdate"] == "user_message_chunk"
    });
    let text = notification["params"]["update"]["content"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("update carries text: {notification}"));
    assert!(
        text.contains(live) && !text.contains("ancient history"),
        "the first delivery should be the live message, not replayed history: {text}"
    );
}

fn await_real_receiver_seat(backend: &dyn runtime::FsBackend, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match backend.try_lock(path, 30_000).expect("probe the real seat") {
            None => break,
            Some(probe) => drop(probe),
        }
        assert!(
            Instant::now() < deadline,
            "receiver never acquired its real seat"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
