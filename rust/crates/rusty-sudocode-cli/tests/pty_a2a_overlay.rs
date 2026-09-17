//! PTY mock tests for inbound A2A message rendering — no daemon, no LLM `send`.
//!
//! A2A is DRY with human input, so a peer message must surface the same way an
//! input does, differing only in marker (`📨 A2A from X: …` vs `❯`):
//!   1. Received while idle → echoed to scrollback now (bold `📨` line) and a
//!      turn starts to handle it.
//!   2. Received during a running turn → held in the pending overlay as
//!      `↳ queued: 📨 A2A from X: …` (not scrollback), flushed at the turn
//!      boundary.
//!
//! Instead of a daemon or a second process, the test writes an envelope directly
//! into the receiver's own same-machine inbox (`{pair_root}/agents/{self}/
//! chat-with-me`) — exactly what a peer's `send` would produce — and reads the
//! receiver's REPL.
//!
//! The receiver's name is derived from its cwd, and that path can differ from
//! what the test sees (macOS resolves the temp dir through a `/private` symlink,
//! changing the FNV path hash), so the test does NOT recompute the name. It
//! discovers it: the local poller creates `agents/{self}/` with a `.cursor-{self}`
//! sibling on its startup seek. Finding that cursor is the readiness signal — the
//! seek has run, so a message injected afterward lands after the tail and cannot
//! be skipped — and its directory is the receiver's real inbox.

mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use runtime::agent_mailbox::{self, MailboxEnvelope};
use runtime::mailbox::local_pair_root_in;

const BUDGET: Duration = Duration::from_secs(30);

/// Wait until the receiver has registered under the pair root and started
/// polling, then return its real inbox path — discovered from the filesystem so
/// the test never has to reproduce the binary's cwd-derived name hash. The pair
/// root itself is stable: `{config_home}/local-mailbox`, and the harness sets
/// `SUDO_CODE_CONFIG_HOME` to the workspace config home the test knows.
fn wait_for_receiver_inbox(env: &TestEnv) -> std::path::PathBuf {
    let agents = local_pair_root_in(env.config_home()).join("agents");
    let deadline = Instant::now() + BUDGET;
    loop {
        if let Ok(entries) = std::fs::read_dir(&agents) {
            for agent_dir in entries.flatten().map(|e| e.path()) {
                let name = agent_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if agent_dir.join(format!(".cursor-{name}")).exists() {
                    return agent_dir.join("chat-with-me");
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no receiver cursor appeared under {} — the receiver is not polling, \
             so an injected message would be missed",
            agents.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn inject(inbox: &std::path::Path, from: &str, body: &str) {
    let env = MailboxEnvelope {
        from: from.to_string(),
        to: String::new(),
        body: body.to_string(),
        summary: Some("mock a2a".to_string()),
        timestamp: 0,
        color: None,
        kind: agent_mailbox::kinds::MESSAGE.to_string(),
        request_id: None,
    };
    agent_mailbox::append_envelope_to_path(&inbox.to_string_lossy(), env)
        .expect("inject a2a envelope");
}

/// Idle receipt: the message lands in scrollback with the `📨` marker.
#[test]
fn a2a_received_while_idle_surfaces_in_scrollback() {
    let env = TestEnv::new("a2a-idle");
    // Receiver must run the async REPL (the only loop that polls) — pin queue
    // mode so the harness default of `off` can't turn the poller off.
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "workspace-write"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(BUDGET);
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("async REPL initial prompt");

    let inbox = wait_for_receiver_inbox(&env);
    inject(&inbox, "mac-ai", "A2A-IDLE-MARKER");

    // The `📨 A2A from mac-ai: …` line is the whole receive chain: poller woke,
    // coordinator took the message, it rendered with the peer marker.
    sess.expect("A2A from mac-ai")
        .expect("idle a2a should surface with the 📨 marker");
    sess.expect("A2A-IDLE-MARKER")
        .expect("the message body should be shown");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}

/// During-turn receipt: the message is held in the pending overlay, not
/// scrollback, as a `↳ queued: 📨 …` line.
#[test]
fn a2a_received_during_a_turn_shows_in_pending_overlay() {
    let env = TestEnv::new("a2a-busy");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(BUDGET);
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("async REPL initial prompt");

    let inbox = wait_for_receiver_inbox(&env);

    // Put the receiver in a long-running turn so the injected a2a is queued.
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("send long prompt");
    sess.expect("interrupt-start")
        .expect("bash tool should start before we inject the a2a");

    inject(&inbox, "mac-ai", "A2A-BUSY-MARKER");

    // Held in the overlay as a queued peer line, NOT committed to scrollback yet.
    sess.expect("\u{21b3} queued: \u{1f4e8} A2A from mac-ai")
        .expect("during-turn a2a should render in the pending overlay");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
