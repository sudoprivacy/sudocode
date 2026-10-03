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
use pty_expect::PtySession;
use runtime::agent_mailbox::{self, MailboxEnvelope};
use runtime::mailbox::{local_pair_root_in, Mailbox};

const BUDGET: Duration = Duration::from_secs(30);

/// Match physical screen rows: `contents()` joins rows marked as soft-wrapped
/// by ConPTY, which can join the input to its separators and footer. Keep the
/// real row boundaries so caret and multiline-order assertions remain exact.
fn wait_for_screen(sess: &PtySession, context: &str, predicate: impl Fn(&str) -> bool) {
    common::expect_screen(
        sess,
        |_| {
            sess.render(|screen| {
                let (_, cols) = screen.size();
                let rows = screen.raw().rows(0, cols).collect::<Vec<_>>().join("\n");
                predicate(&rows)
            })
        },
        BUDGET,
        context,
    );
}

/// Only the live input region, excluding queued chips and prior scrollback.
fn input_text(screen: &str) -> String {
    screen
        .rsplit_once('❯')
        .map_or_else(String::new, |(_, tail)| {
            tail.lines()
                .take_while(|line| !line.trim().starts_with('─'))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
}

fn has_chip(screen: &str, marker: &str) -> bool {
    screen
        .lines()
        .any(|line| line.contains("queued:") && line.contains(marker))
}

/// Ctrl-D exits even when the input contains recalled text.
fn exit(sess: &mut PtySession) {
    sess.send("\x04").expect("exit REPL");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    sess.expect_eof().expect("REPL should exit cleanly");
}

/// Keep the model busy and wait until each submitted item is actually queued.
fn queued_session(label: &str, messages: &[&str]) -> (TestEnv, PtySession) {
    let env = TestEnv::new(label);
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(BUDGET);
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("initial prompt");
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("start long turn");
    sess.expect("interrupt-start").expect("tool started");
    for message in messages {
        sess.send(&format!("{message}\r"))
            .expect("queue human message");
        wait_for_screen(
            &sess,
            "human message must be queued and input cleared",
            |screen| has_chip(screen, message) && input_text(screen).is_empty(),
        );
    }
    (env, sess)
}

/// Wait until the receiver has announced itself under the pair root, then
/// return its name - discovered from the filesystem so the test never has to
/// reproduce the binary's cwd-derived name hash. The pair root itself is
/// stable: `{config_home}/local-mailbox`, and the harness sets
/// `SUDO_CODE_CONFIG_HOME` to the workspace config home the test knows.
///
/// The receiver announces by creating its chat list, which is also the
/// signal that its poller is up. There is no cursor file to watch for any
/// more: a read position now lives inside a conversation, and a receiver with
/// no conversations yet has none to write.
fn wait_for_receiver(env: &TestEnv) -> String {
    let agents = local_pair_root_in(env.config_home()).join("agents");
    let deadline = Instant::now() + BUDGET;
    loop {
        if let Ok(entries) = std::fs::read_dir(&agents) {
            for agent_dir in entries.flatten().map(|e| e.path()) {
                if agent_dir.join("conversations").is_dir() {
                    return agent_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "no receiver announced itself under {} - the receiver is not \
             polling, so an injected message would be missed",
            agents.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Deliver a message the way a real peer does: through a Mailbox rooted at
/// the same pair root, which provisions the conversation and appends to the
/// shared transcript. Writing a path by hand is what let this test keep
/// injecting into a file the receiver had stopped reading.
fn inject(env: &TestEnv, receiver: &str, from: &str, body: &str) {
    let mailbox =
        Mailbox::workspace_local(&local_pair_root_in(env.config_home()), from.to_string());
    mailbox
        .send(MailboxEnvelope {
            from: from.to_string(),
            to: receiver.to_string(),
            body: body.to_string(),
            summary: Some("mock a2a".to_string()),
            timestamp: 0,
            color: None,
            kind: agent_mailbox::kinds::MESSAGE.to_string(),
            request_id: None,
        })
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

    let receiver = wait_for_receiver(&env);
    inject(&env, &receiver, "mac-ai", "A2A-IDLE-MARKER");

    // The `📨 A2A from mac-ai: …` line is the whole receive chain: poller woke,
    // coordinator took the message, it rendered with the peer marker.
    sess.expect("A2A from mac-ai")
        .expect("idle a2a should surface with the 📨 marker");
    sess.expect("A2A-IDLE-MARKER")
        .expect("the message body should be shown");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let _ = sess.expect_eof();
}

/// `↑` with a human message and an a2a message both queued must pop the
/// HUMAN one back into the input slot and leave the a2a (peer) item queued —
/// a peer message is machine-delivered, not something the user typed, so it
/// must never be pulled into the edit buffer.
///
/// Regression guard for the kind-blind `pop_back` bug: with that bug, ↑ would
/// pop the peer item (queued last) instead, splicing the a2a body into the
/// input and dropping its overlay chip. This test queues a human line, then
/// injects a peer message so the peer sits at the queue tail, presses ↑, and
/// asserts (a) the human marker surfaces in the input line and (b) the peer
/// chip is still in the overlay.
#[test]
fn up_arrow_pops_human_and_skips_queued_a2a() {
    const HUMAN_MARKER: &str = "HUMAN-EDIT-MARKER";

    let env = TestEnv::new("a2a-up-skip");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(BUDGET);
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("async REPL initial prompt");

    let receiver = wait_for_receiver(&env);

    // Long-running turn so both the human submit and the injected a2a queue
    // behind it rather than running immediately.
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("send long prompt");
    sess.expect("interrupt-start")
        .expect("bash tool should start before we queue anything");

    // Queue a HUMAN message during the turn. Its overlay chip renders as
    // `↳ queued: HUMAN-EDIT-MARKER` (no `❯` in the chip display).
    sess.send(&format!("{HUMAN_MARKER}\r"))
        .expect("queue human marker during turn");
    sess.expect(&format!("\u{21b3} queued: {HUMAN_MARKER}"))
        .expect("human message should appear as a queued overlay chip");

    // Then a peer a2a lands AFTER it — now the queue tail is the peer item.
    inject(&env, &receiver, "mac-ai", "A2A-SKIP-MARKER");
    sess.expect("\u{21b3} queued: \u{1f4e8} A2A from mac-ai")
        .expect("a2a should be queued behind the human message");

    wait_for_screen(&sess, "both chips must be queued before recall", |screen| {
        has_chip(screen, HUMAN_MARKER)
            && has_chip(screen, "A2A from mac-ai")
            && input_text(screen).is_empty()
    });
    sess.send("\x1b[A").expect("send Up-arrow");
    wait_for_screen(
        &sess,
        "recall must move only the human item out of staging",
        |screen| {
            input_text(screen) == HUMAN_MARKER
                && !has_chip(screen, HUMAN_MARKER)
                && has_chip(screen, "A2A from mac-ai")
        },
    );
    exit(&mut sess);
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

    let receiver = wait_for_receiver(&env);

    // Put the receiver in a long-running turn so the injected a2a is queued.
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: \
         printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("send long prompt");
    sess.expect("interrupt-start")
        .expect("bash tool should start before we inject the a2a");

    inject(&env, &receiver, "mac-ai", "A2A-BUSY-MARKER");

    // Held in the overlay as a queued peer line, NOT committed to scrollback yet.
    sess.expect("\u{21b3} queued: \u{1f4e8} A2A from mac-ai")
        .expect("during-turn a2a should render in the pending overlay");

    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(15)));
    let _ = sess.expect_eof();
}

/// Typing after recall must append to the last line, including UTF-8 text.
#[test]
fn up_arrow_recall_places_caret_at_end_of_text() {
    const MARKER: &str = "CARET-召回";
    let (_env, mut sess) = queued_session("a2a-up-caret", &[MARKER]);
    sess.send("\x1b[A").expect("recall human");
    wait_for_screen(&sess, "human recalled", |screen| {
        input_text(screen) == MARKER && !has_chip(screen, MARKER)
    });
    sess.send("!").expect("type sentinel");
    wait_for_screen(&sess, "caret must append on the same line", |screen| {
        input_text(screen) == format!("{MARKER}!")
    });
    exit(&mut sess);
}

/// Repeated recall preserves submit order and stops at a drained queue.
#[test]
fn up_arrow_twice_stacks_two_queued_humans_in_order() {
    const FIRST: &str = "FIRST-QUEUED";
    const SECOND: &str = "SECOND-QUEUED";
    let (_env, mut sess) = queued_session("a2a-up-stack", &[FIRST, SECOND]);
    sess.send("\x1b[A").expect("recall newest");
    wait_for_screen(&sess, "newest recalled first", |screen| {
        input_text(screen) == SECOND && !has_chip(screen, SECOND) && has_chip(screen, FIRST)
    });
    sess.send("\x1b[A").expect("recall older");
    let stacked = format!("{FIRST}\n{SECOND}");
    wait_for_screen(&sess, "both messages stacked in submit order", |screen| {
        input_text(screen) == stacked && !has_chip(screen, FIRST) && !has_chip(screen, SECOND)
    });
    sess.send("!").expect("type at end of stacked input");
    wait_for_screen(&sess, "stack caret must remain on final line", |screen| {
        input_text(screen) == format!("{stacked}!")
    });
    // Submit and recall again: verify that the composed multiline text goes
    // through the real queue without losing either message or its newline.
    sess.send("\r").expect("resubmit stack");
    wait_for_screen(&sess, "edited stack queued", |screen| {
        input_text(screen).is_empty() && has_chip(screen, FIRST)
    });
    sess.send("\x1b[A").expect("recall edited stack");
    wait_for_screen(&sess, "stack survives resubmission", |screen| {
        input_text(screen) == format!("{stacked}!") && !has_chip(screen, FIRST)
    });
    exit(&mut sess);
}

/// Editing or pasting ends recall mode: ↑ moves the cursor, not another item.
#[test]
fn editing_recalled_input_keeps_older_message_queued() {
    for (label, edit) in [("typed", "!"), ("pasted", "\x1b[200~!\x1b[201~")] {
        const OLDER: &str = "OLDER-QUEUED";
        const NEWER: &str = "NEWER-QUEUED";
        let (_env, mut sess) = queued_session(label, &[OLDER, NEWER]);
        sess.send("\x1b[A").expect("recall newest");
        wait_for_screen(&sess, "newest recalled", |screen| {
            input_text(screen) == NEWER
        });
        sess.send(edit).expect("edit recalled input");
        wait_for_screen(&sess, "edit applied", |screen| {
            input_text(screen) == format!("{NEWER}!")
        });
        sess.send("\x1b[A@").expect("Up then type sentinel");
        wait_for_screen(
            &sess,
            "edited input must leave older item queued",
            |screen| {
                let input = input_text(screen);
                input.contains(NEWER)
                    && input.contains('!')
                    && input.contains('@')
                    && !input.contains(OLDER)
                    && has_chip(screen, OLDER)
            },
        );
        exit(&mut sess);
    }
}

/// Down exits recall mode and restores ordinary cursor navigation.
#[test]
fn down_after_recall_keeps_older_message_queued() {
    const OLDER: &str = "OLDER-QUEUED";
    const NEWER: &str = "NEWER-QUEUED";
    let (_env, mut sess) = queued_session("a2a-up-down", &[OLDER, NEWER]);
    sess.send("\x1b[A").expect("recall newest");
    wait_for_screen(&sess, "newest recalled", |screen| {
        input_text(screen) == NEWER
    });
    sess.send("\x1b[B\x1b[A@")
        .expect("Down, Up then type sentinel");
    wait_for_screen(&sess, "Down must end queue recall", |screen| {
        let input = input_text(screen);
        input.contains(NEWER)
            && input.contains('@')
            && !input.contains(OLDER)
            && has_chip(screen, OLDER)
    });
    exit(&mut sess);
}
