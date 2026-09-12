//! Live E2E: a nexus-A2A peer message reaches an ACP client.
//!
//! The send half of standalone A2A has worked since #508 — `scode` advertises
//! a `send_message` tool and writes to the peer's replicated DT_STREAM inbox.
//! The receive half was only ever wired into the interactive REPL, which
//! printed `📨 A2A from <peer>` to the terminal. Anything driving `scode acp`
//! programmatically — which is what an agent does, and the reason the ACP cut
//! exists — saw nothing at all. A duet between two such agents could send and
//! never receive.
//!
//! This proves the other half over a real daemon and a real DT_STREAM: a peer
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
//! Loopback + no-TLS is the auth-off plane, where the stamping hook is
//! fail-open and the authored `from` survives, so the assertion can pin the
//! sender exactly.

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
    Mailbox::over_nexus(Arc::clone(client), agent, auth)
}

fn send_to(
    client: &Arc<NexusVfsClient>,
    from_agent: &str,
    to_agent: &str,
    body: &str,
    auth: &str,
) -> Result<(), String> {
    mailbox(client, from_agent, auth).send(MailboxEnvelope {
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
}

impl AcpStdio {
    fn request(&mut self, method: &str, params: Value) -> Value {
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
    // An isolated config home needs a config: the point of isolating it is the
    // read cursor, not to test scode's behaviour without providers.
    std::fs::write(
        config_home.path().join("sudocode.json"),
        runtime::SAMPLE_SUDOCODE_JSON,
    )
    .expect("seed the isolated config home");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.arg("acp")
        .current_dir(workspace.path())
        .env("SUDO_CODE_CONFIG_HOME", config_home.path())
        .env("NEXUS_A2A_ENDPOINT", endpoint)
        .env("NEXUS_A2A_AGENT", agent)
        .env("NEXUS_A2A_PEER", peer)
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
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial the daemon"));
    mailbox(&client, SELF_AGENT, "")
        .ensure_inbox()
        .expect("provision the agent inbox");
    mailbox(&client, PEER_AGENT, "")
        .ensure_inbox()
        .expect("provision the peer inbox");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let config_home = tempfile::tempdir().expect("temp config home");
    let mut acp = spawn_acp(&endpoint, SELF_AGENT, PEER_AGENT, &workspace, &config_home);

    let init = acp.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    assert!(init.get("result").is_some(), "initialize failed: {init}");

    // The receiver starts with the first session — before one exists there is
    // nobody to notify.
    let new_session = acp.request(
        "session/new",
        json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
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

    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial the daemon"));
    mailbox(&client, &agent, "")
        .ensure_inbox()
        .expect("provision the agent inbox");
    mailbox(&client, &peer, "")
        .ensure_inbox()
        .expect("provision the peer inbox");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let config_home = tempfile::tempdir().expect("temp config home");

    // First run: establishes the cursor, then goes away.
    let cursor_file = config_home.path().join(format!("a2a-cursor-{agent}"));
    {
        let mut acp = spawn_acp(&endpoint, &agent, &peer, &workspace, &config_home);
        acp.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        acp.request(
            "session/new",
            json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
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
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    acp.request(
        "session/new",
        json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
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

    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial the daemon"));
    mailbox(&client, &agent, "")
        .ensure_inbox()
        .expect("provision the agent inbox");
    mailbox(&client, &peer, "")
        .ensure_inbox()
        .expect("provision the peer inbox");

    // History accumulates before this client has ever existed.
    send_to(&client, &peer, &agent, "ancient history", "").expect("write history");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let config_home = tempfile::tempdir().expect("temp config home");
    let mut acp = spawn_acp(&endpoint, &agent, &peer, &workspace, &config_home);
    acp.request(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    acp.request(
        "session/new",
        json!({"cwd": workspace.path().to_string_lossy(), "mcpServers": []}),
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
