//! Delivery receipts against the actual kernel and host filesystem adapters.
use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::Kernel;
use runtime::mailbox::Mailbox;
use runtime::zone_context::ENV_NEXUS_RESOURCE_SCOPE;
use runtime::FsBackend;
use runtime::KernelFsBackend;
use std::sync::{Arc, Barrier};
use std::time::Duration;

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

fn pair() -> (Mailbox, Mailbox) {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let sender = Mailbox::daemon_absolute(
        Arc::new(KernelFsBackend::for_agent_descriptor(
            Arc::clone(&kernel),
            &planted("sender"),
            "/",
        )),
        "sender".into(),
    );
    let receiver = Mailbox::daemon_absolute(
        Arc::new(KernelFsBackend::for_agent_descriptor(
            kernel,
            &planted("receiver"),
            "/",
        )),
        "receiver".into(),
    );
    sender
        .send(runtime::agent_mailbox::MailboxEnvelope {
            send_id: None,
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: "sender".into(),
            to: "receiver".into(),
            body: "durable delivery".into(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: "message".into(),
            request_id: None,
        })
        .unwrap();
    (sender, receiver)
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
