//! A conversation's read position is written only by its current holder.
//!
//! Over a backend whose locks are expiring leases (a node), a holder frozen
//! past its lease can resume after another instance has taken the
//! conversation, and finish a write it started before. These tests drive real
//! receivers over [`file_cas::FileCasBackend`], which has the node's lease and
//! conditional-write semantics on host files, and check that such a holder is
//! fenced out rather than moving the newer holder's position back; that an
//! unknown write outcome is settled by reading it back; and that a backend
//! without a conditional write, or the disk's own lock, keeps the plain store
//! and says which guarantee it gives.

#[path = "support/file_cas.rs"]
mod file_cas;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use file_cas::{expire_leases_under, wait_while_held, FileCasBackend};
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::cursor_fence::CursorGuarantee;
use runtime::fs_backend::{FsBackend, StdFsBackend};
use runtime::mailbox::{
    cursor_guarantee, spawn_inbox_poller, InboxConvention, Mailbox, ReaderRegister,
};
use runtime::HookAbortSignal;

const PEER: &str = "fence-peer";

type Seen = Arc<Mutex<Vec<(&'static str, String)>>>;

struct Receiver {
    stop: HookAbortSignal,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Hold files this receiver may be parked on, removed before joining so a
    /// failed assertion ends the test instead of hanging it.
    holds: Vec<PathBuf>,
}

impl Receiver {
    fn releasing(mut self, hold: PathBuf) -> Self {
        self.holds.push(hold);
        self
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        for hold in &self.holds {
            let _ = std::fs::remove_file(hold);
        }
        self.stop.abort();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn tmp_root(label: &str) -> PathBuf {
    let root = runtime::test_support::temp_dir_path(&format!("cursor-fence-{label}"));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn convention(root: &Path) -> InboxConvention {
    InboxConvention::new(root.to_string_lossy().into_owned())
}

fn mailbox(backend: Arc<dyn FsBackend>, root: &Path, agent: &str) -> Arc<Mailbox> {
    Arc::new(Mailbox::new(backend, agent.to_string(), convention(root)))
}

fn send(root: &Path, to: &str, body: &str) {
    mailbox(Arc::new(StdFsBackend), root, PEER)
        .send(MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: PEER.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("the peer writes");
}

fn start(
    backend: Arc<dyn FsBackend>,
    root: &Path,
    agent: &str,
    who: &'static str,
    seen: &Seen,
) -> Receiver {
    start_holding(backend, root, agent, who, seen, None)
}

/// [`start`], with each delivery held while `hold` exists.
fn start_holding(
    backend: Arc<dyn FsBackend>,
    root: &Path,
    agent: &str,
    who: &'static str,
    seen: &Seen,
    hold: Option<PathBuf>,
) -> Receiver {
    let stop = HookAbortSignal::new();
    let seen = Arc::clone(seen);
    let holds = hold.iter().cloned().collect();
    let handle = spawn_inbox_poller(
        mailbox(backend, root, agent),
        200,
        who,
        stop.clone(),
        move |m| {
            seen.lock().unwrap().push((who, m.body.clone()));
            if let Some(hold) = &hold {
                wait_while_held(hold);
            }
            true
        },
    );
    Receiver {
        stop,
        handle: Some(handle),
        holds,
    }
}

fn bodies(seen: &Seen, who: &str) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(by, _)| *by == who)
        .map(|(_, body)| body.clone())
        .collect()
}

fn register_raw(root: &Path, agent: &str) -> Option<Vec<u8>> {
    std::fs::read(convention(root).reader_path(agent, PEER, agent)).ok()
}

fn register(root: &Path, agent: &str) -> Option<ReaderRegister> {
    serde_json::from_slice(&register_raw(root, agent)?).ok()
}

fn transcript_end(root: &Path, agent: &str) -> u64 {
    std::fs::metadata(convention(root).transcript_path(agent, PEER)).map_or(0, |m| m.len())
}

/// The register shows `epoch` and has taken everything sent so far.
fn caught_up(root: &Path, agent: &str, epoch: u64) -> bool {
    register(root, agent)
        .is_some_and(|r| r.epoch == epoch && r.read_offset == transcript_end(root, agent))
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_holder_resumed_after_losing_its_lease_cannot_move_the_cursor_back() {
    const AGENT: &str = "fence-resumed-holder";
    let root = tmp_root("resumed");
    let gate = root.join("gate");
    std::fs::create_dir_all(&gate).unwrap();
    let b_hold = root.join("b-hold");
    let seen: Seen = Arc::default();

    send(&root, AGENT, "m1");
    let a = start(
        Arc::new(FileCasBackend::new().with_gate(&gate)),
        &root,
        AGENT,
        "A",
        &seen,
    )
    .releasing(gate.join("hold"));
    wait_until("A to take and record m1", || caught_up(&root, AGENT, 1));
    assert_eq!(cursor_guarantee(AGENT), Some(CursorGuarantee::FencedCas));
    let after_m1 = transcript_end(&root, AGENT);

    // Park A's next commit after its lease check and before its write, then
    // end every lease: A is a holder frozen past its TTL at the worst moment.
    std::fs::write(gate.join("hold"), b"").unwrap();
    send(&root, AGENT, "m2");
    wait_until("A to be parked in its commit", || {
        gate.join("entered").exists()
    });

    // B takes over and writes its fence, then is held mid-delivery: from here
    // on only the fence stands between A's stale write and the register.
    std::fs::write(&b_hold, b"").unwrap();
    expire_leases_under(&root);
    let b = start_holding(
        Arc::new(FileCasBackend::new()),
        &root,
        AGENT,
        "B",
        &seen,
        Some(b_hold.clone()),
    );
    wait_until("B to fence and start delivering m2", || {
        b_hold.with_extension("entered").exists()
    });
    let fence = register(&root, AGENT).unwrap();
    assert_eq!(
        (fence.epoch, fence.read_offset),
        (2, after_m1),
        "B's fence takes the next epoch at the last recorded position"
    );

    // A resumes and finishes the write it started against its old version.
    std::fs::remove_file(gate.join("hold")).unwrap();
    wait_until("A's parked write to be decided", || {
        gate.join("left").exists()
    });
    assert_eq!(
        register(&root, AGENT).unwrap(),
        fence,
        "the resumed holder must be refused by the fence, not record m2 over it"
    );

    // A was refused, so it keeps nothing: the next message goes to B alone.
    send(&root, AGENT, "m3");
    std::fs::remove_file(&b_hold).unwrap();
    wait_until("B to record m2 and m3", || caught_up(&root, AGENT, 2));
    assert_eq!(
        bodies(&seen, "A"),
        ["m1", "m2"],
        "a fenced-out holder must not go on delivering"
    );
    assert_eq!(
        bodies(&seen, "B"),
        ["m2", "m3"],
        "the message A never recorded is delivered again, not lost"
    );

    drop((a, b));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unknown_commit_that_landed_is_kept_without_redelivery() {
    const AGENT: &str = "fence-unknown-landed";
    let root = tmp_root("unknown-landed");
    let seen: Seen = Arc::default();
    let backend = Arc::new(FileCasBackend::new());

    send(&root, AGENT, "m1");
    let a = start(
        Arc::clone(&backend) as Arc<dyn FsBackend>,
        &root,
        AGENT,
        "A",
        &seen,
    );
    wait_until("m1 recorded", || caught_up(&root, AGENT, 1));
    let holder = register(&root, AGENT).unwrap().holder;

    backend.inject_unknown(true);
    send(&root, AGENT, "m2");
    wait_until("m2 recorded", || caught_up(&root, AGENT, 1));
    std::thread::sleep(Duration::from_secs(2));
    let settled = register(&root, AGENT).unwrap();
    assert_eq!(
        (settled.holder, settled.epoch),
        (holder, 1),
        "a commit read back as this holder's own keeps the claim, with no re-fence"
    );
    assert_eq!(bodies(&seen, "A"), ["m1", "m2"]);

    drop(a);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unknown_commit_that_did_not_land_is_redelivered_under_a_new_fence() {
    const AGENT: &str = "fence-unknown-lost";
    let root = tmp_root("unknown-lost");
    let seen: Seen = Arc::default();
    let backend = Arc::new(FileCasBackend::new());

    send(&root, AGENT, "m1");
    let a = start(
        Arc::clone(&backend) as Arc<dyn FsBackend>,
        &root,
        AGENT,
        "A",
        &seen,
    );
    wait_until("m1 recorded", || caught_up(&root, AGENT, 1));

    backend.inject_unknown(false);
    send(&root, AGENT, "m2");
    wait_until("m2 recorded after a fresh claim", || {
        caught_up(&root, AGENT, 2)
    });
    assert_eq!(
        bodies(&seen, "A"),
        ["m1", "m2", "m2"],
        "a commit that did not land leaves the message unread, so it comes again"
    );

    drop(a);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_unknown_fence_is_settled_by_reading_it_back() {
    for (committed, label) in [(true, "fence-claim-landed"), (false, "fence-claim-lost")] {
        let root = tmp_root(label);
        let seen: Seen = Arc::default();
        let backend = Arc::new(FileCasBackend::new());
        backend.inject_unknown(committed);

        send(&root, label, "m1");
        let a = start(
            Arc::clone(&backend) as Arc<dyn FsBackend>,
            &root,
            label,
            "A",
            &seen,
        );
        wait_until("m1 recorded", || caught_up(&root, label, 1));
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(
            bodies(&seen, "A"),
            ["m1"],
            "[{label}] an unknown fence either turns out to be ours or is claimed again; \
             either way m1 is delivered once"
        );

        drop(a);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[test]
fn without_a_conditional_write_the_cursor_is_declared_at_least_once() {
    const AGENT: &str = "fence-unsupported";
    let root = tmp_root("unsupported");
    let seen: Seen = Arc::default();

    send(&root, AGENT, "m1");
    let a = start(
        Arc::new(FileCasBackend::without_conditional_writes()),
        &root,
        AGENT,
        "A",
        &seen,
    );
    wait_until("m1 recorded", || caught_up(&root, AGENT, 0));
    assert_eq!(cursor_guarantee(AGENT), Some(CursorGuarantee::AtLeastOnce));
    assert!(
        !String::from_utf8_lossy(&register_raw(&root, AGENT).unwrap()).contains("epoch"),
        "the plain store writes the register as before"
    );

    drop(a);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_disk_lock_keeps_its_plain_store() {
    const AGENT: &str = "fence-disk";
    let root = tmp_root("disk");
    let seen: Seen = Arc::default();

    send(&root, AGENT, "m1");
    let a = start(Arc::new(StdFsBackend), &root, AGENT, "A", &seen);
    wait_until("m1 recorded", || caught_up(&root, AGENT, 0));
    assert_eq!(cursor_guarantee(AGENT), Some(CursorGuarantee::Fenced));
    assert!(
        !String::from_utf8_lossy(&register_raw(&root, AGENT).unwrap()).contains("epoch"),
        "a disk register is stored exactly as before"
    );

    drop(a);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_exhausted_claim_epoch_fails_closed() {
    const AGENT: &str = "fence-epoch-exhausted";
    let root = tmp_root("epoch-exhausted");
    let backend = Arc::new(FileCasBackend::new());

    send(&root, AGENT, "m1");
    let exhausted = ReaderRegister {
        epoch: u64::MAX,
        ..ReaderRegister::default()
    };
    backend
        .write_if_version(
            &convention(&root).reader_path(AGENT, PEER, AGENT),
            None,
            &serde_json::to_vec(&exhausted).unwrap(),
        )
        .expect("seed the register");
    let before = register_raw(&root, AGENT).unwrap();

    let error = mailbox(backend, &root, AGENT)
        .read_unread(PEER)
        .err()
        .expect("a claim with no next epoch must fail");
    assert!(error.contains("epoch exhausted"), "{error}");
    assert_eq!(
        register_raw(&root, AGENT).unwrap(),
        before,
        "nothing is written when the claim cannot fence"
    );

    let _ = std::fs::remove_dir_all(&root);
}
