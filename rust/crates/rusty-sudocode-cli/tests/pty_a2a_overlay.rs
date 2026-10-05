//! PTY acceptance for inbound A2A: isolated real mailbox delivery, queueing,
//! source styling and persisted replay. Runs with mock or live model backends.
//! Peer messages share input scheduling, but retain their sender and quote
//! gutter instead of impersonating the human prompt or the assistant answer.

mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use runtime::agent_mailbox::{self, MailboxEnvelope};
use runtime::mailbox::{local_pair_root_in, Mailbox};

const BUDGET: Duration = Duration::from_secs(30);

/// Match physical screen rows: `contents()` joins rows marked as soft-wrapped
/// by `ConPTY`, which can join the input to its separators and footer. Keep the
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

fn receive_and_resume(background: &str, sender_color: &str, no_color: bool) {
    let env = TestEnv::new("a2a-received-style");
    let vars = [
        ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
        ("NO_COLOR", if no_color { "1" } else { "" }),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("COLORFGBG", background),
    ];
    let mut sess = env.spawn_with_env(&["--permission-mode", "read-only"], &vars);
    sess.resize(80, 52).unwrap();
    common::expect_input_line_cleared(&sess, BUDGET, "receiver ready");
    let receiver = wait_for_receiver(&env);
    let marker = common::turn_status_marker(&sess);
    let body = format!(
        "Pong received.\n\n**Strong** and `code`.\n\n```rust\nlet value = 7;\n```\n\n{}\n\nA2A-BODY-END\n\nReply only with A2A-ACK. Do not call tools.",
        "界".repeat(60)
    );
    let body = format!(
        "{body}\n\n{}",
        env.prompt("Reply only with A2A-ACK.", "single_turn_text")
    );
    inject(&env, &receiver, "mac-ai", &body);
    wait_for_screen(&sess, "complete received block", |s| {
        s.contains("│ A2A-BODY-END") && s.contains("╰─")
    });
    let live = received_block(&sess, sender_color, no_color);
    common::expect_turn_complete_after(
        &sess,
        &marker,
        common::LIVE_TURN_BUDGET,
        "received message handled",
    );
    exit(&mut sess);

    let mut resumed = env.spawn_with_env(
        &["--resume", "latest", "--permission-mode", "read-only"],
        &vars,
    );
    resumed.resize(80, 52).unwrap();
    wait_for_screen(&resumed, "restored received block", |s| {
        s.contains("│ A2A-BODY-END") && s.contains("╰─")
    });
    assert_eq!(
        received_block(&resumed, sender_color, no_color),
        live,
        "live and replay must use the same layout"
    );
    exit(&mut resumed);
}

fn received_block(sess: &PtySession, sender_color: &str, no_color: bool) -> Vec<String> {
    sess.render(|screen| {
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let start = rows
            .iter()
            .position(|r| r == "╭─ Message from mac-ai")
            .expect("sender header");
        let end = start
            + rows[start..]
                .iter()
                .position(|r| r == "╰─")
                .expect("message frame end");
        assert!(
            rows[start + 1..end].iter().all(|r| r.starts_with('│')),
            "every body row needs a gutter: {rows:?}"
        );
        assert!(!rows.join("\n").contains("<mailbox-message"));
        let rendered = rows[start..=end].join("\n");
        assert!(
            rendered.contains("Strong and code."),
            "inline Markdown: {rendered}"
        );
        assert!(
            rendered.contains("let value = 7;"),
            "complete code body: {rendered}"
        );
        let header = screen
            .raw()
            .cell(u16::try_from(start).unwrap(), 16)
            .unwrap();
        assert_eq!(format!("{:?}", header.fgcolor()), sender_color);
        assert!(!header.dim(), "peer identity must keep its blue contrast");
        let body = screen
            .raw()
            .cell(u16::try_from(start + 1).unwrap(), 2)
            .unwrap();
        assert_eq!(
            format!("{:?}", body.fgcolor()),
            "Default",
            "plain body color"
        );
        assert!(!body.bold(), "only Markdown emphasis should be bold");
        assert!(!body.dim(), "body must retain normal brightness");
        let border = screen
            .raw()
            .cell(u16::try_from(start + 1).unwrap(), 0)
            .unwrap();
        let muted = if no_color {
            "Default"
        } else if sender_color == "Rgb(28, 100, 200)" {
            "Idx(241)"
        } else {
            "Idx(247)"
        };
        assert_eq!(format!("{:?}", border.fgcolor()), muted);
        for (row, col) in [(start, 0), (start, 3), (start + 1, 0), (end, 0)] {
            let cell = screen.raw().cell(u16::try_from(row).unwrap(), col).unwrap();
            assert_eq!(format!("{:?}", cell.fgcolor()), muted);
            assert!(!cell.dim(), "do not dim the muted frame a second time");
            assert!(!cell.bold(), "message chrome stays quiet");
        }
        assert_eq!(
            rows[start..=end].join("").matches('界').count(),
            60,
            "full Unicode body"
        );
        // ConPTY can encode trailing blank cells as either literal spaces or
        // erased cells. Compare visible physical rows, keeping all leading
        // indentation, interior spacing and blank rows (styles checked above).
        rows[start..=end]
            .iter()
            .map(|row| row.trim_end_matches(' ').to_owned())
            .collect()
    })
}

#[test]
fn a2a_received_while_idle_surfaces_in_scrollback() {
    receive_and_resume("15;0", "Rgb(99, 168, 248)", false);
}

#[test]
fn a2a_received_light_theme_and_replay() {
    receive_and_resume("0;15", "Rgb(28, 100, 200)", false);
}

#[test]
fn a2a_received_without_color_and_replay() {
    receive_and_resume("15;0", "Default", true);
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
    sess.expect("\u{21b3} queued: Message from mac-ai")
        .expect("a2a should be queued behind the human message");

    wait_for_screen(&sess, "both chips must be queued before recall", |screen| {
        has_chip(screen, HUMAN_MARKER)
            && has_chip(screen, "Message from mac-ai")
            && input_text(screen).is_empty()
    });
    sess.send("\x1b[A").expect("send Up-arrow");
    wait_for_screen(
        &sess,
        "recall must move only the human item out of staging",
        |screen| {
            input_text(screen) == HUMAN_MARKER
                && !has_chip(screen, HUMAN_MARKER)
                && has_chip(screen, "Message from mac-ai")
        },
    );
    exit(&mut sess);
}

/// A queued preview resizes in one row, leaves editing responsive, then flushes
/// the full body exactly once. The preview must never replace the queued body.
#[test]
fn a2a_received_during_a_turn_shows_in_pending_overlay() {
    let (env, mut sess) = queued_session("a2a-busy", &[]);
    let receiver = wait_for_receiver(&env);
    let first = format!("A2A-BUSY-MARKER {}", "preview ".repeat(12));
    let body = format!("{first}\n\nQUEUED-BODY-END\n\nReply only with A2A-ACK. Do not call tools.");
    let body = format!(
        "{body}\n\n{}",
        env.prompt("Reply only with A2A-ACK.", "single_turn_text")
    );
    inject(&env, &receiver, "mac-ai", &body);
    wait_for_screen(&sess, "peer preview queued", |s| {
        has_chip(s, "Message from mac-ai")
    });
    sess.resize(60, 42).unwrap();
    wait_for_screen(&sess, "narrow one-line preview", |s| {
        s.lines()
            .any(|l| l.contains("queued: Message from mac-ai") && l.ends_with('…'))
            && !s.contains("QUEUED-BODY-END")
    });
    sess.send("draft-中文").unwrap();
    common::expect_input_line(&sess, "draft-中文", BUDGET, "typing with peer queued");
    sess.resize(60, 180).unwrap();
    wait_for_screen(&sess, "widened preview recovers full first line", |s| {
        s.lines()
            .any(|l| l.contains("queued: Message from mac-ai") && l.contains(first.trim_end()))
    });
    sess.send(&"\x7f".repeat(8)).unwrap(); // Clear the draft before checking readiness.
                                           // A running turn keeps the status animation moving. Observe the input,
                                           // rather than requiring the entire terminal to stop repainting.
    wait_for_screen(&sess, "draft cleared", |screen| {
        screen.contains('❯') && input_text(screen).is_empty()
    });
    sess.send("\x1b").unwrap(); // Cancel Bash; the queued peer now runs.
    wait_for_screen(&sess, "complete peer body flushed", |s| {
        s.contains("│ QUEUED-BODY-END") && !has_chip(s, "Message from mac-ai")
    });
    sess.render(|screen| {
        let text = screen.raw().contents();
        assert_eq!(
            text.lines()
                .filter(|l| l.trim() == "╭─ Message from mac-ai")
                .count(),
            1
        );
        assert!(!text.contains("<mailbox-message"));
    });
    exit(&mut sess);
}

/// Legacy sessions may mix humans and multiple peer envelopes in one queued
/// turn. Recognize complete wrappers only, leaving quoted examples untouched.
#[test]
fn a2a_replay_preserves_mixed_sources_and_literal_markup() {
    use runtime::{ContentBlock, ConversationMessage, Session};
    let env = TestEnv::new_mock("a2a-mixed-replay");
    let mut saved = Session::new().with_workspace_root(env.workspace_root().to_path_buf());
    saved
        .push_user_text(concat!(
            "HUMAN-BEFORE\n\n",
            "<mailbox-message from=\"peer-one\">\nFIRST-PEER\n</mailbox-message>\n\n",
            "HUMAN-BETWEEN\n\n",
            "<mailbox-message from=\"peer-two&amp;three\">\nSECOND-PEER\n</mailbox-message>\n\n",
            "HUMAN-AFTER"
        ))
        .unwrap();
    saved
        .push_user_text(concat!(
            "Keep this example literal:\n\n```xml\n\n",
            "<mailbox-message from=\"code-example\">\nEXAMPLE-BODY\n</mailbox-message>\n\n```\n\n",
            "Malformed wrapper:\n\n<mailbox-message from=\"incomplete\">\nKEEP-THIS-TEXT"
        ))
        .unwrap();
    saved
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "ASSISTANT-REPLY".into(),
        }]))
        .unwrap();
    let path = env.workspace_root().join("peer-history.jsonl");
    saved.save_to_path(&path).unwrap();
    let mut sess = env.spawn_with_env(
        &["--resume", path.to_str().unwrap()],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.resize(80, 100).unwrap();
    wait_for_screen(&sess, "mixed history restored", |s| {
        s.contains("ASSISTANT-REPLY")
    });
    sess.render(|screen| {
        let text = screen.raw().contents();
        let needles = [
            "❯ HUMAN-BEFORE",
            "Message from peer-one",
            "│ FIRST-PEER",
            "❯ HUMAN-BETWEEN",
            "Message from peer-two&three",
            "│ SECOND-PEER",
            "❯ HUMAN-AFTER",
            "ASSISTANT-REPLY",
        ];
        let offsets: Vec<_> = needles
            .iter()
            .map(|needle| {
                text.find(needle)
                    .unwrap_or_else(|| panic!("missing {needle}: {text}"))
            })
            .collect();
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            text.contains("<mailbox-message from=\"code-example\">"),
            "{text}"
        );
        assert!(
            text.contains("<mailbox-message from=\"incomplete\">"),
            "{text}"
        );
        assert!(!text.contains("Message from code-example"));
        assert!(!text.contains("Message from incomplete"));
    });
    exit(&mut sess);
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

/// One `↑` recalls ALL queued human messages at once (pop-all), joined in
/// submit order, oldest-at-top — matching Claude Code's `popAllEditable`.
#[test]
fn up_arrow_recalls_all_queued_humans_in_submit_order() {
    const FIRST: &str = "FIRST-QUEUED";
    const SECOND: &str = "SECOND-QUEUED";
    let (_env, mut sess) = queued_session("a2a-up-popall", &[FIRST, SECOND]);
    let stacked = format!("{FIRST}\n{SECOND}");
    sess.send("\x1b[A").expect("recall all queued humans");
    wait_for_screen(
        &sess,
        "one ↑ pulls back both messages in submit order, both chips cleared",
        |screen| {
            input_text(screen) == stacked && !has_chip(screen, FIRST) && !has_chip(screen, SECOND)
        },
    );
    // Caret sits at the end: a typed char appends to the last line.
    sess.send("!").expect("type at end of recalled stack");
    wait_for_screen(&sess, "caret at end of recalled block", |screen| {
        input_text(screen) == format!("{stacked}!")
    });
    // Resubmit the edited multiline block and recall it again: proves the
    // composed text round-trips through the real queue without losing a line.
    sess.send("\r").expect("resubmit stack");
    wait_for_screen(&sess, "edited stack re-queued", |screen| {
        input_text(screen).is_empty() && has_chip(screen, FIRST)
    });
    sess.send("\x1b[A").expect("recall edited stack");
    wait_for_screen(&sess, "stack survives resubmission", |screen| {
        input_text(screen) == format!("{stacked}!") && !has_chip(screen, FIRST)
    });
    exit(&mut sess);
}

/// After pop-all recall, editing the buffer then pressing `↑` does NOT pull
/// more queued items (the queue's human side is already empty) — it just moves
/// the cursor. Guards that recall took everything in one step and that a later
/// ↑ on edited text is ordinary cursor movement, not another recall.
#[test]
fn up_arrow_after_pop_all_does_not_recall_again() {
    const FIRST: &str = "FIRST-QUEUED";
    const SECOND: &str = "SECOND-QUEUED";
    let (_env, mut sess) = queued_session("a2a-up-norepeat", &[FIRST, SECOND]);
    let stacked = format!("{FIRST}\n{SECOND}");
    sess.send("\x1b[A").expect("recall all");
    wait_for_screen(&sess, "both recalled at once", |screen| {
        input_text(screen) == stacked && !has_chip(screen, FIRST) && !has_chip(screen, SECOND)
    });
    // Edit, then ↑ + type a sentinel: the buffer keeps its content (no new
    // recall appears), proving the queue was fully drained by the first ↑.
    sess.send("!").expect("edit recalled stack");
    wait_for_screen(&sess, "edit applied", |screen| {
        input_text(screen) == format!("{stacked}!")
    });
    sess.send("\x1b[A@").expect("Up then sentinel");
    wait_for_screen(&sess, "no second recall; content intact", |screen| {
        let input = input_text(screen);
        input.contains(FIRST)
            && input.contains(SECOND)
            && input.contains('!')
            && input.contains('@')
    });
    exit(&mut sess);
}
