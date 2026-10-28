//! The S6 fence across real processes.
//!
//! The in-process tests in `cursor_fence.rs` park a holder's commit on a gate.
//! Here the holder is a separate process that is actually frozen: the parent
//! re-runs this test binary as two receivers for the same agent over
//! [`file_cas::FileCasBackend`], parks the first one inside its commit,
//! freezes it with SIGSTOP, ends its leases, lets the second write its fence
//! and hold its delivery, and only then resumes the first. The resumed process
//! must be refused by the fence alone: the register keeps the second
//! receiver's fence, the first delivers nothing more, and the message it never
//! recorded is delivered again rather than lost.
//!
//! Freezing a process is Unix-only, so the scenario is ignored elsewhere; the
//! file still compiles on every platform.

#[path = "support/file_cas.rs"]
mod file_cas;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use file_cas::{expire_leases_under, wait_while_held, FileCasBackend};
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::StdFsBackend;
use runtime::mailbox::{spawn_inbox_poller, InboxConvention, Mailbox, ReaderRegister};
use runtime::HookAbortSignal;

const AGENT: &str = "fence-processes";
const PEER: &str = "fence-processes-peer";
const ROOT_ENV: &str = "CURSOR_FENCE_CHILD_ROOT";
const LABEL_ENV: &str = "CURSOR_FENCE_CHILD_LABEL";
const EVENTS_ENV: &str = "CURSOR_FENCE_CHILD_EVENTS";
const GATE_ENV: &str = "CURSOR_FENCE_CHILD_GATE";
const HOLD_ENV: &str = "CURSOR_FENCE_CHILD_HOLD";

/// One receiver, when this binary is re-run as a child. A no-op otherwise.
///
/// Records each delivery as `<label> <body>` in the events file, holds it
/// while the hold file (if any) exists, then accepts it. Runs until the parent
/// kills it.
#[test]
fn child_receiver() {
    let Ok(root) = std::env::var(ROOT_ENV) else {
        return;
    };
    let label = std::env::var(LABEL_ENV).expect("child label");
    let events = PathBuf::from(std::env::var(EVENTS_ENV).expect("child events file"));
    let hold = std::env::var(HOLD_ENV).ok().map(PathBuf::from);
    let mut backend = FileCasBackend::new();
    if let Ok(gate) = std::env::var(GATE_ENV) {
        backend = backend.with_gate(gate);
    }
    let mailbox = Arc::new(Mailbox::new(
        Arc::new(backend),
        AGENT.to_string(),
        InboxConvention::new(root),
    ));
    let _receiver = spawn_inbox_poller(mailbox, 200, "child", HookAbortSignal::new(), move |m| {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&events)
            .expect("open events");
        writeln!(file, "{label} {}", m.body).expect("record delivery");
        if let Some(hold) = &hold {
            wait_while_held(hold);
        }
        true
    });
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A child receiver, killed when dropped.
struct ChildReceiver(Child);

impl ChildReceiver {
    fn spawn(
        root: &Path,
        label: &str,
        events: &Path,
        gate: Option<&Path>,
        hold: Option<&Path>,
    ) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("test binary"));
        command
            .args([
                "child_receiver",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT_ENV, root)
            .env(LABEL_ENV, label)
            .env(EVENTS_ENV, events)
            .stdout(Stdio::null());
        if let Some(gate) = gate {
            command.env(GATE_ENV, gate);
        }
        if let Some(hold) = hold {
            command.env(HOLD_ENV, hold);
        }
        Self(command.spawn().expect("spawn child receiver"))
    }

    fn signal(&self, signal: &str) {
        let status = Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(self.0.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{signal} failed");
    }
}

impl Drop for ChildReceiver {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn convention(root: &Path) -> InboxConvention {
    InboxConvention::new(root.to_string_lossy().into_owned())
}

fn send(root: &Path, body: &str) {
    Mailbox::new(Arc::new(StdFsBackend), PEER.to_string(), convention(root))
        .send(MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: PEER.to_string(),
            to: AGENT.to_string(),
            body: body.to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("the peer writes");
}

fn register(root: &Path) -> Option<ReaderRegister> {
    serde_json::from_slice(&std::fs::read(convention(root).reader_path(AGENT, PEER, AGENT)).ok()?)
        .ok()
}

fn transcript_end(root: &Path) -> u64 {
    std::fs::metadata(convention(root).transcript_path(AGENT, PEER)).map_or(0, |m| m.len())
}

fn caught_up(root: &Path, epoch: u64) -> bool {
    register(root).is_some_and(|r| r.epoch == epoch && r.read_offset == transcript_end(root))
}

fn events(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_mins(1);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[cfg_attr(not(unix), ignore = "freezing a process takes SIGSTOP")]
fn a_frozen_process_resumed_after_losing_its_lease_cannot_move_the_cursor_back() {
    let root = runtime::test_support::temp_dir_path("cursor-fence-processes");
    let gate = root.join("gate");
    std::fs::create_dir_all(&gate).unwrap();
    let b_hold = root.join("b-hold");
    let events_file = root.join("events");

    send(&root, "m1");
    let a = ChildReceiver::spawn(&root, "A", &events_file, Some(&gate), None);
    wait_until("A to take and record m1", || caught_up(&root, 1));
    let after_m1 = transcript_end(&root);

    // Park A inside its next commit, then freeze the whole process there.
    std::fs::write(gate.join("hold"), b"").unwrap();
    send(&root, "m2");
    wait_until("A to be parked in its commit", || {
        gate.join("entered").exists()
    });
    a.signal("STOP");

    // A stays frozen past its lease; a second process takes the conversation,
    // writes its fence and is held mid-delivery.
    std::fs::write(&b_hold, b"").unwrap();
    expire_leases_under(&root);
    let b = ChildReceiver::spawn(&root, "B", &events_file, None, Some(&b_hold));
    wait_until("B to fence and start delivering m2", || {
        b_hold.with_extension("entered").exists()
    });
    let fence = register(&root).unwrap();
    assert_eq!((fence.epoch, fence.read_offset), (2, after_m1));

    // A wakes up and finishes the write it started under its old version.
    std::fs::remove_file(gate.join("hold")).unwrap();
    a.signal("CONT");
    wait_until("A's parked write to be decided", || {
        gate.join("left").exists()
    });
    assert_eq!(
        register(&root).unwrap(),
        fence,
        "the resumed process must be refused by the fence, not record m2 over it"
    );

    send(&root, "m3");
    std::fs::remove_file(&b_hold).unwrap();
    wait_until("B to record m2 and m3", || caught_up(&root, 2));
    let delivered = events(&events_file);
    let by = |who: &str| -> Vec<String> {
        delivered
            .iter()
            .filter_map(|line| line.strip_prefix(&format!("{who} ")).map(str::to_string))
            .collect()
    };
    assert_eq!(
        by("A"),
        ["m1", "m2"],
        "a fenced-out process must not go on delivering: {delivered:?}"
    );
    assert_eq!(
        by("B"),
        ["m2", "m3"],
        "the message A never recorded is delivered again, not lost: {delivered:?}"
    );

    drop((a, b));
    let _ = std::fs::remove_dir_all(&root);
}
