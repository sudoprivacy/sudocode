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
#[path = "support/sync_input.rs"]
mod sync_input;

use std::path::Path;
use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::{local_pair_root_in, Mailbox};

const AGENT: &str = "sync-receiver";
const PEER: &str = "sync-peer";
const BUDGET: Duration = Duration::from_secs(30);

#[inline]
fn send_from(pair_root: &Path, peer: &str, body: &str) {
    Mailbox::workspace_local(pair_root, peer.to_string())
        .send(MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: peer.to_string(),
            to: AGENT.to_string(),
            body: body.to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("the peer writes on the pair root");
}

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
    session
        .send("draft-界-keep")
        .expect("type a fast Unicode draft");
    common::expect_input_line(&session, "draft-界-keep", BUDGET, "draft before notice");
    session
        .send("\x1b[D\x1b[D\x1b[D\x1b[D")
        .expect("move before keep");

    let pair_root = local_pair_root_in(env.config_home());
    send_from(
        &pair_root,
        PEER,
        "for the sync REPL PARITY_SCENARIO:single_turn_text",
    );
    let inbox = Mailbox::workspace_local(&pair_root, AGENT.to_string());
    let position = || {
        inbox
            .read_position(PEER)
            .expect("read the agent's position")
            .unwrap_or(0)
    };

    // Announced while the prompt waits, and not yet taken.
    expect_on_screen(&mut session, &format!("Message from {PEER} is waiting"));
    common::expect_input_line(&session, "draft-界-keep", BUDGET, "draft after notice");
    session.send("X").expect("type at the preserved cursor");
    common::expect_input_line(&session, "draft-界-Xkeep", BUDGET, "cursor after notice");
    // A single control-key burst also exercises the upstream buffered-input
    // fix. The draft is not submitted as a user or peer turn.
    session.send("\x05\x15").expect("end then clear the draft");
    common::expect_input_line_cleared(&session, BUDGET, "draft cleared before peer turn");
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

    sync_input::expect_prompt_after_turn(&session, &marker, BUDGET, "status at ready prompt");

    // And the session says how it receives, not just that it does.
    session.send("/status\r").expect("send /status");
    expect_on_screen(
        &mut session,
        "receiving; waiting messages run at the next prompt",
    );
    common::expect_input_line_cleared(&session, BUDGET, "status completed");

    // Closing with an announced, unconsumed delivery must release the receiver
    // and the printer worker. The notice is fresh (a distinct peer), and its
    // actual message must remain unread for the next owner.
    session
        .send("/exit")
        .expect("type exit before the next notice");
    send_from(&pair_root, "sync-exit-peer", "LEAVE_UNREAD_ON_EXIT");
    expect_on_screen(&mut session, "Message from sync-exit-peer is waiting");
    common::expect_input_line(&session, "/exit", BUDGET, "exit draft after notice");
    session.send("\r").expect("exit the REPL");
    assert_eq!(session.expect_eof().expect("the REPL exits"), 0);
    assert_eq!(
        inbox.read_position("sync-exit-peer").unwrap().unwrap_or(0),
        0
    );
}
