//! A peer message whose turn is still running when its receiver dies is not
//! lost: the next instance of the same agent receives and runs it again.
//!
//! The receiver moves a conversation's read position only after the turn the
//! message started has finished. A peer writes, the first instance starts the
//! message's turn and is killed while the model is still answering, so the
//! position never advances; a second instance of the same agent, started
//! fresh, runs the very same message to completion. That is at-least-once
//! delivery across a crash.
//!
//! Mock only: the mock holds its answer long enough to kill the receiver
//! mid-turn. Unix only, like the other PTY journeys here (this harness does not
//! reliably drive a child through Windows `ConPTY`); it compiles everywhere.

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
        let screen = sess.render(common::screen_contents);
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
fn the_next_instance_runs_a_message_the_crashed_one_left_unfinished() {
    let env = TestEnv::new("peer-crash-redeliver");
    if !cfg!(unix) || !env.is_mock() {
        eprintln!("skipped: needs the mock's held answer and a Unix PTY");
        return;
    }
    node_home::pin_agent_name(env.workspace_root(), AGENT);

    // A peer writes one message to the agent's pair-root inbox. The mock holds
    // its answer for a few seconds, so the receiver can die mid-turn.
    let pair_root = local_pair_root_in(env.config_home());
    Mailbox::workspace_local(&pair_root, PEER.to_string())
        .send(MailboxEnvelope {
            send_id: None,
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: PEER.to_string(),
            to: AGENT.to_string(),
            body: "for the crashed receiver PARITY_SCENARIO:delayed_text".to_string(),
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

    // The first instance starts the message's turn on its own and is killed
    // while the model is still answering: the message is never acknowledged.
    let mut first = spawn_instance(&env);
    expect_on_screen(&mut first, &format!("Message from {PEER}"));
    assert_eq!(
        position(),
        0,
        "a running turn must not acknowledge its message"
    );
    drop(first); // the receiver dies with the turn unfinished
    std::thread::sleep(mock_anthropic_service::DELAYED_TEXT_LATENCY + Duration::from_secs(1));
    assert_eq!(
        position(),
        0,
        "a receiver that died mid-turn must not have advanced the position"
    );

    // A second instance of the same agent, started fresh, runs it to completion.
    let mut second = spawn_instance(&env);
    expect_on_screen(&mut second, &format!("Message from {PEER}"));
    expect_on_screen(&mut second, "Mock streaming says hello");
    let deadline = Instant::now() + BUDGET;
    while position() == 0 {
        assert!(
            Instant::now() < deadline,
            "the redelivered message ran but its read position never moved"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let ready_deadline = Instant::now() + BUDGET;
    submit_line(&mut second, "/status", ready_deadline);
    common::expect_screen_settled(
        &second,
        |screen| {
            common::screen_contains(screen, "receiving") && common::input_line_of(screen).is_empty()
        },
        ready_deadline.saturating_duration_since(Instant::now()),
        "the new receiver reports ownership and restores its input row",
    );

    submit_line(&mut second, "/exit", ready_deadline);
    let exit = second.expect_eof().unwrap_or_else(|error| {
        panic!(
            "the REPL did not exit: {error:?}\nPTY:\n{}",
            second.render(common::screen_contents)
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
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &[("PAGER", "")]);
    sess.set_default_timeout(BUDGET);
    sess
}
