//! A frozen holder of an agent's receiver seat does not hand it off; a peer
//! instance of the same agent stays on standby until the holder actually exits.
//!
//! D1's own `one_receiver_per_agent.rs` proves the seat in-process: two pollers
//! in one process, abort one, the other takes over. Abort is an exit. A seat
//! holder that is merely stopped — a laptop asleep, a `SIGSTOP`, a paused VM —
//! is the harder case, and an in-process test cannot reach it, because freezing
//! is per-process: you cannot stop one thread and keep the test driving. So
//! this runs two real scode processes for one agent and freezes the one holding
//! the seat. The seat is an OS file lock at `agents/<name>/receiver.lock`; a
//! frozen process keeps it (the lock has no timeout — it lives with the open
//! descriptor), so the peer must stay on standby, and only the holder's exit
//! releases it.
//!
//! Unix only, like the other PTY journeys here: this harness does not reliably
//! drive a child through Windows `ConPTY` (sudoprivacy/pty-expect#1), and the
//! Windows freeze path is covered by manual verification. It still compiles
//! everywhere so a Windows build type-checks it.

mod common;
#[path = "support/freeze.rs"]
mod freeze;
#[path = "support/node_home.rs"]
mod node_home;

use std::time::{Duration, Instant};

use common::TestEnv;

/// The one identity both instances run as.
const AGENT: &str = "identity-seat-probe";

/// `/status` line of the instance that holds the seat.
const RECEIVING: &str = "Receiver         receiving";
/// Substring of the `/status` line of an instance that does not (field-matched,
/// not a whole-line compare, so D3 adding a `delivery` field won't break it).
const STANDBY: &str = "Receiver         standing by";

#[test]
fn a_frozen_seat_holder_keeps_the_seat_and_a_peer_stays_on_standby() {
    if !cfg!(unix) {
        eprintln!("skipped: the PTY runtime journey is Unix-only in this harness");
        return;
    }

    let env = TestEnv::new("identity-receiver-freeze");
    node_home::pin_agent_name(env.workspace_root(), AGENT);

    // Instance A starts and claims the agent's one receiver seat.
    let mut a = spawn_instance(&env);
    a.expect("\u{276f}").expect("instance A's REPL starts");
    assert!(
        status_shows(&mut a, RECEIVING),
        "A never became the receiver:\n{}",
        screen_dump(&a)
    );

    // Take A's pid while it is alive and prove it is the scode image; freezing a
    // shell between the PTY and scode would make this test measure nothing.
    let pid_a = a.process_id().expect("a live instance has a process id");
    assert!(
        image_is_scode(pid_a),
        "pid {pid_a} is not the scode image; a wrapper would make this test vacuous"
    );

    // Freeze A. It still holds the seat's OS lock, so a peer cannot take over.
    let frozen = freeze::freeze(pid_a).expect("freeze instance A");

    // Instance B (same identity, same pair root) can only stand by.
    let mut b = spawn_instance(&env);
    b.expect("\u{276f}").expect("instance B's REPL starts");
    assert!(
        status_shows(&mut b, STANDBY),
        "B did not stand by while the frozen holder kept the seat:\n{}",
        screen_dump(&b)
    );

    // Held deliberately: a frozen holder must not hand off. Re-check that B is
    // still standby, never flips to receiving, while A stays frozen.
    std::thread::sleep(Duration::from_secs(3));
    b.send("/status\r").expect("re-ask B for status");
    let b_screen = screen_dump(&b);
    assert!(
        b_screen.contains(STANDBY) && !b_screen.contains(RECEIVING),
        "B took over the seat while the holder was only frozen, not exited:\n{b_screen}"
    );

    // Resume A and let it exit. Only the exit releases the seat.
    frozen.resume().expect("resume instance A");
    a.send("/exit\r").ok();
    let _ = a.expect_eof();

    // Now B takes over.
    assert!(
        status_shows(&mut b, RECEIVING),
        "B never took over after the holder exited:\n{}",
        screen_dump(&b)
    );

    b.send("/exit\r").ok();
    let _ = b.expect_eof();
}

/// The terminal's current contents. A closure is required: `render`'s bound is
/// higher-ranked over the screen's lifetime, which a bare `Screen::contents` fn
/// item cannot satisfy.
#[allow(clippy::redundant_closure_for_method_calls)]
fn screen_dump(sess: &pty_expect::PtySession) -> String {
    sess.render(|s| s.contents())
}

fn spawn_instance(env: &TestEnv) -> pty_expect::PtySession {
    env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"), ("PAGER", "")],
    )
}

/// Ask `/status` until `needle` shows, retrying the command a few times because
/// the receiver claims its seat on its own thread rather than at startup.
fn status_shows(sess: &mut pty_expect::PtySession, needle: &str) -> bool {
    for _ in 0..8 {
        sess.send("/status\r").expect("send /status");
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if screen_dump(sess).contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    false
}

/// Whether `pid` is a scode process. `ps -o comm=` is portable across Linux and
/// macOS; only reached on Unix, where the journey runs.
fn image_is_scode(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("scode"))
}
