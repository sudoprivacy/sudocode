//! A refused batch is re-read, and that re-read is now observable.
//!
//! Drives the REAL tail loop over a REAL filesystem rather than calling the
//! recorder directly: the unit tests prove the record holds what it claims, and
//! this proves the delivery path actually reaches it. Those are different
//! claims, and only this one would have caught the loop never being wired up.
//!
//! Motivation: when six agents saw the same message delivered repeatedly, the
//! consumer-refused re-read — the path that produces a byte-identical repeat —
//! was logged nowhere, so the cause had to be recorded as UNKNOWN.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::{delivery_events, DeliveryStall};

fn envelope(from: &str, to: &str, body: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: from.to_string(),
        to: to.to_string(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: "message".to_string(),
        request_id: None,
    }
}

/// A temp root that removes itself even when an assertion panics.
///
/// Cleanup used to be a `remove_dir_all` after the assertions, so every failing
/// run left a whole mailbox tree in `$TMPDIR` — and a test for unbounded growth
/// has no business growing unboundedly itself. A guard runs on unwind too.
struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "delivery-stall-{label}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self(root)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_refused_batch_records_a_consumer_refused_stall_with_the_stuck_offset() {
    let root = TempRoot::new("refused");
    let abort = runtime::HookAbortSignal::default();
    let handed_over = Arc::new(AtomicUsize::new(0));

    // A consumer that always refuses: the real at-least-once boundary, which is
    // exactly the condition the fleet hit during a long turn.
    let seen = Arc::clone(&handed_over);
    let poller = runtime::mailbox::spawn_local_poller(
        root.path(),
        "receiver-under-test".to_string(),
        abort.clone(),
        move |_envelope: &MailboxEnvelope| {
            seen.fetch_add(1, Ordering::Release);
            false
        },
    );

    let sender = runtime::mailbox::Mailbox::workspace_local(root.path(), "peer-sender".to_string());
    sender
        .send(envelope(
            "peer-sender",
            "receiver-under-test",
            "a message that will not be accepted",
        ))
        .expect("send");

    // Wait for the loop to hand the batch over at least twice, which is what
    // "re-read without advancing" means in observable terms.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while handed_over.load(Ordering::Acquire) < 2 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let handovers = handed_over.load(Ordering::Acquire);
    abort.abort();
    let _ = poller.join();

    assert!(
        handovers >= 2,
        "the refused batch should be handed over again; saw {handovers}"
    );

    let stalls: Vec<_> = delivery_events()
        .into_iter()
        .filter(|e| e.peer == "peer-sender" && e.stall == DeliveryStall::ConsumerRefused)
        .collect();
    assert!(
        !stalls.is_empty(),
        "the re-read must be recorded, not silent"
    );
    assert!(
        stalls.iter().all(|e| e.offset == Some(0)),
        "every stall sits at the offset that did not advance: {:?}",
        stalls.iter().map(|e| e.offset).collect::<Vec<_>>()
    );
    assert!(
        stalls.iter().any(|e| e.repeats >= 2),
        "consecutive refusals at one offset are counted: {:?}",
        stalls.iter().map(|e| e.repeats).collect::<Vec<_>>()
    );
    assert!(
        stalls.iter().all(|e| !e.holder.is_empty()),
        "the stall is attributable to the reader that holds the lease"
    );
    assert!(
        stalls.iter().all(|e| e
            .tail
            .zip(e.offset)
            .is_some_and(|(tail, offset)| tail > offset)),
        "tail is known and past the stuck offset, so the pending bytes are: {:?}",
        stalls
            .iter()
            .map(|e| (e.offset, e.tail))
            .collect::<Vec<_>>()
    );
    // Attribution, not just detection. The batch test is
    // `all(|m| reader.is_valid() && sink(m))`, so a lapsed lease ALSO makes it
    // false; recording that as the consumer's refusal would blame the wrong
    // cause for the very repeats this record explains. Here the lease is held
    // throughout — this backend's `renew()` cannot fail — so every stall must
    // be attributed to the consumer and none to the lease.
    assert!(
        !delivery_events()
            .iter()
            .any(|e| e.peer == "peer-sender" && e.stall == DeliveryStall::LeaseLost),
        "a held lease must never be reported as lost: {:?}",
        delivery_events()
            .iter()
            .filter(|e| e.peer == "peer-sender")
            .map(|e| e.stall)
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_peer_appending_during_a_refusal_moves_tail_but_not_offset() {
    // The case `offset` alone cannot describe, driven for real rather than
    // asserted: the consumer refuses throughout, so the cursor is pinned while
    // the peer appends. A reader of the records must be able to see that new
    // content arrived without the cursor moving — otherwise "transport re-hand"
    // and "peer said something else" are indistinguishable, which is the one
    // question this recorder exists to answer.
    let root = TempRoot::new("appending");
    let abort = runtime::HookAbortSignal::default();
    let handed_over = Arc::new(AtomicUsize::new(0));

    let seen = Arc::clone(&handed_over);
    let poller = runtime::mailbox::spawn_local_poller(
        root.path(),
        "pinned-receiver".to_string(),
        abort.clone(),
        move |_envelope: &MailboxEnvelope| {
            seen.fetch_add(1, Ordering::Release);
            false
        },
    );

    let sender = runtime::mailbox::Mailbox::workspace_local(root.path(), "chatty-peer".to_string());
    sender
        .send(envelope("chatty-peer", "pinned-receiver", "first"))
        .expect("send first");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while handed_over.load(Ordering::Acquire) < 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let tail_before = runtime::mailbox::delivery_events()
        .into_iter()
        .filter(|e| e.peer == "chatty-peer")
        .filter_map(|e| e.tail)
        .max()
        .expect("a stall was recorded for the first message");

    // Append while the consumer is still refusing.
    sender
        .send(envelope(
            "chatty-peer",
            "pinned-receiver",
            "second, sent while the first was still being refused",
        ))
        .expect("send second");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut tail_after = tail_before;
    while tail_after <= tail_before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
        tail_after = runtime::mailbox::delivery_events()
            .into_iter()
            .filter(|e| e.peer == "chatty-peer")
            .filter_map(|e| e.tail)
            .max()
            .unwrap_or(tail_before);
    }
    abort.abort();
    let _ = poller.join();

    let stalls: Vec<_> = runtime::mailbox::delivery_events()
        .into_iter()
        .filter(|e| e.peer == "chatty-peer" && e.stall == DeliveryStall::ConsumerRefused)
        .collect();

    assert!(
        tail_after > tail_before,
        "the appended message must show up as a larger tail: {tail_before} -> {tail_after}"
    );
    assert!(
        stalls.iter().all(|e| e.offset == Some(0)),
        "the cursor never advanced, because nothing was ever accepted: {:?}",
        stalls.iter().map(|e| e.offset).collect::<Vec<_>>()
    );
    assert!(
        stalls.iter().filter_map(|e| e.tail).min() < stalls.iter().filter_map(|e| e.tail).max(),
        "so tail is the only field that moved, and it is recorded"
    );
}

#[test]
fn an_accepted_batch_records_no_stall() {
    // The complement: with no failure injected, delivery advances and leaves
    // nothing behind. Without this, the test above would pass even if the code
    // recorded a stall on every poll.
    let root = TempRoot::new("accepted");
    let abort = runtime::HookAbortSignal::default();
    let accepted = Arc::new(AtomicUsize::new(0));

    let seen = Arc::clone(&accepted);
    let poller = runtime::mailbox::spawn_local_poller(
        root.path(),
        "accepting-receiver".to_string(),
        abort.clone(),
        move |_envelope: &MailboxEnvelope| {
            seen.fetch_add(1, Ordering::Release);
            true
        },
    );

    let sender = runtime::mailbox::Mailbox::workspace_local(root.path(), "quiet-peer".to_string());
    sender
        .send(envelope(
            "quiet-peer",
            "accepting-receiver",
            "accepted first time",
        ))
        .expect("send");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while accepted.load(Ordering::Acquire) < 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Give the loop room to poll again after committing, so a spurious stall
    // would have a chance to appear.
    std::thread::sleep(std::time::Duration::from_millis(300));
    abort.abort();
    let _ = poller.join();

    assert_eq!(
        accepted.load(Ordering::Acquire),
        1,
        "an accepted message is delivered exactly once"
    );
    assert!(
        !delivery_events()
            .iter()
            .any(|e| e.peer == "quiet-peer" && e.stall == DeliveryStall::ConsumerRefused),
        "an accepted batch must not record a stall"
    );
}
