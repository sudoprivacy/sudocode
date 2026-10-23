//! The synchronous REPL receives its peers too.
//!
//! `SUDOCODE_INTERRUPT_QUEUE_MODE=off` selects the rustyline REPL, which used to
//! run no receiver at all: peer messages sat unread while the session looked
//! alive. rustyline cannot be interrupted from another thread, so a message
//! that arrives while the prompt waits is announced at once and runs as a turn
//! when the user next presses Enter. Until then it is not acknowledged, so a
//! session that exits first leaves it for the next instance of the agent.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::{local_pair_root_in, Mailbox};

const AGENT: &str = "sync-receiver";
const PEER: &str = "sync-peer";
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
fn a_peer_message_waits_at_the_sync_prompt_and_runs_on_enter() {
    let env = TestEnv::new("sync-receive");
    node_home::pin_agent_name(env.workspace_root(), AGENT);
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "off"), ("PAGER", "")],
    );
    session.set_default_timeout(BUDGET);
    session.expect("❯").expect("the REPL should start");
    common::expect_input_line_cleared(&session, BUDGET, "sync receiver ready");

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

    // Announced while the prompt waits, and not yet taken.
    expect_on_screen(&mut session, &format!("Message from {PEER} is waiting"));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(
        position(),
        0,
        "a message that has not run yet must stay unread"
    );

    // Enter hands the prompt back, and the waiting message runs as a turn.
    let marker = common::turn_status_marker(&session);
    session.send("\r").expect("press Enter");
    if env.is_mock() {
        expect_on_screen(&mut session, "The answer is 4");
    }
    let deadline = Instant::now() + BUDGET;
    while position() == 0 {
        assert!(
            Instant::now() < deadline,
            "the message ran but its read position never moved"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // A committed reader is not a re-armed prompt. Wait for this actual turn
    // and the ready input row before submitting the next command.
    common::expect_turn_complete_after(&session, &marker, BUDGET, "peer turn completed");
    common::expect_input_line_cleared(&session, BUDGET, "status at ready prompt");

    // And the session says how it receives, not just that it does.
    session.send("/status\r").expect("send /status");
    expect_on_screen(
        &mut session,
        "receiving; waiting messages run at the next prompt",
    );
    common::expect_input_line_cleared(&session, BUDGET, "status completed");

    session.send("/exit\r").expect("exit the REPL");
    assert_eq!(session.expect_eof().expect("the REPL exits"), 0);
}
