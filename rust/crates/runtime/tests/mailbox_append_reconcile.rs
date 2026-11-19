//! A framed append that returns an error may still have committed. `send`
//! looks for the exact envelope in the conversation before it reports, queues
//! a send it cannot confirm, and a retry of it reaches the receiver once.
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kernel::kernel::Kernel;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::{FsDirEntry, FsMetadata};
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::send_outbox::{SendOutbox, SendOutcome, SettledSend, MAX_ATTEMPTS};
use runtime::{FsBackend, KernelFsBackend};

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    /// The frame is written, then the caller is told the append timed out.
    CommitThenTimeOut,
    /// Nothing is written and the append times out.
    TimeOut,
    /// The append is refused outright.
    Refuse,
}

struct Faulty {
    inner: Arc<dyn FsBackend>,
    fault: Mutex<Fault>,
    unreadable: AtomicBool,
    /// Every read returns one more frame from another sender: a stream that
    /// keeps growing for as long as anyone reads it.
    growing: AtomicBool,
    tail_reads: AtomicUsize,
    /// Holds each mailbox's queued sends, so no test writes under the real
    /// config home.
    outboxes: tempfile::TempDir,
}

impl Faulty {
    fn set(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    fn outbox(&self, agent: &str) -> SendOutbox {
        SendOutbox::new(self.outboxes.path().join(agent))
    }
}

impl FsBackend for Faulty {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.inner.read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.inner.write(path, data)
    }
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let timed_out =
            || io::Error::new(io::ErrorKind::TimedOut, "append did not commit: deadline");
        match *self.fault.lock().unwrap() {
            Fault::None => self.inner.append(path, data),
            Fault::CommitThenTimeOut => {
                self.inner.append(path, data)?;
                Err(timed_out())
            }
            Fault::TimeOut => Err(timed_out()),
            Fault::Refuse => Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused")),
        }
    }
    fn delete(&self, path: &str) -> io::Result<()> {
        self.inner.delete(path)
    }
    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        self.inner.stat(path)
    }
    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        self.inner.readdir(path)
    }
    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.inner.rename(from, to)
    }
    fn canonicalize(&self, path: &str) -> io::Result<String> {
        self.inner.canonicalize(path)
    }
    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        self.inner.symlink_metadata(path)
    }
    fn create_append_log(&self, path: &str, retention: u64) -> io::Result<()> {
        self.inner.create_append_log(path, retention)
    }
    fn is_append_stream(&self, path: &str) -> io::Result<bool> {
        self.inner.is_append_stream(path)
    }
    fn link(&self, alias: &str, target: &str) -> io::Result<()> {
        self.inner.link(alias, target)
    }
    fn read_link(&self, alias: &str) -> io::Result<String> {
        self.inner.read_link(alias)
    }
    fn tail_read(
        &self,
        path: &str,
        cursor: u64,
        block_ms: u64,
    ) -> io::Result<(Vec<u8>, u64, bool)> {
        self.tail_reads.fetch_add(1, Ordering::SeqCst);
        if self.unreadable.load(Ordering::SeqCst) {
            return Err(io::Error::other("link down"));
        }
        if self.growing.load(Ordering::SeqCst) {
            let mut other = message("someone else's words", 1);
            other.from = "carol".into();
            other.send_id = Some(format!("carol-{cursor}"));
            let frame = other.to_bytes();
            let next = cursor + frame.len() as u64;
            return Ok((frame, next, false));
        }
        self.inner.tail_read(path, cursor, block_ms)
    }
}

/// `alice`'s mailbox over a faulty backend, and `bob`'s over the same store
/// without faults, with their conversation already a stream.
fn pair() -> (Mailbox, Mailbox, Arc<Faulty>) {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &kernel::core::agents::registry::AgentDescriptor {
            name: "alice".into(),
            owner_id: "test-owner".into(),
            zone_id: "root".into(),
            ..Default::default()
        },
        "/workspace",
    ));
    let faulty = Arc::new(Faulty {
        inner: Arc::clone(&fs),
        fault: Mutex::new(Fault::None),
        unreadable: AtomicBool::new(false),
        growing: AtomicBool::new(false),
        tail_reads: AtomicUsize::new(0),
        outboxes: tempfile::tempdir().unwrap(),
    });
    let root = "/workspace/mail".to_string();
    let alice = Mailbox::new(
        Arc::clone(&faulty) as Arc<dyn FsBackend>,
        "alice".into(),
        InboxConvention::new(root.clone()),
    )
    .with_outbox(faulty.outbox("alice"));
    let bob = Mailbox::new(fs, "bob".into(), InboxConvention::new(root))
        .with_outbox(faulty.outbox("bob"));
    let transcript = alice.transcript_path("bob");
    let parent = std::path::Path::new(&transcript).parent().unwrap();
    faulty.create_dir_all(parent.to_str().unwrap()).unwrap();
    kernel.create_stream(&transcript, 1 << 20).unwrap();
    assert!(faulty.is_append_stream(&transcript).unwrap());
    (alice, bob, faulty)
}

fn message(body: &str, timestamp: u64) -> MailboxEnvelope {
    MailboxEnvelope {
        send_id: None,
        delivery: runtime::turn_input::InputDelivery::Queue,
        from: "alice".into(),
        to: "bob".into(),
        body: body.into(),
        summary: None,
        timestamp,
        color: None,
        kind: "message".into(),
        request_id: None,
    }
}

fn bodies(bob: &Mailbox) -> Vec<String> {
    bob.read_conversation("alice")
        .unwrap()
        .into_iter()
        .map(|envelope| envelope.body)
        .collect()
}

/// Run `bob`'s receiver until it has handed over `last`, then stop it and
/// return every body it handed over, in order.
fn receive_through(bob: Mailbox, last: &str) -> Vec<String> {
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let abort = runtime::HookAbortSignal::new();
    let sink = Arc::clone(&delivered);
    let receiver = runtime::mailbox::spawn_inbox_record_poller(
        Arc::new(bob),
        100,
        "reconcile-test",
        abort.clone(),
        move |record| {
            sink.lock().unwrap().push(record.envelope().body.clone());
            true
        },
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while delivered.lock().unwrap().last().map(String::as_str) != Some(last) {
        assert!(
            Instant::now() < deadline,
            "the receiver never handed over {last:?}: {:?}",
            delivered.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    abort.abort();
    receiver.join().unwrap();
    let bodies = delivered.lock().unwrap().clone();
    bodies
}

#[test]
fn a_successful_send_does_not_read_the_conversation() {
    let (alice, bob, faulty) = pair();
    alice.send(message("hello", 0)).unwrap();
    assert_eq!(faulty.tail_reads.load(Ordering::SeqCst), 0);
    assert_eq!(bodies(&bob), ["hello"]);
}

#[test]
fn an_append_that_committed_before_its_error_is_delivered() {
    let (alice, bob, faulty) = pair();
    faulty.set(Fault::CommitThenTimeOut);
    alice.send(message("landed", 0)).unwrap();
    assert_eq!(bodies(&bob), ["landed"]);
}

#[test]
fn a_missing_message_is_unconfirmed_not_undelivered() {
    let (alice, bob, faulty) = pair();
    faulty.set(Fault::TimeOut);
    let error = alice.send(message("lost", 0)).unwrap_err();
    assert!(error.starts_with("delivery unconfirmed"), "{error}");
    assert!(error.contains("do not resend it yourself"), "{error}");
    assert_eq!(bodies(&bob), Vec::<String>::new());
}

#[test]
fn an_unreadable_conversation_leaves_the_outcome_unknown() {
    let (alice, _bob, faulty) = pair();
    faulty.set(Fault::TimeOut);
    faulty.unreadable.store(true, Ordering::SeqCst);
    let error = alice.send(message("unknown", 0)).unwrap_err();
    assert!(error.contains("delivery unknown"), "{error}");
    assert!(error.contains("link down"), "{error}");
}

#[test]
fn a_refusal_is_reported_without_checking() {
    let (alice, _bob, faulty) = pair();
    faulty.set(Fault::Refuse);
    let error = alice.send(message("refused", 0)).unwrap_err();
    assert!(error.contains("refused"), "{error}");
    assert!(!error.contains("delivery"), "{error}");
    assert_eq!(faulty.tail_reads.load(Ordering::SeqCst), 0);
}

/// Two sends of the same words in the same second are byte-identical apart
/// from their identity. Finding the first must not confirm the second, which
/// wrote nothing.
#[test]
fn an_identical_message_from_the_same_second_is_not_this_send() {
    let (alice, bob, faulty) = pair();
    alice.send(message("same words", 100)).unwrap();
    faulty.set(Fault::TimeOut);
    let error = alice.send(message("same words", 100)).unwrap_err();
    assert!(error.contains("delivery unconfirmed"), "{error}");
    assert_eq!(bodies(&bob), ["same words"]);
}

/// A retransmission of a send that already committed is that same send, so
/// finding it is delivery even though the retransmission itself wrote nothing.
#[test]
fn a_retransmission_of_a_committed_send_is_delivered() {
    let (alice, bob, faulty) = pair();
    let mut original = message("once only", 100);
    original.send_id = Some("logical-send-1".into());
    alice.send(original.clone()).unwrap();
    faulty.set(Fault::TimeOut);
    alice.send(original).unwrap();
    assert_eq!(bodies(&bob), ["once only"]);
}

/// The check is bounded: a stream that keeps growing ends it as unconfirmed.
#[test]
fn a_check_against_a_growing_stream_ends_unconfirmed() {
    let (alice, _bob, faulty) = pair();
    faulty.set(Fault::TimeOut);
    faulty.growing.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let error = alice.send(message("lost in the noise", 0)).unwrap_err();
    assert!(error.contains("delivery unconfirmed"), "{error}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the check must stop on its own: {:?}",
        started.elapsed()
    );
}

/// A retry whose append succeeds after the original also landed puts one send
/// in the conversation twice. The receiver hands it over once.
#[test]
fn a_retransmission_that_lands_is_delivered_once() {
    let (alice, bob, _faulty) = pair();
    let mut original = message("said once", 100);
    original.send_id = Some("logical-send-2".into());
    alice.send(original.clone()).unwrap();
    alice.send(original).unwrap();
    alice.send(message("after", 101)).unwrap();
    assert_eq!(bodies(&bob), ["said once", "said once", "after"]);
    assert_eq!(receive_through(bob, "after"), ["said once", "after"]);
}

/// An append that keeps failing past the check leaves the send queued, and
/// once the link is back a retry delivers it exactly once and says so, once.
#[test]
fn an_unconfirmed_send_is_queued_and_retried_once_the_link_is_back() {
    let (alice, bob, faulty) = pair();
    faulty.set(Fault::TimeOut);
    let error = alice.send(message("queued", 0)).unwrap_err();
    let queued = faulty.outbox("alice").list().unwrap();
    assert_eq!(queued.len(), 1, "{queued:?}");
    let send_id = queued[0].send_id().to_string();
    assert!(error.starts_with("delivery unconfirmed"), "{error}");
    assert!(
        error.contains(&format!("queued as send {send_id}")),
        "{error}"
    );
    assert_eq!(bodies(&bob), Vec::<String>::new());

    faulty.set(Fault::None);
    alice.retry_outbox(None).unwrap();
    assert_eq!(bodies(&bob), ["queued"]);
    assert!(
        faulty.outbox("alice").list().unwrap().is_empty(),
        "a delivered send leaves the queue"
    );
    let settled = alice.take_settled_outcomes();
    assert_eq!(
        settled,
        [SettledSend {
            to: "bob".into(),
            send_id: send_id.clone(),
            outcome: SendOutcome::Delivered,
            attempts: 2,
        }]
    );
    assert_eq!(
        settled[0].to_string(),
        format!("Earlier queued send {send_id} to bob: delivered after retry (2 attempts)")
    );
    alice.retry_outbox(None).unwrap();
    assert_eq!(bodies(&bob), ["queued"], "a settled send is not retried");
    assert!(
        alice.take_settled_outcomes().is_empty(),
        "an outcome is reported once"
    );
}

/// A send that never gets through is abandoned at the attempt limit, and the
/// agent is told to resend it if it still matters.
#[test]
fn a_send_that_never_gets_through_is_abandoned_at_the_attempt_limit() {
    let (alice, _bob, faulty) = pair();
    faulty.set(Fault::TimeOut);
    // Every check fails at once instead of waiting out a late commit.
    faulty.unreadable.store(true, Ordering::SeqCst);
    let error = alice.send(message("never", 0)).unwrap_err();
    assert!(error.starts_with("delivery unknown"), "{error}");
    assert!(error.contains("queued as send"), "{error}");
    for attempts in 2..MAX_ATTEMPTS {
        alice.retry_outbox(None).unwrap();
        assert_eq!(faulty.outbox("alice").list().unwrap()[0].attempts, attempts);
        assert!(
            alice.take_settled_outcomes().is_empty(),
            "a send under its limits has not ended"
        );
    }
    alice.retry_outbox(None).unwrap();
    assert!(
        faulty.outbox("alice").list().unwrap().is_empty(),
        "an abandoned send leaves the queue"
    );
    let settled = alice.take_settled_outcomes();
    assert_eq!(settled.len(), 1, "{settled:?}");
    assert_eq!(settled[0].outcome, SendOutcome::Abandoned);
    assert_eq!(settled[0].attempts, MAX_ATTEMPTS);
    assert!(
        settled[0].to_string().ends_with(&format!(
            "abandoned after {MAX_ATTEMPTS} attempts — resend if still needed"
        )),
        "{}",
        settled[0]
    );
}

/// Envelopes without a send id are independent sends, however alike: the
/// receiver hands over every one.
#[test]
fn envelopes_without_a_send_id_are_never_taken_for_repeats() {
    let (alice, bob, faulty) = pair();
    let path = alice.transcript_path("bob");
    let legacy = br#"{"from":"alice","to":"bob","body":"no identity"}"#;
    faulty.append(&path, legacy).unwrap();
    faulty.append(&path, legacy).unwrap();
    alice.send(message("marker", 0)).unwrap();
    assert_eq!(
        receive_through(bob, "marker"),
        ["no identity", "no identity", "marker"]
    );
}
