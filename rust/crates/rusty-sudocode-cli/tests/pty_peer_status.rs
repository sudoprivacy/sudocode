//! `/status` says where a session's peers are, whether it receives for them,
//! and what keeps a resumed instance from moving a read position back.
//!
//! An agent has one receiver at a time across every instance of it, so a
//! session can be up and still not be the one receiving. The only way to tell
//! from the session itself is to ask it.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::MailboxEnvelope;
use runtime::mailbox::{local_pair_root_in, InboxConvention, Mailbox};

const AGENT: &str = "status-probe";
const PEER: &str = "status-peer";

fn screen_shows_all(sess: &mut pty_expect::PtySession, needles: &[&str], budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let screen = sess.render(common::screen_contents);
        if needles.iter().all(|needle| screen.contains(needle)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn status_reports_the_disk_namespace_and_that_this_session_receives() {
    let env = TestEnv::new("peer-status");
    node_home::pin_agent_name(env.workspace_root(), AGENT);
    let pair_root = local_pair_root_in(env.config_home());
    let reader_path = InboxConvention::new(pair_root.to_string_lossy().into_owned())
        .reader_path(AGENT, PEER, AGENT);
    std::fs::create_dir_all(std::path::Path::new(&reader_path).parent().unwrap()).unwrap();
    // An older record's diagnostic deadline grants no ownership. The actual
    // process lock admits this receiver and its next write keeps durable facts.
    std::fs::write(
        &reader_path,
        serde_json::to_vec(&serde_json::json!({
            "holder": "previous-process",
            "lease_expires_at": u64::MAX,
            "read_offset": 0,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"), ("PAGER", "")],
    );
    session.set_default_timeout(Duration::from_secs(30));
    session.expect("❯").expect("the REPL should start");

    // The cursor guarantee is decided when a conversation is first claimed,
    // so give the receiver one to claim.
    Mailbox::workspace_local(&local_pair_root_in(env.config_home()), PEER.to_string())
        .send(MailboxEnvelope {
            delivery: runtime::turn_input::InputDelivery::Steer,
            from: PEER.to_string(),
            to: AGENT.to_string(),
            body: "status check PARITY_SCENARIO:single_turn_text".to_string(),
            summary: None,
            timestamp: 0,
            color: None,
            kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .expect("the peer writes on the pair root");

    // The receiver claims the agent's seat and the conversation on its own
    // thread; ask again until it has, rather than racing it.
    let expected = [
        "Peers",
        &format!("Agent            {AGENT}") as &str,
        "Namespace        same-machine pair root",
        "Receiver         receiving",
        "Cursor           fenced: the lock excludes a frozen holder until it exits",
    ];
    let mut reported = false;
    for _ in 0..8 {
        session.send("/status\r").expect("send /status");
        if screen_shows_all(&mut session, &expected, Duration::from_secs(4)) {
            reported = true;
            break;
        }
    }
    assert!(
        reported,
        "/status never reported {expected:?}\nscreen:\n{}",
        session.render(common::screen_contents)
    );
    let register: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&reader_path).unwrap()).unwrap();
    assert!(register.get("lease_expires_at").is_none(), "{register}");
    let holder = register["holder"].as_str().unwrap();
    assert!(!holder.is_empty());
    assert_ne!(holder, "previous-process");
}
