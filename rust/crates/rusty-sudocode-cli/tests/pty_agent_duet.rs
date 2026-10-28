//! Two real `scode` processes exchange durable A2A messages over Nexus.
//!
//! The fixture uses one daemon, or two Raft-connected daemons when a peer
//! endpoint is supplied. Both processes authenticate with minted credentials.
//! Conversation layout and indexing follow the shared Mailbox contract.
//!
//! The sender calls `send`; the receiver surfaces its exact message in the
//! REPL and commits its own reader position. The offline journey starts the
//! receiver after the message is stored and verifies that inspecting delivery
//! leaves its unread position intact.
//!
//! Mock models run by default. `SCODE_TEST_BACKEND=live` lets a funded model
//! choose the tool call. `e2e/nexus-a2a/run.sh` supplies the daemon and credentials;
//! absent an endpoint, these tests report that transport acceptance was skipped.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

use std::path::Path;
use std::time::{Duration, Instant};

use common::TestEnv;
use mock_anthropic_service::{UNIFIED_SEND_BODY, UNIFIED_SEND_RECIPIENT};
use runtime::mailbox::Mailbox;

/// The receiver's mailbox identity, for every transport.
///
/// Locally this is not a choice: `run_repl_loop` polls as the coordinator, one
/// fixed name. Over nexus the receiver could carry any name, and carrying this
/// one keeps a single recipient across the table above.
const RECEIVER: &str = UNIFIED_SEND_RECIPIENT;
const GREETING_RECEIPT: &str = "GREETING_RECEIVED";

/// Long enough for a transport hop and a REPL turn under CI load, short enough
/// that a genuine failure is not mistaken for slowness.
const BUDGET: Duration = Duration::from_secs(30);
// Both journeys use the harness's same signed Agent credentials and reader
// registers. Independent fixture processes must not consume each other's mail.
static DUET_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How the two processes reach each other.
enum Transport {
    /// nexus, both processes on the same node.
    OneNode { endpoint: String },
    /// nexus, sender and receiver on different nodes, so every assertion can
    /// only be satisfied by an envelope raft replicated between them.
    TwoNodes { receiver: String, sender: String },
}

impl Transport {
    /// The nexus transports this environment can run.
    ///
    /// The nexus cases need a daemon, and a second endpoint turns the one-node
    /// case into the two-node one — there is no reason to run both against the
    /// same pair of daemons, since two nodes is the stronger statement. The
    /// standalone same-machine (no-daemon) pair is covered deterministically by
    /// `runtime/tests/same_machine_pair.rs`, which needs no PTY or config
    /// plumbing to pin two identities under one pair root.
    fn available() -> Vec<Self> {
        let mut transports = Vec::new();
        let endpoint = std::env::var("NEXUS_A2A_TEST_ENDPOINT")
            .ok()
            .filter(|e| !e.is_empty());
        let peer = std::env::var("NEXUS_A2A_TEST_PEER_ENDPOINT")
            .ok()
            .filter(|e| !e.is_empty());
        match (endpoint, peer) {
            (Some(receiver), Some(sender)) => transports.push(Self::TwoNodes { receiver, sender }),
            (Some(endpoint), None) => transports.push(Self::OneNode { endpoint }),
            (None, _) => {
                eprintln!("SKIP(nexus): set NEXUS_A2A_TEST_ENDPOINT — e2e/nexus-a2a/run.sh does")
            }
        }
        transports
    }

    fn label(&self) -> &'static str {
        match self {
            Self::OneNode { .. } => "nexus, one node",
            Self::TwoNodes { .. } => "nexus, two nodes",
        }
    }

    /// The mock scenario the sender runs. The nexus cases leave `sender` out so
    /// the session's own A2A identity has to answer instead.
    fn scenario(&self) -> &'static str {
        match self {
            _ => "unified_send_roundtrip",
        }
    }
}

/// The receiver's mailbox, as the test reads it.
fn receiver_mailbox(transport: &Transport, _config_home: &Path) -> Mailbox {
    match transport {
        Transport::OneNode { endpoint }
        | Transport::TwoNodes {
            receiver: endpoint, ..
        } => {
            // Cert-only, like production: dial the receiver's node with its
            // minted credential; the node stamps identity from the cert.
            let credential = std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL")
                .expect("set NEXUS_A2A_TEST_RECEIVER_CREDENTIAL=<minted bundle dir>");
            let credential = runtime::nexus_mailbox::AgentCredential::load(&credential)
                .expect("load the receiver credential bundle");
            let client = runtime::nexus_mailbox::Config {
                endpoint: endpoint.to_string(),
                agent: credential.agent.clone(),
                tls: credential.tls,
            }
            .connect()
            .expect("dial the receiver's node");
            // The mTLS dial is the server-verified credential (identity from
            // the cert's SAN), so this is the transport-verified construction
            // production's standalone session uses — not the fail-closed
            // env/delegation one.
            Mailbox::over_nexus_mtls(client, RECEIVER, String::new(), "root")
        }
    }
}

/// Poll the rendered screen for `needle`.
///
/// Not `expect`: that matches the unconsumed byte stream, and iocraft redraws
/// the whole screen on any change, so chrome already on screen can satisfy a
/// stream match before anything happens.
fn expect_on_screen(sess: &mut pty_expect::PtySession, needle: &str) {
    let deadline = Instant::now() + BUDGET;
    loop {
        let screen = sess.render(|s| s.contents());
        if screen.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the receiver's screen never showed {needle:?} within {BUDGET:?}\nPTY:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn one_scode_sends_and_another_surfaces_it_on_every_transport() {
    let _guard = DUET_SERIAL.lock().unwrap();
    for transport in Transport::available() {
        run_duet(&transport, false);
    }
}

#[test]
fn a_message_sent_before_the_receiver_starts_survives_on_every_transport() {
    let _guard = DUET_SERIAL.lock().unwrap();
    for transport in Transport::available() {
        run_duet(&transport, true);
    }
}

fn run_duet(transport: &Transport, offline: bool) {
    // Cert-only: the sender dials with its minted credential, so its A2A name -
    // the `from` the node stamps - is the credential's agent, not a name the
    // test invents. Reading it here keeps the assertion pinned to what shipped.
    let sender_credential = std::env::var("NEXUS_A2A_TEST_SENDER_CREDENTIAL")
        .expect("set NEXUS_A2A_TEST_SENDER_CREDENTIAL=<minted bundle dir>");
    let sender_name = runtime::nexus_mailbox::AgentCredential::load(&sender_credential)
        .expect("load the sender credential bundle")
        .agent;
    let expected_from = sender_name.clone();

    // ── 2. The RECEIVER: a real scode REPL ─────────────────────────────────
    // Started before the inbox is read from, because its cursor is what the
    // send below must not race. One mock server routes the scenario by the
    // marker the sender's prompt carries.
    let env = TestEnv::new("duet");
    let workspace = env.workspace_root().to_path_buf();
    std::fs::write(workspace.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // ── 1. Provision the conversation ──────────────────────────────────
    // A send to a path that is not an append stream fails loudly rather than
    // writing where nothing tails, so provision the pair's transcript first.
    // Either side may do it; here the test does, so the tail below has
    // something to seek in even when the sender has not started.
    let inbox = receiver_mailbox(transport, env.config_home());
    inbox
        .ensure_conversation(&sender_name)
        .expect("provision the conversation");
    let (_history, tail) = inbox
        .poll_conversation(&sender_name, 0, 0)
        .expect("seek the transcript to its tail");
    let initial_position = inbox.read_position(&sender_name).unwrap().unwrap_or(0);

    // Cert-only: the receiver dials with its minted credential; its A2A name
    // comes from the cert, so RECEIVER is the name that bundle must carry.
    let receiver_credential = std::env::var("NEXUS_A2A_TEST_RECEIVER_CREDENTIAL")
        .expect("set NEXUS_A2A_TEST_RECEIVER_CREDENTIAL=<minted bundle dir>");
    let receiver_endpoint = match transport {
        Transport::OneNode { endpoint }
        | Transport::TwoNodes {
            receiver: endpoint, ..
        } => endpoint,
    };
    // The receiver attaches the way a machine does: a node record in its home
    // and a session named for the credential the node minted.
    let receiver_home = node_home::NodeHome::new(
        "duet-receiver",
        receiver_endpoint,
        RECEIVER,
        Path::new(&receiver_credential),
    );
    node_home::pin_agent_name(&workspace, RECEIVER);
    let [receiver_home_var, receiver_profile_var] = receiver_home.env();
    let receiver_env_vars = vec![
        ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        (receiver_home_var.0, receiver_home_var.1.as_str()),
        (receiver_profile_var.0, receiver_profile_var.1.as_str()),
    ];
    let greeting_task = format!(
        "When another agent sends a greeting, write exactly {GREETING_RECEIPT} \
         in your own conversation. Do not call tools or send another message \
         for that greeting."
    );
    let start_receiver = || {
        let mut receiver = env.spawn_with_env(
            &[
                "--permission-mode",
                "read-only",
                "--append-system-prompt",
                &greeting_task,
            ],
            &receiver_env_vars,
        );
        receiver.set_default_timeout(BUDGET);
        receiver
            .expect("❯")
            .expect("the receiver's REPL should start");
        receiver
    };
    let mut receiver = if offline {
        None
    } else {
        Some(start_receiver())
    };
    // No wait for a read position here. It used to block until the receiver
    // had written its cursor file, because a first-ever read seeked to the
    // tail and anything sent before that was skipped. A first read now starts
    // at the beginning of the conversation, so a message sent before the
    // receiver has discovered it is still delivered - and the file that wait
    // watched for is gone, the position having moved into the conversation.

    // ── 3. The SENDER: another real scode, whose model calls `send` ────────
    // Its own workspace and home: on a node a session's identity is its own
    // name, so two sessions sharing a workspace would be one agent.
    let sender_env = env.peer("duet-sender");
    let prompt = sender_env.prompt(
        &format!(
            "Use send to message {RECEIVER}. The message body must be exactly \
             \"hello from unified send\" (lowercase, without a period). \
             Use summary exactly \"greeting test\".",
        ),
        transport.scenario(),
    );
    let sender_endpoint = match transport {
        Transport::OneNode { endpoint }
        | Transport::TwoNodes {
            sender: endpoint, ..
        } => endpoint,
    };
    let sender_home = node_home::NodeHome::new(
        "duet-sender",
        sender_endpoint,
        &sender_name,
        Path::new(&sender_credential),
    );
    node_home::pin_agent_name(sender_env.workspace_root(), &sender_name);
    let [sender_home_var, sender_profile_var] = sender_home.env();
    let sender_env_vars = vec![
        (sender_home_var.0, sender_home_var.1.as_str()),
        (sender_profile_var.0, sender_profile_var.1.as_str()),
    ];
    // No `--allowedTools send`. Narrowing the tool set changes which channel the
    // proxy gateway picks: with one tool allowed, `api.sudorouter.ai` answers
    // `500 … 分组 auto 下模型 <model> 的可用渠道不存在` on every retry, for models
    // it serves fine with the full set. The flag is not load-bearing — the mock
    // scenario issues the call and the live prompt names it.
    let mut sending = sender_env.spawn_with_env(
        &["--permission-mode", "workspace-write", &prompt],
        &sender_env_vars,
    );
    sending.set_default_timeout(BUDGET);
    let exit = match sending.expect_eof() {
        Ok(code) => code,
        Err(error) => {
            // A gateway that will not route this model says nothing about
            // `send`, and a live test that reports that as a product failure is
            // worse than one that says it could not run. Mock mode never
            // reaches here.
            let tail = common::screen_tail(&sending, 1200);
            assert!(
                common::model_unavailable_in_screen(&tail),
                "[{}] the sender did not exit: {error}\nscreen tail:\n{tail}",
                transport.label()
            );
            eprintln!(
                "SKIP({}): the proxy could not serve the model",
                transport.label()
            );
            return;
        }
    };
    assert_eq!(
        exit,
        0,
        "[{}] the sender should exit 0 after sending:\n{}",
        transport.label(),
        common::screen_tail(&sending, 1600)
    );
    if offline {
        assert_eq!(
            inbox.read_position(&expected_from).unwrap().unwrap_or(0),
            initial_position
        );
        // Append acknowledges Raft commit; the receiver's node can still be
        // applying it. Use the existing delivery budget and blocking tail.
        let (stored, _) = inbox
            .poll_conversation(
                &expected_from,
                tail,
                u64::try_from(BUDGET.as_millis()).unwrap(),
            )
            .unwrap();
        assert!(
            stored.iter().any(|message| message.from == expected_from),
            "[{}] no durable envelope from {expected_from}: {stored:?}\nsender:\n{}",
            transport.label(),
            common::screen_tail(&sending, 6000)
        );
        assert_eq!(
            inbox.read_position(&expected_from).unwrap().unwrap_or(0),
            initial_position,
            "inspecting durable delivery must not acknowledge it for the offline receiver"
        );
        receiver = Some(start_receiver());
    }
    let mut receiver = receiver.expect("receiver starts after durable delivery");

    // The sender is on nexus, so nothing may be written to its workspace: the
    // failure being guarded is the ambient fallback writing a local file while
    // the tool still answers "sent". "Arrived" has to mean it arrived at the
    // daemon and nowhere else.
    let workspace_inbox =
        runtime::agent_mailbox::inbox_path_under(sender_env.workspace_root(), RECEIVER);
    assert!(
        !workspace_inbox.exists(),
        "[{}] the sender is on nexus, so nothing may be written to {}",
        transport.label(),
        workspace_inbox.display()
    );

    // ── 4. The receiver surfaces it ────────────────────────────────────────
    // The REPL frames a peer message under `Message from <name>`. Seeing
    // it is the whole chain: the tool resolved the recipient's path, the
    // transport carried the envelope, the parked receiver woke, and the
    // coordinator loop took the message.
    expect_on_screen(&mut receiver, &format!("Message from {expected_from}"));
    expect_on_screen(&mut receiver, "hello from unified send");

    // The receiver's OWN read position must advance past what it consumed.
    // Distinct from the offset this test reads below: that one is the test's
    // cursor, this one is the receiver's, and a receiver that surfaces a message
    // without recording it re-delivers the same message forever — which is how
    // a re-reply storm starts.
    let deadline = Instant::now() + BUDGET;
    loop {
        let recorded = inbox
            .read_position(&expected_from)
            .expect("read the receiver's position")
            .unwrap_or(0);
        let confirmed =
            !env.is_live() || common::screen_tail(&receiver, 8000).contains(GREETING_RECEIPT);
        if recorded > tail && confirmed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "[{}] the receiver did not finish the greeting and advance its read \
             position past {tail} in {} (recorded={recorded})\nreceiver:\n{}\nsender:\n{}",
            transport.label(),
            inbox.transcript_path(&expected_from),
            common::screen_tail(&receiver, 8000),
            common::screen_tail(&sending, 3000)
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // ── 5. Check what crossed ──────────────────────────────────────────────
    // The screen proves delivery; the mailbox proves the contents. Read from
    // the tail snapshot so this is about this run.
    let (envelopes, next) = inbox
        .poll_conversation(&expected_from, tail, 0)
        .expect("read the conversation");
    let delivered = envelopes
        .iter()
        .find(|e| e.from == expected_from)
        .unwrap_or_else(|| {
            panic!(
                "[{}] the receiver surfaced the message but no envelope from \
                 {expected_from} is in {RECEIVER}'s inbox — got {envelopes:?}",
                transport.label()
            )
        });
    assert!(
        next > tail,
        "[{}] the read position must advance past a consumed message, or the next \
         poll re-delivers it forever",
        transport.label()
    );
    assert_eq!(
        delivered.body,
        if env.is_mock() {
            UNIFIED_SEND_BODY
        } else {
            "hello from unified send"
        },
        "[{}] the body the tool accepted must be the body that crossed",
        transport.label()
    );
    assert_eq!(
        delivered.summary.as_deref().map(str::trim),
        Some("greeting test"),
        "[{}] a summary the tool accepted must survive the crossing: {delivered:?}",
        transport.label()
    );

    // Both journeys reuse the signed identity. PtySession::drop kills the
    // receiver, leaving its remote reader lease alive for 30 seconds — the
    // next journey's entire delivery budget. Exit through the real REPL path
    // so it joins the pollers and releases the seat before this fixture ends.
    receiver.send("/exit\r").expect("exit receiver normally");
    assert_eq!(
        receiver.expect_eof().expect("receiver released its reader"),
        0
    );

    eprintln!(
        "[{}] {expected_from} -> {RECEIVER}, surfaced in the receiver's REPL [{}]",
        transport.label(),
        if env.is_mock() { "mock" } else { "live" },
    );
}
