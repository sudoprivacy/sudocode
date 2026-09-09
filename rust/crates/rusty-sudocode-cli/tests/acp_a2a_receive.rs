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
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_vfs_client::NexusVfsClient;
use runtime::nexus_mailbox::{ensure_inbox, send};
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
    stdout: BufReader<ChildStdout>,
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
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read from scode acp");
        assert!(read > 0, "scode acp closed its stdout unexpectedly");
        serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("non-JSON line from acp: {e}: {line}"))
    }

    /// Read notifications until one satisfies `matches`, or the budget expires.
    ///
    /// Waits on the transport rather than issuing requests to sweep for
    /// notifications: what is under test is precisely whether the server can
    /// speak to the client with nothing in flight.
    fn await_notification(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + NOTIFICATION_BUDGET;
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            let msg = self.read_message();
            if msg.get("method").is_some() && matches(&msg) {
                return msg;
            }
            seen.push(msg);
        }
        panic!("no matching notification within {NOTIFICATION_BUDGET:?}; saw {seen:#?}");
    }
}

impl Drop for AcpStdio {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    ensure_inbox(&client, SELF_AGENT, "").expect("provision the agent inbox");
    ensure_inbox(&client, PEER_AGENT, "").expect("provision the peer inbox");

    let workspace = tempfile::tempdir().expect("temp workspace");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scode"));
    cmd.arg("acp")
        .current_dir(workspace.path())
        .env("NEXUS_A2A_ENDPOINT", &endpoint)
        .env("NEXUS_A2A_AGENT", SELF_AGENT)
        .env("NEXUS_A2A_PEER", PEER_AGENT)
        .env("SUDOCODE_INTERRUPT_QUEUE_MODE", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().expect("spawn scode acp");
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = child.stdout.take().expect("stdout piped");
    let mut acp = AcpStdio {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        next_id: 0,
    };

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
    send(&client, PEER_AGENT, SELF_AGENT, body, "").expect("peer writes to the agent inbox");

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
