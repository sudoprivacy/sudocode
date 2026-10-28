//! A frozen receiver keeps its reader lease, so a second receiver on the same
//! identity cannot take over while the first is only stopped — not dead.
//!
//! This is the disk side of the reader-lease handoff. On disk the lease is an
//! OS file lock held for as long as the holder's process exists: its renew is a
//! no-op and the lock lives with the open descriptor, so freezing the holder —
//! a laptop asleep, a paused VM, a `SIGSTOP` — does not release it even past the
//! lease TTL. A second receiver therefore must not advance the delivery cursor
//! while the first is frozen, and after the first resumes the cursor must never
//! have moved backwards (delivery stays at-least-once, never exactly-once).
//!
//! The journey runs on Unix only: this harness does not reliably drive a child
//! through Windows `ConPTY` (see sudoprivacy/pty-expect#1), and the Windows freeze
//! path is covered separately by manual verification. The test still compiles on
//! every platform so a Windows build catches type errors here.

mod common;

// Included by path rather than placed in `tests/common/`: that directory is the
// render-performance harness's shared crate, a protected path whose every change
// needs a maintainer review. This freeze helper has nothing to do with rendering
// — only the disk receiver tests use it — so it lives outside the harness.
#[path = "support/freeze.rs"]
mod freeze;

use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::{kinds, MailboxEnvelope};
use runtime::mailbox::{local_pair_root_in, Mailbox};

/// The single identity both spawned receivers share.
const AGENT: &str = "freeze-receiver";
/// The peer whose conversation carries the delivery cursor under test.
const SENDER: &str = "freeze-sender";
/// `READER_LEASE_MS` in `runtime::mailbox`. The freeze outlasts it, so the test
/// proves the disk lock ignores the TTL rather than merely racing under it.
const READER_LEASE_MS: u64 = 30_000;

#[test]
fn a_frozen_receiver_keeps_its_lease_so_a_second_one_cannot_take_over() {
    if !cfg!(unix) {
        eprintln!("skipped: the PTY runtime journey is Unix-only in this harness");
        return;
    }

    let env = TestEnv::new("receiver-freeze");

    // Pin one identity. Both scode processes below read this same settings file,
    // so they are two receivers for the same agent, not two different agents.
    let settings_path = env.config_home().join("settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["agentName"] = serde_json::json!(AGENT);
    std::fs::write(&settings_path, settings.to_string()).unwrap();

    let root = local_pair_root_in(env.config_home());
    let inbox = Mailbox::workspace_local(&root, AGENT.to_string());
    let outbox = Mailbox::workspace_local(&root, SENDER.to_string());

    // A first message, persisted before any receiver exists.
    send_message(
        &outbox,
        &env.prompt("Reply with exactly FREEZE_ONE.", "single_turn_text"),
    );

    // Receiver A starts, consumes the first message, and commits the cursor: it
    // now holds the reader lease for this conversation.
    let mut a = spawn_receiver(&env);
    let committed = wait_until(Duration::from_secs(90), || read_offset(&inbox) > 0);
    assert!(
        committed,
        "receiver A never committed the first delivery:\n{}",
        screen_dump(&a)
    );
    let offset1 = read_offset(&inbox);

    // Take A's pid while it is alive, and prove it is the scode image. If the
    // harness ever placed a shell between the PTY and scode, freezing that pid
    // would stop the shell and the test would measure nothing.
    let pid_a = a.process_id().expect("a live receiver has a process id");
    assert!(
        image_is_scode(pid_a),
        "pid {pid_a} is not the scode image; a wrapper would make this test vacuous"
    );

    // Freeze A past the lease TTL.
    let frozen = freeze::freeze(pid_a).expect("freeze receiver A");
    let froze_at = Instant::now();

    // A second message arrives, and receiver B starts on the SAME identity while
    // A is frozen.
    send_message(
        &outbox,
        &env.prompt("Reply with exactly FREEZE_TWO.", "single_turn_text"),
    );
    let mut b = spawn_receiver(&env);

    // Across the whole freeze — held deliberately past the TTL — the cursor must
    // not move: A's lock is still held, so B cannot take over.
    let hold = Duration::from_millis(READER_LEASE_MS + 5_000);
    while froze_at.elapsed() < hold {
        assert_eq!(
            read_offset(&inbox),
            offset1,
            "the delivery cursor advanced while the lease holder was frozen; \
             a disk handoff must not happen"
        );
        std::thread::sleep(Duration::from_millis(500));
    }

    // Resume A. The cursor must never have regressed, and A — still the holder —
    // eventually delivers the second message.
    frozen.resume().expect("resume receiver A");
    let advanced = wait_until(Duration::from_secs(90), || read_offset(&inbox) > offset1);
    let offset2 = read_offset(&inbox);
    assert!(
        offset2 >= offset1,
        "the delivery cursor regressed after resume: {offset2} < {offset1}"
    );
    assert!(
        advanced,
        "receiver A never delivered the second message after resume:\n{}",
        screen_dump(&a)
    );

    b.send("/exit\r").ok();
    let _ = b.expect_eof();
    a.send("/exit\r").ok();
    let _ = a.expect_eof();
}

/// The terminal's current contents, for an assertion failure message. A closure
/// is required: `render`'s bound is higher-ranked over the screen's lifetime,
/// which a bare `Screen::contents` fn item cannot satisfy.
#[allow(clippy::redundant_closure_for_method_calls)]
fn screen_dump(sess: &pty_expect::PtySession) -> String {
    sess.render(common::screen_contents)
}

fn spawn_receiver(env: &TestEnv) -> pty_expect::PtySession {
    env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    )
}

fn send_message(outbox: &Mailbox, body: &str) {
    outbox
        .send(MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: SENDER.into(),
            to: AGENT.into(),
            body: body.to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: kinds::MESSAGE.into(),
            request_id: None,
        })
        .expect("persist a delivery");
}

fn read_offset(inbox: &Mailbox) -> u64 {
    inbox.read_position(SENDER).unwrap().unwrap_or(0)
}

/// The committed cursor is the backend truth; poll it rather than the terminal,
/// which can render a frame later than the durable write.
fn wait_until(budget: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    done()
}

/// Whether `pid` is a scode process. `ps -o comm=` is portable across Linux and
/// macOS; only reached on Unix, where the journey runs.
fn image_is_scode(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("scode"))
}
