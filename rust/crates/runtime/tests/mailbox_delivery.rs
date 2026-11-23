//! Delivery receipts against the actual kernel and host filesystem adapters.
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::Kernel;
use runtime::mailbox::{CommitOutcome, CommitRefusalReason, Mailbox};
use runtime::zone_context::ENV_NEXUS_RESOURCE_SCOPE;
use runtime::FsBackend;
use runtime::KernelFsBackend;
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

/// A planted descriptor carrying a resource scope that admits the daemon
/// mail world — the `/agents` chat-list indexes and the flat
/// `/conversations` transcript store they point into — the authority a
/// co-hosted agent actually runs with, so the fail-closed zone contract
/// admits the kernel round-trips this suite asserts.
fn planted(name: &str) -> AgentDescriptor {
    let mut descriptor = AgentDescriptor {
        pid: format!("pid-{name}"),
        name: name.to_string(),
        owner_id: "owner".to_string(),
        zone_id: "root".to_string(),
        ..AgentDescriptor::default()
    };
    descriptor.labels.insert(
        ENV_NEXUS_RESOURCE_SCOPE.to_string(),
        serde_json::json!({
            "schema_version": 1,
            "zone_id": "root",
            "rules": [
                {"capability": "zone.data.read", "resource_prefixes": ["/agents", "/conversations"]},
                {"capability": "zone.data.write", "resource_prefixes": ["/agents", "/conversations"]}
            ]
        })
        .to_string(),
    );
    descriptor
}

fn mounted_kernel() -> Arc<Kernel> {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    kernel
}

fn mailbox(kernel: &Arc<Kernel>, name: &str) -> Mailbox {
    Mailbox::daemon_absolute(
        Arc::new(KernelFsBackend::for_agent_descriptor(
            Arc::clone(kernel),
            &planted(name),
            "/",
        )),
        name.into(),
    )
}

fn note(body: &str) -> runtime::agent_mailbox::MailboxEnvelope {
    runtime::agent_mailbox::MailboxEnvelope {
        send_id: None,
        delivery: runtime::turn_input::InputDelivery::Steer,
        from: "sender".into(),
        to: "receiver".into(),
        body: body.into(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: "message".into(),
        request_id: None,
    }
}

fn pair() -> (Mailbox, Mailbox) {
    let kernel = mounted_kernel();
    let sender = mailbox(&kernel, "sender");
    let receiver = mailbox(&kernel, "receiver");
    sender.send(note("durable delivery")).unwrap();
    (sender, receiver)
}

/// A receiver running over a mailbox, recording every body offered to its
/// consumer, in order. `accept` answers for the latest offer, given them all.
struct Receiver {
    offered: Arc<Mutex<Vec<String>>>,
    abort: runtime::HookAbortSignal,
    thread: std::thread::JoinHandle<()>,
}

impl Receiver {
    fn start(mailbox: Mailbox, accept: impl Fn(&[String]) -> bool + Send + Sync + 'static) -> Self {
        let offered = Arc::new(Mutex::new(Vec::new()));
        let abort = runtime::HookAbortSignal::new();
        let log = Arc::clone(&offered);
        let thread = runtime::mailbox::spawn_inbox_record_poller(
            Arc::new(mailbox),
            100,
            "delivery-test",
            abort.clone(),
            move |record| {
                let mut log = log.lock().unwrap();
                log.push(record.envelope().body.clone());
                accept(&log)
            },
        );
        Self {
            offered,
            abort,
            thread,
        }
    }

    fn wait_for(&self, done: impl Fn(&[String]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(&self.offered.lock().unwrap()) {
            assert!(
                Instant::now() < deadline,
                "the receiver was offered {:?}",
                self.offered.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(self) -> Vec<String> {
        self.abort.abort();
        self.thread.join().unwrap();
        let offered = self.offered.lock().unwrap().clone();
        offered
    }
}

#[test]
fn dropped_receipt_replays_and_acknowledged_receipt_does_not() {
    let (_sender, receiver) = pair();
    let pending = receiver.read_unread("sender").unwrap().unwrap();
    assert_eq!(pending.messages()[0].body, "durable delivery");
    assert_eq!(receiver.read_position("sender").unwrap(), Some(0));
    assert!(receiver.read_unread("sender").unwrap().is_none());
    drop(pending);
    let replay = receiver.read_unread("sender").unwrap().unwrap();
    assert_eq!(replay.messages().len(), 1);
    replay.acknowledge().unwrap();
    assert_eq!(
        receiver.read_unread("sender").unwrap().unwrap().messages(),
        []
    );
}

#[test]
fn simultaneous_kernel_readers_cannot_both_own_the_cursor() {
    let (_sender, receiver) = pair();
    let receiver = Arc::new(receiver);
    let barrier = Arc::new(Barrier::new(8));
    let release = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let receiver = Arc::clone(&receiver);
            let barrier = Arc::clone(&barrier);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                barrier.wait();
                let pending = receiver.read_unread("sender").unwrap();
                let acquired = pending.is_some();
                release.wait();
                drop(pending);
                acquired
            })
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|acquired| *acquired)
            .count(),
        1
    );
}

#[test]
fn blocked_kernel_consumer_renews_ownership_past_the_lease() {
    let (_sender, receiver) = pair();
    let pending = receiver.read_unread("sender").unwrap().unwrap();
    std::thread::sleep(Duration::from_secs(35));
    assert!(
        receiver.read_unread("sender").unwrap().is_none(),
        "blocked consumer must renew independently"
    );
    assert_eq!(receiver.read_position("sender").unwrap(), Some(0));
    pending.acknowledge().unwrap();
    assert!(receiver.read_position("sender").unwrap().unwrap() > 0);
}

#[test]
fn legacy_record_identity_uses_committed_kernel_positions() {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let backend = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &planted("sender"),
        "/",
    ));
    let sender = Mailbox::daemon_absolute(backend.clone(), "sender".into());
    let receiver = Mailbox::daemon_absolute(backend.clone(), "receiver".into());
    let path = sender.transcript_path("receiver");
    backend
        .create_dir_all(
            std::path::Path::new(&path)
                .parent()
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
    kernel.create_stream(&path, 1 << 20).unwrap();
    sender.ensure_conversation("receiver").unwrap();
    assert!(
        backend.is_append_stream(&path).unwrap(),
        "exercise real DT_STREAM"
    );
    let raw = br#"{"from":"sender","to":"receiver","body":"identical legacy"}"#;
    backend.append(&path, raw).unwrap();
    let (first, cursor) = receiver.poll_conversation_records("sender", 0, 0).unwrap();
    assert_eq!(first.len(), 1);
    backend.append(&path, raw).unwrap();
    let (both, _) = receiver.poll_conversation_records("sender", 0, 0).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(both[0].envelope(), both[1].envelope());
    assert_eq!(first[0].delivery_id(), both[0].delivery_id());
    assert_ne!(both[0].delivery_id(), both[1].delivery_id());
    let (resumed, _) = receiver
        .poll_conversation_records("sender", cursor, 0)
        .unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].delivery_id(), both[1].delivery_id());
}

/// One stream append is one frame, however many records it holds: it is
/// offered whole, both records keep distinct identities, and one commit to the
/// frame's end acknowledges both.
#[test]
fn a_multi_record_stream_frame_is_offered_and_committed_whole() {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let backend = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &planted("sender"),
        "/",
    ));
    let sender = Mailbox::daemon_absolute(backend.clone(), "sender".into());
    let receiver = Mailbox::daemon_absolute(backend.clone(), "receiver".into());
    let path = sender.transcript_path("receiver");
    backend
        .create_dir_all(
            std::path::Path::new(&path)
                .parent()
                .unwrap()
                .to_str()
                .unwrap(),
        )
        .unwrap();
    kernel.create_stream(&path, 1 << 20).unwrap();
    sender.ensure_conversation("receiver").unwrap();
    assert!(
        backend.is_append_stream(&path).unwrap(),
        "exercise real DT_STREAM"
    );
    let two = b"{\"from\":\"sender\",\"to\":\"receiver\",\"body\":\"one\"}\n\
{\"from\":\"sender\",\"to\":\"receiver\",\"body\":\"two\"}\n";
    backend.append(&path, two).unwrap();
    let single = br#"{"from":"sender","to":"receiver","body":"three"}"#;
    backend.append(&path, single).unwrap();

    let unread = receiver.unread_frames("sender").unwrap();
    assert_eq!(unread.start(), 0);
    let frames = unread.frames();
    assert_eq!(frames.len(), 2);
    let bodies: Vec<_> = frames[0]
        .records()
        .iter()
        .map(|record| record.envelope().body.clone())
        .collect();
    assert_eq!(bodies, ["one", "two"]);
    assert_ne!(
        frames[0].records()[0].delivery_id(),
        frames[0].records()[1].delivery_id()
    );
    assert_eq!(frames[0].end(), frames[1].start());
    assert_eq!(frames[1].records().len(), 1);

    assert_eq!(
        receiver
            .commit_through("sender", unread.start(), frames[0].end())
            .unwrap(),
        CommitOutcome::Committed
    );
    let rest = receiver.unread_frames("sender").unwrap();
    assert_eq!(rest.start(), frames[0].end());
    assert_eq!(rest.frames().len(), 1);
    assert_eq!(rest.frames()[0].records()[0].envelope().body, "three");
    assert_eq!(
        receiver
            .commit_through("sender", unread.start(), frames[1].end())
            .unwrap(),
        CommitOutcome::Refused(CommitRefusalReason::PositionMoved)
    );
    assert_eq!(
        receiver
            .commit_through("sender", rest.start(), frames[1].end())
            .unwrap(),
        CommitOutcome::Committed
    );
    assert!(receiver
        .unread_frames("sender")
        .unwrap()
        .frames()
        .is_empty());
    assert_eq!(
        receiver
            .commit_through("sender", rest.start(), frames[1].end())
            .unwrap(),
        CommitOutcome::Already
    );
}

/// On the kernel backend too, a held reader refuses a commit.
#[test]
fn a_kernel_commit_is_refused_while_the_reader_is_held() {
    let (_sender, receiver) = pair();
    let end = receiver.unread_frames("sender").unwrap().frames()[0].end();
    let held = receiver.read_unread("sender").unwrap().unwrap();
    assert_eq!(
        receiver.commit_through("sender", 0, end).unwrap(),
        CommitOutcome::Refused(CommitRefusalReason::LeaseLost)
    );
    drop(held);
    assert_eq!(
        receiver.commit_through("sender", 0, end).unwrap(),
        CommitOutcome::Committed
    );
    assert_eq!(receiver.read_position("sender").unwrap(), Some(end));
}

/// One send appended at two positions, as a retry of an unconfirmed append
/// can leave it, is handed to the consumer once.
#[test]
fn one_send_at_two_positions_is_delivered_once() {
    let (sender, receiver) = pair();
    let receiver = Receiver::start(receiver, |_| true);
    receiver.wait_for(|offered| offered == ["durable delivery"]);
    let mut retried = note("retried");
    retried.send_id = Some("retried-send".into());
    sender.send(retried.clone()).unwrap();
    receiver.wait_for(|offered| offered.len() == 2);
    sender.send(note("between")).unwrap();
    sender.send(retried).unwrap();
    sender.send(note("last")).unwrap();
    receiver.wait_for(|offered| offered.last().is_some_and(|body| body == "last"));
    assert_eq!(
        receiver.stop(),
        ["durable delivery", "retried", "between", "last"]
    );
}

/// A batch the consumer refuses part of is read again and offered whole, the
/// sends it accepted the first time included: nothing counts as delivered
/// until its batch is committed.
#[test]
fn a_refused_batch_is_redelivered_whole() {
    let (sender, receiver) = pair();
    sender.send(note("accepted")).unwrap();
    sender.send(note("refused once")).unwrap();
    let receiver = Receiver::start(receiver, |offered| {
        offered.last().map(String::as_str) != Some("refused once")
            || offered
                .iter()
                .filter(|body| *body == "refused once")
                .count()
                > 1
    });
    receiver.wait_for(|offered| offered.len() == 6);
    assert_eq!(
        receiver.stop(),
        [
            "durable delivery",
            "accepted",
            "refused once",
            "durable delivery",
            "accepted",
            "refused once"
        ]
    );
}

/// The sends a receiver delivered and committed are remembered across its
/// restart, so a retry of one that lands afterwards is not offered again.
#[test]
fn delivered_sends_are_remembered_across_a_receiver_restart() {
    let kernel = mounted_kernel();
    let sender = mailbox(&kernel, "sender");
    let mut retried = note("before the restart");
    retried.send_id = Some("restart-send".into());
    sender.send(retried.clone()).unwrap();
    let first = Receiver::start(mailbox(&kernel, "receiver"), |_| true);
    first.wait_for(|offered| offered == ["before the restart"]);
    assert_eq!(first.stop(), ["before the restart"]);

    sender.send(retried).unwrap();
    sender.send(note("after the restart")).unwrap();
    let second = Receiver::start(mailbox(&kernel, "receiver"), |_| true);
    second.wait_for(|offered| {
        offered
            .last()
            .is_some_and(|body| body == "after the restart")
    });
    assert_eq!(second.stop(), ["after the restart"]);
}
