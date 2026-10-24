//! A sync-REPL message that is still waiting when its receiver dies is not lost:
//! the next instance of the same agent receives it again.
//!
//! `pty_sync_repl_receive` proves one session announces a waiting peer message
//! and runs it on Enter, and that its read position only moves once the turn
//! has run. Its doc notes that "a session that exits first leaves it for the
//! next instance" — but a single-session test cannot show the next instance
//! actually getting it. This does: a peer writes, the receiver announces the
//! message but is killed before Enter (so no turn runs and the position never
//! advances), and a second instance of the same agent, started fresh, announces
//! the very same message. That is at-least-once delivery across a crash — the
//! unacknowledged message survives the receiver's death.
//!
//! Unix only, like the other PTY journeys here (this harness does not reliably
//! drive a child through Windows `ConPTY`); it compiles everywhere.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::{local_pair_root_in, Mailbox};

const AGENT: &str = "crash-receiver";
const PEER: &str = "crash-peer";
const BUDGET: Duration = Duration::from_secs(30);

fn expect_on_screen(sess: &mut pty_expect::PtySession, needle: &str) {
    let deadline = Instant::now() + BUDGET;
    loop {
        let screen = sess.render(|s| s.contents());
        if screen.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the screen never showed {needle:?} within {BUDGET:?}\nscreen:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_next_instance_receives_a_message_the_crashed_one_left_unrun() {
    if !cfg!(unix) {
        eprintln!("skipped: the PTY runtime journey is Unix-only in this harness");
        return;
    }

    let env = TestEnv::new("sync-crash-redeliver");
    node_home::pin_agent_name(env.workspace_root(), AGENT);

    // A peer writes one message to the agent's pair-root inbox.
    let pair_root = local_pair_root_in(env.config_home());
    Mailbox::workspace_local(&pair_root, PEER.to_string())
        .send(MailboxEnvelope {
            from: PEER.to_string(),
            to: AGENT.to_string(),
            body: "for the sync REPL PARITY_SCENARIO:single_turn_text".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("the peer writes on the pair root");
    let inbox = Mailbox::workspace_local(&pair_root, AGENT.to_string());
    let position = || {
        inbox
            .read_position(PEER)
            .expect("read the agent's position")
            .unwrap_or(0)
    };

    // The first instance announces the waiting message, but is killed before
    // Enter: no turn runs, so the message is never acknowledged.
    let mut first = spawn_instance(&env);
    first
        .expect("\u{276f}")
        .expect("the first REPL should start");
    expect_on_screen(&mut first, &format!("Message from {PEER} is waiting"));
    assert_eq!(position(), 0, "the announced message must still be unread");
    drop(first); // the receiver dies with the message still pending
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        position(),
        0,
        "a receiver that died before Enter must not have advanced the position"
    );

    // A second instance of the same agent, started fresh, receives it again.
    let mut second = spawn_instance(&env);
    second
        .expect("\u{276f}")
        .expect("the second REPL should start");
    expect_on_screen(&mut second, &format!("Message from {PEER} is waiting"));
    assert_eq!(
        position(),
        0,
        "redelivery must not have advanced the position before the message runs"
    );

    // The redelivered message is live, not a stale notice: Enter runs it and the
    // position finally advances.
    let turn_marker = common::turn_status_marker(&second);
    second
        .send("\r")
        .expect("press Enter on the second instance");
    if env.is_mock() {
        expect_on_screen(&mut second, "The answer is 4");
    }
    let deadline = Instant::now() + BUDGET;
    while position() == 0 {
        assert!(
            Instant::now() < deadline,
            "the redelivered message ran but its read position never moved"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // A durable read position can advance before the input row is rearmed.
    // Reuse the shared turn/ready gate before asking for a fresh receiver report.
    let ready_deadline = Instant::now() + BUDGET;
    common::expect_turn_complete_after(&second, &turn_marker, BUDGET, "redelivered peer turn");
    submit_line(&mut second, "/status", ready_deadline);
    common::expect_screen_settled(
        &second,
        |screen| {
            common::screen_contains(screen, "receiving; waiting messages run at the next prompt")
                && common::input_line_of(screen).is_empty()
        },
        ready_deadline.saturating_duration_since(Instant::now()),
        "the new receiver reports ownership and restores its input row",
    );

    submit_line(&mut second, "/exit", ready_deadline);
    let exit = second.expect_eof().unwrap_or_else(|error| {
        panic!(
            "the REPL did not exit: {error:?}\nPTY:\n{}",
            second.render(|screen| screen.contents())
        );
    });
    assert_eq!(exit, 0);
}

/// Observe the command in the live editor before Enter: a status block can
/// appear before raw input is rearmed, discarding a batched submit key.
fn submit_line(sess: &mut pty_expect::PtySession, line: &str, deadline: Instant) {
    common::expect_input_line_cleared(
        sess,
        deadline.saturating_duration_since(Instant::now()),
        "ready for the next command",
    );
    sess.send(line).expect("type command");
    common::expect_screen_settled(
        sess,
        |screen| common::input_line_of(screen) == line,
        deadline.saturating_duration_since(Instant::now()),
        "command reached the live editor",
    );
    sess.send("\r").expect("submit the rendered command");
}

fn spawn_instance(env: &TestEnv) -> pty_expect::PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "off"), ("PAGER", "")],
    );
    sess.set_default_timeout(BUDGET);
    sess
}
