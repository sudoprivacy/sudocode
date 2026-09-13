//! The `send` tool over a real daemon, and the reply that comes back.
//!
//! ## What this covers that nothing else does
//!
//! `send_message_routing.rs` proves the destination a recipient's name resolves
//! to, against a recording backend. `mailbox_nexus_live` proves a real
//! `nexusd-cluster` moves an envelope, by calling `Mailbox` directly. Neither
//! touches the seam between them: the **tool**, in the **real binary**, over the
//! **real transport**. That gap is the shape of the bug this whole path exists
//! because of — a `send` that answered "Message sent to <peer>'s inbox" while
//! writing a local file — so a test that exercises the transport but not the
//! tool is structurally blind to it.
//!
//! ## The workflow
//!
//! A two-agent exchange, which is the thing A2A is for:
//!
//! 1. provision both inboxes on the daemon — a send to an unprovisioned stream
//!    fails, so this is a prerequisite, not decoration
//! 2. snapshot the peer's tail, so the assertion sees this run's message and not
//!    a previous run's
//! 3. run the real `scode` binary with nexus configured; the model calls `send`
//! 4. read the peer's inbox **from the snapshot** through the daemon, and take
//!    the `from` the envelope carries
//! 5. reply to **that** `from`, and read the sender's own inbox
//!
//! Each step needs the one before it: the cursor from 2 is what makes 4's answer
//! this run's, and the `from` from 4 is the only thing 5 addresses. Step 5 is
//! there because `from` is an address — the convention turns it straight back
//! into a path — so an envelope whose `from` is wrong is delivered and
//! unanswerable, which looks like success from the sending side.
//!
//! ## Dual mode, and two topologies
//!
//! Mock by default (CI-safe, no key), live under `SCODE_TEST_BACKEND=live` with
//! a real model choosing to call the tool. Both need a daemon:
//! `NEXUS_A2A_TEST_ENDPOINT` is set by `e2e/nexus-a2a/run.sh`, and without it
//! this returns early rather than pretending to have checked.
//!
//! `NEXUS_A2A_TEST_PEER_ENDPOINT` puts the `scode` process on a DIFFERENT node
//! from the one this test reads, so step 4 can only be satisfied by an envelope
//! that crossed a raft boundary — the cross-machine case, on one runner.
//! `e2e/nexus-a2a/run-cross-node.sh` sets both. The steps are unchanged: the
//! topology decides which node each side holds, not what is being proven.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::TestEnv;
use nexus_vfs_client::NexusVfsClient;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::Mailbox;

/// The recipient the mock scenario addresses, from the scenario itself.
///
/// Live mode is told the same name, so one workflow describes both backends —
/// and taking it from the mock means the test cannot end up watching a stream
/// the scenario stopped writing to.
const PEER: &str = mock_anthropic_service::UNIFIED_SEND_RECIPIENT;

/// Long enough for a daemon round trip under CI load, short enough that a
/// genuine failure is not mistaken for slowness.
const ARRIVAL_BUDGET: Duration = Duration::from_secs(20);

/// The node this test reads and provisions on.
fn daemon_endpoint() -> Option<String> {
    std::env::var("NEXUS_A2A_TEST_ENDPOINT")
        .ok()
        .filter(|e| !e.is_empty())
}

/// The node the `scode` process connects to, when it is a different one.
///
/// Set it and the same workflow becomes a cross-node one: `scode` sends through
/// its own node while this test reads the peer's inbox on the OTHER node, so the
/// envelope has to cross a raft boundary to satisfy step 4. Unset, both sides
/// share a node and the workflow is the single-node case.
///
/// One test rather than two, because the steps and their dependencies do not
/// change with the topology — only which node each side is holding.
fn scode_endpoint(reader: &str) -> String {
    std::env::var("NEXUS_A2A_TEST_PEER_ENDPOINT")
        .ok()
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| reader.to_string())
}

/// An agent name no other run can be using.
///
/// The sender's identity has to be unique because step 5 reads its inbox: a
/// name shared with a concurrent run would let that run's reply satisfy this
/// one's assertion.
fn unique_sender() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "scode-sender-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn mailbox_for(client: &Arc<NexusVfsClient>, agent: &str) -> Mailbox {
    Mailbox::over_nexus(Arc::clone(client), agent, String::new())
}

/// Poll `agent`'s inbox from `cursor` until an envelope satisfies `want`.
///
/// Polls because this side is a plain client rather than the parked receiver the
/// REPL runs; what is under test is that the envelope arrives at all, not how
/// fast the tail wakes (`mailbox_nexus_live::live_blocking_read_wakes_on_write`
/// owns that). Returns the envelope so the caller can read the fields it needs.
fn await_envelope(
    client: &Arc<NexusVfsClient>,
    agent: &str,
    cursor: u64,
    want: impl Fn(&MailboxEnvelope) -> bool,
) -> Option<MailboxEnvelope> {
    let deadline = Instant::now() + ARRIVAL_BUDGET;
    while Instant::now() < deadline {
        if let Ok((envelopes, _)) = mailbox_for(client, agent).poll(cursor, 0) {
            if let Some(found) = envelopes.into_iter().find(&want) {
                return Some(found);
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    None
}

#[test]
fn a_send_crosses_the_daemon_and_the_peer_can_reply_to_its_sender() {
    let Some(endpoint) = daemon_endpoint() else {
        eprintln!("SKIP: set NEXUS_A2A_TEST_ENDPOINT (e2e/nexus-a2a/run.sh does)");
        return;
    };
    let client = Arc::new(NexusVfsClient::connect(&endpoint).expect("dial the daemon"));
    let sender = unique_sender();
    let sender_node = scode_endpoint(&endpoint);

    // ── 1. Provision both inboxes ──────────────────────────────────────────
    // A send to a path that is not an append stream fails loudly rather than
    // writing where nothing tails, so the peer's inbox has to exist first. The
    // sender's has to exist for step 5 to have somewhere to land.
    mailbox_for(&client, PEER)
        .ensure_inbox()
        .expect("provision the peer's inbox");
    mailbox_for(&client, &sender)
        .ensure_inbox()
        .expect("provision the sender's inbox");

    // ── 2. Snapshot the peer's tail ────────────────────────────────────────
    // `PEER` is a fixed name the mock scenario addresses, so its stream carries
    // earlier runs. Reading from this offset is what makes step 4 about this run.
    let (_history, peer_tail) = mailbox_for(&client, PEER)
        .poll(0, 0)
        .expect("seek the peer's inbox to its tail");

    // ── 3. Run the real binary, nexus configured ───────────────────────────
    let env = TestEnv::new("a2a-send-over-nexus");
    let prompt = env.prompt(
        &format!(
            "Send a message to {PEER} saying hello from unified send. \
             Use summary \"greeting test\"."
        ),
        "unified_send_roundtrip",
    );
    // No `--allowedTools send`. Narrowing the tool set changes which channel the
    // proxy gateway picks: with one tool allowed, `api.sudorouter.ai` answers
    // `500 … 分组 auto 下模型 <model> 的可用渠道不存在` on every retry, for
    // models it serves fine with the full set. The flag is not load-bearing here
    // — the mock scenario issues the call and the live prompt names it — and
    // dropping it is what lets one workflow describe both backends.
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "workspace-write", &prompt],
        &[
            ("NEXUS_A2A_ENDPOINT", sender_node.as_str()),
            ("NEXUS_A2A_AGENT", sender.as_str()),
            ("NEXUS_A2A_PEER", PEER),
        ],
    );
    sess.set_default_timeout(Duration::from_secs(60));
    let exit = match sess.expect_eof() {
        Ok(code) => code,
        Err(error) => {
            // A gateway that will not route this model says nothing about `send`,
            // and a live test that reports that as a product failure is worse
            // than one that says it could not run. Returning rather than exiting
            // the process, so a sibling test in this binary still reports itself.
            // Mock mode never reaches here.
            let tail = common::screen_tail(&sess, 1200);
            assert!(
                common::model_unavailable_in_screen(&tail),
                "scode did not exit: {error}\nscreen tail:\n{tail}"
            );
            eprintln!("SKIP: the proxy could not serve the model:\n{tail}");
            return;
        }
    };
    assert_eq!(exit, 0, "scode should exit 0 after sending");

    // The workspace inbox must stay empty. This is the failure being guarded:
    // the ambient fallback writes `.sudocode-inbox/<peer>.jsonl` and the tool
    // still answers with success, so "the envelope arrived" has to mean it
    // arrived at the daemon and nowhere else.
    let workspace_inbox = env
        .workspace_root()
        .join(runtime::mailbox::LOCAL_INBOX_DIR)
        .join(format!("{PEER}.jsonl"));
    assert!(
        !workspace_inbox.exists(),
        "the session is on nexus, so nothing may be written to {}",
        workspace_inbox.display()
    );

    // ── 4. Read the peer's inbox through the daemon ─────────────────────────
    let delivered =
        await_envelope(&client, PEER, peer_tail, |e| e.from == sender).unwrap_or_else(|| {
            panic!(
                "no envelope from {sender} reached {PEER}'s stream within {ARRIVAL_BUDGET:?} \
                 — the tool reported success, so it went somewhere else"
            )
        });
    assert!(
        delivered.body.contains("unified send"),
        "the body the tool accepted must be the body that crossed: {delivered:?}"
    );
    assert_eq!(
        delivered.summary.as_deref().map(str::trim),
        Some("greeting test"),
        "a summary the tool accepted must survive the crossing: {delivered:?}"
    );

    // `from` is what step 5 addresses, and the only reason this assertion is
    // separate from the one above: a reply is sent to the name the envelope
    // carries, so a stamped-in default would be delivered and unanswerable.
    //
    // It is also what makes this test impossible to satisfy by accident. `sender`
    // is generated here and reaches the binary only as `NEXUS_A2A_AGENT`, so an
    // envelope on the daemon carrying it can only have been written by that
    // process, through the tool, over this transport.
    assert_eq!(
        delivered.from, sender,
        "the envelope must name the session's own identity, not a default"
    );

    // Printed so a CI log shows what crossed rather than only that something did,
    // naming both nodes so a cross-node run is distinguishable from a
    // single-node one at a glance.
    eprintln!(
        "crossed {}: from={} to={} at {} (sent via {sender_node}, read on {endpoint}, {})",
        if sender_node == endpoint {
            "the daemon"
        } else {
            "a raft boundary"
        },
        delivered.from,
        delivered.to,
        mailbox_for(&client, PEER).own_inbox_path(),
        if env.is_mock() { "mock" } else { "live" },
    );

    // ── 5. Reply to the sender the envelope named ──────────────────────────
    // Addressed from `delivered.from`, never from `sender` — deriving the reply
    // path from the envelope is the contract, and using the local variable here
    // would assert nothing about what crossed.
    let reply_body = format!("reply to {} from {PEER}", delivered.from);
    mailbox_for(&client, PEER)
        .send(MailboxEnvelope {
            from: PEER.to_string(),
            to: delivered.from.clone(),
            body: reply_body.clone(),
            summary: Some("reply".to_string()),
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .expect("reply to the address the envelope gave");

    // Read back on the SENDER's node, which is where a real sender would be
    // parked. In the cross-node topology that sends the reply back over the raft
    // boundary it just came across, so both directions are covered rather than
    // only the outbound one; in the single-node case it is the same client.
    let sender_side = if sender_node == endpoint {
        Arc::clone(&client)
    } else {
        Arc::new(NexusVfsClient::connect(&sender_node).expect("dial the sender's node"))
    };
    let answered = await_envelope(&sender_side, &sender, 0, |e| e.body == reply_body)
        .unwrap_or_else(|| {
            panic!(
                "the reply addressed to {} never reached {sender}'s own inbox on {sender_node} — \
                 a `from` that is not the sender's name is delivered and unanswerable",
                delivered.from
            )
        });
    assert_eq!(
        answered.from, PEER,
        "the reply must name its own sender: {answered:?}"
    );
}
