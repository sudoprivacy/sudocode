//! A framed append that returns an error may still have committed. `send`
//! looks for the exact envelope in the conversation before it reports.
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kernel::kernel::Kernel;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::{FsDirEntry, FsMetadata};
use runtime::mailbox::{InboxConvention, Mailbox};
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
    tail_reads: AtomicUsize,
}

impl Faulty {
    fn set(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
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
        tail_reads: AtomicUsize::new(0),
    });
    let root = "/workspace/mail".to_string();
    let alice = Mailbox::new(
        Arc::clone(&faulty) as Arc<dyn FsBackend>,
        "alice".into(),
        InboxConvention::new(root.clone()),
    );
    let bob = Mailbox::new(fs, "bob".into(), InboxConvention::new(root));
    let transcript = alice.transcript_path("bob");
    let parent = std::path::Path::new(&transcript).parent().unwrap();
    faulty.create_dir_all(parent.to_str().unwrap()).unwrap();
    kernel.create_stream(&transcript, 1 << 20).unwrap();
    assert!(faulty.is_append_stream(&transcript).unwrap());
    (alice, bob, faulty)
}

fn message(body: &str, timestamp: u64) -> MailboxEnvelope {
    MailboxEnvelope {
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
    assert!(error.contains("delivery unconfirmed"), "{error}");
    assert!(error.contains("before resending"), "{error}");
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

#[test]
fn an_earlier_identical_message_is_not_this_send() {
    let (alice, bob, faulty) = pair();
    alice.send(message("same words", 100)).unwrap();
    faulty.set(Fault::TimeOut);
    let error = alice.send(message("same words", 101)).unwrap_err();
    assert!(error.contains("delivery unconfirmed"), "{error}");
    assert_eq!(bodies(&bob), ["same words"]);
}
