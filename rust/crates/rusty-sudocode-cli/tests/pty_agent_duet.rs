//! Two `scode` processes converse, over every transport a message can take.
//!
//! ## One workflow, two nexus transports
//!
//! | transport | path an envelope takes | reach |
//! |---|---|---|
//! | nexus, one node | `/agents/<to>/chat-with-me` on that node | one machine |
//! | nexus, two nodes | the same stream, replicated by raft | the cross-machine case |
//!
//! The standalone same-machine (no-daemon) pair is covered deterministically by
//! `runtime/tests/same_machine_pair.rs`; this test is the nexus, real-binary
//! duet.
//!
//! The steps and the dependencies between them do not change with the
//! transport, so this is one test rather than near-copies that would
//! drift. What changes is which mailbox the assertions read and, for the local
//! case, how the sender gets a name — see [`Transport`].
//!
//! ## What it covers that nothing else does
//!
//! `send_message_routing.rs` proves which destination a recipient's name
//! resolves to, against a recording backend, and covers the tool's spellings.
//! `mailbox_nexus_live` proves a real `nexusd-cluster` moves an envelope and
//! wakes a parked tail, by driving `Mailbox` directly. Neither runs what a user
//! runs: two real binaries, one calling the `send` tool and one whose REPL
//! receiver is parked waiting.
//!
//! That seam is where the failure this path exists because of lived — a `send`
//! that answered "Message sent to <peer>'s inbox" while writing a local file —
//! so a test that drives the transport but not the tool, or the tool but not
//! the receiver, is blind to half of it.
//!
//! ## The workflow
//!
//! 1. provision the receiver's inbox — over nexus a send to an unprovisioned
//!    stream fails, so this is a prerequisite rather than setup
//! 2. start the RECEIVER: a real `scode` REPL, and wait until it has recorded
//!    where it starts reading. A first-ever receiver seeks to the tail rather
//!    than replaying a backlog, so a message sent before that seek lands behind
//!    the cursor and is never delivered — establishing "listening" is part of
//!    the workflow
//! 3. run the SENDER: another real `scode`, whose model calls `send`
//! 4. the receiver's screen shows it, which is the whole chain: the tool
//!    resolved the peer's path, the transport carried it, the parked receiver
//!    woke, and the REPL surfaced it
//! 5. read the envelope back and check what crossed — `from`, the summary, and
//!    that the cursor advanced past a consumed message
//!
//! Step 4 is the one no client-side read can stand in for: a delivered message
//! nobody surfaces looks like success from the sender's side.
//!
//! ## Dual mode
//!
//! Mock by default (CI-safe, no key), live under `SCODE_TEST_BACKEND=live` with
//! a real model choosing to call the tool. The nexus transports need a daemon:
//! `e2e/nexus-a2a/run.sh` sets `NEXUS_A2A_TEST_ENDPOINT`, and
//! `e2e/nexus-a2a/run-cross-node.sh` adds `NEXUS_A2A_TEST_PEER_ENDPOINT` for the
//! two-node case. Absent a daemon those cases return early rather than
//! pretending to have checked; the workspace-file case needs nothing and always
//! runs.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::TestEnv;
use mock_anthropic_service::{UNIFIED_SEND_BODY, UNIFIED_SEND_RECIPIENT};
use nexus_vfs_client::NexusVfsClient;
use runtime::mailbox::Mailbox;

/// The receiver's mailbox identity, for every transport.
///
/// Locally this is not a choice: `run_repl_loop` polls as the coordinator, one
/// fixed name. Over nexus the receiver could carry any name, and carrying this
/// one keeps a single recipient across the table above.
const RECEIVER: &str = UNIFIED_SEND_RECIPIENT;

/// Long enough for a transport hop and a REPL turn under CI load, short enough
/// that a genuine failure is not mistaken for slowness.
const BUDGET: Duration = Duration::from_secs(30);

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

/// An agent name no other run can be using.
///
/// The nexus cases match the envelope on it, and it reaches the binary only as
/// `NEXUS_A2A_AGENT` — so an envelope carrying it can only have been written by
/// that process, through the tool, over that transport.
fn unique_sender_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "scode-sender-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The receiver's mailbox, as the test reads it.
fn receiver_mailbox(transport: &Transport, _config_home: &Path) -> Mailbox {
    match transport {
        Transport::OneNode { endpoint }
        | Transport::TwoNodes {
            receiver: endpoint, ..
        } => Mailbox::over_nexus(
            Arc::new(NexusVfsClient::connect(endpoint).expect("dial the receiver's node")),
            RECEIVER,
            String::new(),
        ),
    }
}

/// Where the receiver records its read position, which is how the test knows it
/// is listening rather than still seeking. Config home: an A2A identity outlives
/// any one directory.
fn receiver_cursor(_transport: &Transport, config_home: &Path) -> PathBuf {
    config_home.join(format!("a2a-cursor-{RECEIVER}"))
}

/// Block until `path` exists.
fn wait_for_file(path: &Path, what: &str) {
    let deadline = Instant::now() + BUDGET;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{what} never appeared at {} — the receiver is not listening, so anything \
             sent now would be missed rather than delivered",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(100));
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
    for transport in Transport::available() {
        run_duet(&transport);
    }
}

fn run_duet(transport: &Transport) {
    let sender_name = unique_sender_name();
    let expected_from = sender_name.clone();

    // ── 2. The RECEIVER: a real scode REPL ─────────────────────────────────
    // Started before the inbox is read from, because its cursor is what the
    // send below must not race. One mock server routes the scenario by the
    // marker the sender's prompt carries.
    let env = TestEnv::new("duet");
    let workspace = env.workspace_root().to_path_buf();
    std::fs::write(workspace.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    // ── 1. Provision the receiver's inbox ──────────────────────────────────
    // A send to a path that is not an append stream fails loudly rather than
    // writing where nothing tails, so provision the receiver's stream first.
    let inbox = receiver_mailbox(transport, env.config_home());
    inbox
        .ensure_inbox()
        .expect("provision the receiver's inbox");
    let (_history, tail) = inbox.poll(0, 0).expect("seek the inbox to its tail");

    let mut receiver_env_vars = vec![("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")];
    if let Transport::OneNode { endpoint }
    | Transport::TwoNodes {
        receiver: endpoint, ..
    } = transport
    {
        receiver_env_vars.push(("NEXUS_A2A_ENDPOINT", endpoint.as_str()));
        receiver_env_vars.push(("NEXUS_A2A_AGENT", RECEIVER));
        receiver_env_vars.push(("NEXUS_A2A_PEER", sender_name.as_str()));
    }
    let mut receiver = env.spawn_with_env(&["--permission-mode", "read-only"], &receiver_env_vars);
    receiver.set_default_timeout(BUDGET);
    receiver
        .expect("❯")
        .expect("the receiver's REPL should start");
    wait_for_file(
        &receiver_cursor(transport, env.config_home()),
        "the receiver's read position",
    );

    // ── 3. The SENDER: another real scode, whose model calls `send` ────────
    // Locally it must share the receiver's workspace, because that is what the
    // local convention is keyed on. Over nexus it gets its own.
    let prompt = env.prompt(
        &format!(
            "Send a message to {RECEIVER} saying hello from unified send. \
             Use summary \"greeting test\".",
        ),
        transport.scenario(),
    );
    let mut sender_env_vars = Vec::new();
    match transport {
        Transport::OneNode { endpoint }
        | Transport::TwoNodes {
            sender: endpoint, ..
        } => {
            sender_env_vars.push(("NEXUS_A2A_ENDPOINT", endpoint.as_str()));
            sender_env_vars.push(("NEXUS_A2A_AGENT", sender_name.as_str()));
            sender_env_vars.push(("NEXUS_A2A_PEER", RECEIVER));
        }
    }
    // No `--allowedTools send`. Narrowing the tool set changes which channel the
    // proxy gateway picks: with one tool allowed, `api.sudorouter.ai` answers
    // `500 … 分组 auto 下模型 <model> 的可用渠道不存在` on every retry, for models
    // it serves fine with the full set. The flag is not load-bearing — the mock
    // scenario issues the call and the live prompt names it.
    let mut sending = env.spawn_with_env(
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
        "[{}] the sender should exit 0 after sending",
        transport.label()
    );

    // The sender is on nexus, so nothing may be written to the workspace: the
    // failure being guarded is the ambient fallback writing a local file while
    // the tool still answers "sent". "Arrived" has to mean it arrived at the
    // daemon and nowhere else.
    let workspace_inbox = runtime::agent_mailbox::inbox_path_under(&workspace, RECEIVER);
    assert!(
        !workspace_inbox.exists(),
        "[{}] the sender is on nexus, so nothing may be written to {}",
        transport.label(),
        workspace_inbox.display()
    );

    // ── 4. The receiver surfaces it ────────────────────────────────────────
    // The REPL announces a peer message as `📨 A2A from <name>: <body>`. Seeing
    // it is the whole chain: the tool resolved the recipient's path, the
    // transport carried the envelope, the parked receiver woke, and the
    // coordinator loop took the message.
    expect_on_screen(&mut receiver, &format!("A2A from {expected_from}"));
    expect_on_screen(&mut receiver, "hello from unified send");

    // The receiver's OWN read position must advance past what it consumed.
    // Distinct from the offset this test reads below: that one is the test's
    // cursor, this one is the receiver's, and a receiver that surfaces a message
    // without recording it re-delivers the same message forever — which is how
    // a re-reply storm starts.
    let cursor_file = receiver_cursor(transport, env.config_home());
    let deadline = Instant::now() + BUDGET;
    loop {
        let recorded = std::fs::read_to_string(&cursor_file)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if recorded > tail {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "[{}] the receiver surfaced the message but never advanced its own read              position past {tail} in {}",
            transport.label(),
            cursor_file.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // ── 5. Check what crossed ──────────────────────────────────────────────
    // The screen proves delivery; the mailbox proves the contents. Read from
    // the tail snapshot so this is about this run.
    let (envelopes, next) = inbox.poll(tail, 0).expect("read the receiver's inbox");
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

    eprintln!(
        "[{}] {expected_from} -> {RECEIVER}, surfaced in the receiver's REPL [{}]",
        transport.label(),
        if env.is_mock() { "mock" } else { "live" },
    );
}
