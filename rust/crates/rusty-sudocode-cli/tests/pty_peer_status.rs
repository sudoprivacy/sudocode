//! `/status` says where a session's peers are and whether it receives for them.
//!
//! An agent has one receiver at a time across every instance of it, so a
//! session can be up and still not be the one receiving. The only way to tell
//! from the session itself is to ask it.

mod common;
#[path = "support/node_home.rs"]
mod node_home;

use std::time::{Duration, Instant};

use common::TestEnv;

const AGENT: &str = "status-probe";

fn screen_shows_all(sess: &mut pty_expect::PtySession, needles: &[&str], budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        let screen = sess.render(|s| s.contents());
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
    let mut session = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"), ("PAGER", "")],
    );
    session.set_default_timeout(Duration::from_secs(30));
    session.expect("❯").expect("the REPL should start");

    // The receiver claims the agent's seat on its own thread; ask again until
    // it has, rather than racing it.
    let expected = [
        "Peers",
        &format!("Agent            {AGENT}") as &str,
        "Namespace        same-machine pair root",
        "Receiver         receiving",
    ];
    let mut reported = false;
    for _ in 0..5 {
        session.send("/status\r").expect("send /status");
        if screen_shows_all(&mut session, &expected, Duration::from_secs(4)) {
            reported = true;
            break;
        }
    }
    assert!(
        reported,
        "/status never reported {expected:?}\nscreen:\n{}",
        session.render(|s| s.contents())
    );
}
