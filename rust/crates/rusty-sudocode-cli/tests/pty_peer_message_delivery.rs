//! PTY e2e — a peer's message reaches a running REPL through the inbox poller.
//!
//! The delivery path this covers is the one the receive loop actually uses:
//!
//! ```text
//! Mailbox::send  →  .sudocode-inbox/team-lead.jsonl
//!                →  spawn_local_poller  (parked in Mailbox::poll / tail_read)
//!                →  CoordinatorEvent::PeerMessage
//!                →  the REPL's coordinator loop
//! ```
//!
//! Distinct from `pty_coordinator_push`, which covers the OTHER way a message
//! reaches a turn: `coordinator_notification::drain` prepending at the turn
//! boundary. Both exist, and only the drain one had an e2e test — so the poller
//! path, which is what a peer `send` goes through, had none.
//!
//! Real binary, real REPL, real inbox file, real poller. The send side uses the
//! same `runtime::mailbox` API the `send` tool calls, rather than asking a model
//! to decide to call it: a mock model cannot faithfully make that decision, and
//! it is not the part that was broken. Everything downstream of the append is
//! the production path, unmodified.
//!
//! Runs under the mock backend, so it runs in CI. The message body carries a
//! mock scenario marker, so the turn the delivery triggers is scripted rather
//! than whatever an unknown prompt would provoke.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::TestEnv;

/// The REPL's own inbox identity — `run_repl_loop` polls
/// `.sudocode-inbox/team-lead.jsonl`.
const REPL_SELF_ID: &str = "team-lead";

/// Distinctive enough that finding it on screen cannot be a coincidence.
const SENTINEL: &str = "PEER_DELIVERY_SENTINEL_QZX";

fn peer_mailbox(workspace: &Path) -> runtime::mailbox::Mailbox {
    runtime::mailbox::Mailbox::new(
        std::sync::Arc::new(runtime::fs_backend::StdFsBackend),
        "peer-bot".to_string(),
        runtime::mailbox::InboxConvention::LocalJsonl {
            root: workspace.to_string_lossy().into_owned(),
        },
    )
}

fn envelope(body: &str) -> runtime::agent_mailbox::MailboxEnvelope {
    runtime::agent_mailbox::MailboxEnvelope {
        from: "peer-bot".to_string(),
        to: REPL_SELF_ID.to_string(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: String::new(),
        request_id: None,
    }
}

/// Poll the rendered screen for `needle`.
///
/// Not `expect`: that matches the unconsumed byte stream, and iocraft redraws
/// the whole screen on any change, so chrome already on screen can satisfy a
/// stream match before anything happens.
fn expect_on_screen(sess: &mut pty_expect::PtySession, needle: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    loop {
        let screen = sess.render(|s| s.contents());
        if screen.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "screen never showed {needle:?} within {budget:?}\nPTY:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the receiver has recorded where it starts reading.
///
/// A first-ever run seeks to the inbox tail rather than replaying a backlog it
/// was never party to, so a message appended before that seek completes is
/// positioned past and never delivered. The test has to establish "listening"
/// before it sends, or it is racing the seek.
fn wait_for_receiver_ready(workspace: &Path, budget: Duration) {
    let cursor =
        runtime::agent_mailbox::mailbox_dir(workspace).join(format!(".cursor-{REPL_SELF_ID}"));
    let deadline = Instant::now() + budget;
    while !cursor.exists() {
        assert!(
            Instant::now() < deadline,
            "the inbox receiver never recorded its cursor at {}",
            cursor.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_peer_message_reaches_the_running_repl() {
    let env = TestEnv::new("pty-peer-delivery");
    let workspace = env.workspace_root().to_path_buf();
    std::fs::write(workspace.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(20));
    sess.expect("❯").expect("REPL prompt should appear");

    wait_for_receiver_ready(&workspace, Duration::from_secs(20));

    // Delivered by the same API the `send` tool uses.
    let body = env.prompt(&format!("{SENTINEL} please ack"), "single_turn_text");
    peer_mailbox(&workspace)
        .send(envelope(&body))
        .expect("peer send should land in the inbox");

    // The REPL announces a peer message as `📨 A2A from <name>: <body>`. Seeing
    // it proves the whole chain ran: the poller was parked on the inbox, woke,
    // put the envelope on the coordinator channel, and the loop took it.
    expect_on_screen(&mut sess, "A2A from peer-bot", Duration::from_secs(20));
    expect_on_screen(&mut sess, SENTINEL, Duration::from_secs(20));

    // The cursor must have advanced past a message the coordinator took —
    // otherwise the next poll re-delivers it forever.
    let cursor_file =
        runtime::agent_mailbox::mailbox_dir(&workspace).join(format!(".cursor-{REPL_SELF_ID}"));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let recorded = std::fs::read_to_string(&cursor_file)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if recorded > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the receiver never advanced its cursor past the delivered message"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
