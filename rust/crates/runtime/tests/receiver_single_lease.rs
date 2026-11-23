//! A receiver holds one lease, however many conversations it tails.
//!
//! The agent's receiver seat, `agents/<name>/receiver.lock`, is the only lock
//! a receiver asks its backend for. Every conversation is read and committed
//! under it, and each register is protected by its own conditional write. Here
//! one agent with five conversations receives over a backend with a node's
//! expiring leases and conditional writes ([`file_cas::FileCasBackend`]),
//! behind a boundary that records every path a lock is asked for.

#[path = "support/file_cas.rs"]
mod file_cas;
#[path = "common/lease_fault.rs"]
#[expect(
    dead_code,
    reason = "Fault controls are exercised by mailbox_nexus_live and receiver_retry_diagnostics, which share this fixture."
)]
mod lease_fault;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use file_cas::FileCasBackend;
use lease_fault::{FaultyBackend, Probe};
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::cursor_fence::CursorGuarantee;
use runtime::fs_backend::StdFsBackend;
use runtime::mailbox::{cursor_guarantee, spawn_inbox_poller, InboxConvention, Mailbox};
use runtime::HookAbortSignal;

const AGENT: &str = "single-lease-receiver";
const PEERS: [&str; 5] = ["peer-a", "peer-b", "peer-c", "peer-d", "peer-e"];

fn send(convention: &InboxConvention, peer: &str, body: &str) {
    Mailbox::new(Arc::new(StdFsBackend), peer.into(), convention.clone())
        .send(MailboxEnvelope {
            send_id: None,
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: peer.into(),
            to: AGENT.into(),
            body: body.into(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.into(),
            request_id: None,
        })
        .expect("the peer writes");
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn positions(mailbox: &Mailbox) -> Vec<u64> {
    PEERS
        .iter()
        .map(|peer| mailbox.read_position(peer).unwrap().unwrap_or(0))
        .collect()
}

fn all_past(now: &[u64], before: &[u64]) -> bool {
    now.iter().zip(before).all(|(now, before)| now > before)
}

#[test]
fn a_receiver_with_five_conversations_asks_for_no_lock_but_its_seat() {
    let root = runtime::test_support::temp_dir_path("receiver-single-lease");
    std::fs::create_dir_all(&root).unwrap();
    let convention = InboxConvention::new(root.to_string_lossy().into_owned());
    let seat = format!("{}/{AGENT}/receiver.lock", convention.agents_dir());
    let probe = Probe::new();
    let mailbox = Arc::new(Mailbox::new(
        Arc::new(FaultyBackend::new(
            Arc::new(FileCasBackend::new()),
            Arc::clone(&probe),
        )),
        AGENT.into(),
        convention.clone(),
    ));

    // Three conversations exist before the receiver starts; two more appear
    // while it runs.
    for peer in &PEERS[..3] {
        send(&convention, peer, &format!("{peer} first"));
    }
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let stop = HookAbortSignal::new();
    let receiver = {
        let seen = Arc::clone(&seen);
        spawn_inbox_poller(
            Arc::clone(&mailbox),
            50,
            "single-lease",
            stop.clone(),
            move |message| {
                seen.lock().unwrap().push(message.body.clone());
                true
            },
        )
    };
    wait_until("the seat to be held", || probe.holds_seat());
    for peer in &PEERS[3..] {
        send(&convention, peer, &format!("{peer} first"));
    }
    let unread = vec![0; PEERS.len()];
    wait_until("every conversation's first message to be committed", || {
        all_past(&positions(&mailbox), &unread)
    });
    let after_first = positions(&mailbox);

    // Every tail commits again under the same seat.
    for peer in PEERS {
        send(&convention, peer, &format!("{peer} second"));
    }
    wait_until(
        "every conversation's second message to be committed",
        || all_past(&positions(&mailbox), &after_first),
    );
    let after_second = positions(&mailbox);
    assert_eq!(
        cursor_guarantee(AGENT),
        Some(CursorGuarantee::FencedCas),
        "each register is fenced by its own conditional write"
    );
    assert_eq!(
        probe.lock_requests(),
        [seat.as_str()],
        "five tails delivered and committed on one seat claim and no other lock"
    );
    assert!(
        probe.releases().is_empty(),
        "the receiver keeps the seat it took while it runs"
    );

    stop.abort();
    receiver.join().expect("the receiver stops");
    let mut delivered = seen.lock().unwrap().clone();
    delivered.sort();
    let mut expected: Vec<String> = PEERS
        .iter()
        .flat_map(|peer| [format!("{peer} first"), format!("{peer} second")])
        .collect();
    expected.sort();
    assert_eq!(
        delivered, expected,
        "each message on each conversation is delivered once"
    );

    // The one-shot read and its acknowledgement take the same seat, once for
    // all five conversations, and nothing else.
    for peer in PEERS {
        send(&convention, peer, &format!("{peer} third"));
    }
    let pending = mailbox.read_pending().expect("read pending");
    assert_eq!(
        pending.messages().count(),
        PEERS.len(),
        "one unread message per conversation"
    );
    pending.acknowledge().expect("acknowledge");
    assert!(
        all_past(&positions(&mailbox), &after_second),
        "every register advanced past its third message"
    );
    assert_eq!(
        probe.lock_requests(),
        [seat.as_str(), seat.as_str()],
        "a pending read claims the seat once and asks for no other lock"
    );
    assert_eq!(
        probe.acquisitions().len(),
        probe.releases().len(),
        "every seat claim was released"
    );

    std::fs::remove_dir_all(root).unwrap();
}
