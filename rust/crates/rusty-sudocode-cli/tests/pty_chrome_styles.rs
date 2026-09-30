//! Exercise the production REPL's style bridge through a real terminal, not
//! just formatter strings: the old Text path kept the words but lost SGR.
mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use unicode_width::UnicodeWidthStr;

// Lowest matching row: a todo/queued label can also occur in scrollback above
// the live chrome. Coordinates use terminal columns, not UTF-8 byte offsets.
fn attributes(sess: &PtySession, needle: &str) -> Option<(String, bool, bool)> {
    sess.render(|screen| {
        // contents() joins soft-wrapped rows; use physical rows for cell
        // coordinates, including after a terminal resize.
        let lines: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        lines.iter().enumerate().rev().find_map(|(row, line)| {
            let byte = line.find(needle)?;
            let col = u16::try_from(line[..byte].width()).ok()?;
            let cell = screen.raw().cell(u16::try_from(row).ok()?, col)?;
            Some((format!("{:?}", cell.fgcolor()), cell.bold(), cell.dim()))
        })
    })
}

fn expect_style(sess: &PtySession, needle: &str, expected: (&str, bool, bool)) {
    let deadline = Instant::now() + common::at_least(Duration::from_secs(30));
    loop {
        let actual = attributes(sess, needle);
        if actual.as_ref().is_some_and(|(color, bold, dim)| {
            color == expected.0 && *bold == expected.1 && *dim == expected.2
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{needle:?}: expected {expected:?}, got {actual:?}; screen:\n{}",
            sess.render(|screen| screen.raw().contents())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn settle_chrome(sess: &PtySession) {
    // A styled label (or the prompt byte itself) can arrive before the frame
    // finishes. Resizing in that window reflows the startup banner while the
    // renderer is still establishing its inline cursor origin. Wait for the
    // visible input AND a settled frame, not a fixed sleep. Do not use the
    // input-buffer parser here: after resize, terminal soft-wraps can join the
    // empty input row to the following separator, which is not typed content.
    expect_style(sess, "❯", ("Default", false, false));
    let deadline = Instant::now() + common::DEFAULT_TIMEOUT;
    let mut previous = String::new();
    let mut stable = 0;
    loop {
        let screen = sess.render(|s| s.contents());
        stable = if screen == previous { stable + 1 } else { 0 };
        if stable >= 4 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "chrome did not settle:\n{screen}"
        );
        previous = screen;
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn resize_idle_chrome(sess: &mut PtySession, rows: u16, cols: u16) {
    settle_chrome(sess);
    sess.resize(rows, cols).expect("resize settled chrome");
    settle_chrome(sess);
}

fn exit(sess: &mut PtySession) {
    sess.send("/exit").expect("type exit");
    common::expect_input_line(
        sess,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "exit must reach input",
    );
    expect_style(sess, "❯ /exit", ("Default", false, false));
    sess.send("\r").expect("submit exit");
    assert_eq!(sess.expect_eof().expect("exit cleanly"), 0);
}

#[test]
fn seeded_todo_chrome_preserves_colors_and_weights() {
    let env = TestEnv::new("chrome-todo-styles");
    let store = env.workspace_root().join("todos.json");
    std::fs::write(
        &store,
        r#"[{"content":"Finished parser","status":"completed","activeForm":"Finishing parser"},{"content":"Check styles","status":"in_progress","activeForm":"Checking styles"},{"content":"Review output","status":"pending","activeForm":"Reviewing output"}]"#,
    )
    .expect("seed three todo states");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    sess.resize(40, 100).expect("resize");
    // The PTY's vt100 model has no strikethrough field; verify it on the wire.
    sess.expect(r"\x1b\[9mFinished parser")
        .expect("completed label is crossed out");
    sess.expect("❯").expect("input ready");
    expect_style(&sess, "✓", ("Idx(10)", false, false));
    expect_style(&sess, "■", ("Idx(36)", false, false));
    expect_style(&sess, "Checking styles", ("Idx(8)", true, false));
    expect_style(&sess, "Finished parser", ("Idx(8)", false, true));
    expect_style(&sess, "Review output", ("Idx(8)", false, false));
    // Reflow must not merge differently styled spans or lose their attributes.
    resize_idle_chrome(&mut sess, 30, 68);
    expect_style(&sess, "Checking styles", ("Idx(8)", true, false));
    exit(&mut sess);
}

#[test]
fn running_card_and_queued_input_keep_distinct_styles() {
    let env = TestEnv::new("chrome-running-styles");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    sess.resize(40, 100).expect("resize");
    sess.expect("❯").expect("input ready");
    let prompt = env.prompt(
        "Run exactly this bash command, nothing else: printf 'interrupt-start'; sleep 30",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt}\r")).expect("start tool");
    expect_style(&sess, "╭─", ("Idx(214)", true, false));
    sess.send("StyleQueuedMarker").expect("type queued message");
    common::expect_input_line(
        &sess,
        "StyleQueuedMarker",
        common::DEFAULT_TIMEOUT,
        "queued draft",
    );
    sess.send("\r").expect("queue draft");
    expect_style(
        &sess,
        "↳ queued: StyleQueuedMarker",
        ("Default", false, true),
    );
    expect_style(&sess, "╭─", ("Idx(214)", true, false));
    sess.resize(30, 78).expect("resize during tool");
    expect_style(
        &sess,
        "↳ queued: StyleQueuedMarker",
        ("Default", false, true),
    );
    exit(&mut sess);
}

#[test]
fn todo_rich_text_preserves_extended_colors_without_replaying_controls() {
    let env = TestEnv::new("chrome-rich-styles");
    let store = env.workspace_root().join("todos.json");
    let labels = [
        "\x1b[1;2mBoldDimSample\x1b[0m",
        "\x1b[38;2;42;142;210mTrueColorSample\x1b[39m DefaultSample",
        "\x1b[38:2::128:64:32mColonRgbSample\x1b[0m",
        "\x1b[38:5:79mIndexedSample\x1b[0m",
        // Backgrounds aren't supported by the pinned MixedText API. Their
        // operands must not be mistaken for bold/italic/underline commands.
        "\x1b[48;2;1;3;4mNoOperandLeak\x1b[0m",
        // Neither a cursor/erase command nor an OSC payload is visible text.
        "SafePrefix\x1b[2J\x1b]0;HiddenTitle\x07SafeSuffix",
    ];
    let todos: Vec<_> = labels
        .iter()
        .map(|label| {
            serde_json::json!({
                "content": label, "activeForm": label, "status": "pending"
            })
        })
        .collect();
    std::fs::write(&store, serde_json::to_vec(&todos).unwrap()).unwrap();
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("COLORFGBG", "15;0"),
        ],
    );
    sess.resize(40, 100).expect("resize");
    // The screen model cannot represent bold and dim simultaneously; keep
    // that bridge regression on the wire even though summaries no longer dim.
    let wire = sess
        .expect("BoldDimSample")
        .expect("combined styles appear");
    let normalized = wire.replace("\r\n", "");
    assert!(
        normalized.contains("\x1b[1m\x1b[2mBoldDimSample")
            || normalized.contains("\x1b[1;2mBoldDimSample"),
        "rich text must retain bold and dim: {wire:?}"
    );
    sess.expect("❯").expect("input ready");
    expect_style(
        &sess,
        "TrueColorSample",
        ("Rgb(42, 142, 210)", false, false),
    );
    expect_style(&sess, "ColonRgbSample", ("Rgb(128, 64, 32)", false, false));
    expect_style(&sess, "IndexedSample", ("Idx(79)", false, false));
    expect_style(&sess, "DefaultSample", ("Idx(8)", false, false));
    expect_style(&sess, "NoOperandLeak", ("Idx(8)", false, false));
    expect_style(&sess, "SafePrefixSafeSuffix", ("Idx(8)", false, false));
    assert!(!sess
        .render(|screen| screen.raw().contents())
        .contains("HiddenTitle"));
    exit(&mut sess);
}

#[test]
fn todo_summary_scopes_every_count_and_label_in_both_themes() {
    for (background, muted) in [("15;0", "Idx(243)"), ("0;15", "Idx(245)")] {
        for statuses in [
            vec!["completed"],
            vec!["completed", "in_progress", "pending"],
        ] {
            let env = TestEnv::new("chrome-summary-spans");
            let store = env.workspace_root().join("todos.json");
            let todos: Vec<_> = statuses.iter().enumerate().map(|(i, status)| {
                serde_json::json!({"content": format!("Task {i}"), "activeForm": format!("Working {i}"), "status": status})
            }).collect();
            std::fs::write(&store, serde_json::to_vec(&todos).unwrap()).unwrap();
            let mut sess = env.spawn_with_env(
                &["--permission-mode", "read-only"],
                &[
                    ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                    ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
                    ("NO_COLOR", ""),
                    ("TERM", "xterm-256color"),
                    ("COLORFGBG", background),
                ],
            );
            sess.resize(40, 100).unwrap();
            sess.expect("❯").expect("input ready");
            for width in [100, 60] {
                resize_idle_chrome(&mut sess, 40, width);
                for label in [" todos (", " done, ", " open)"] {
                    expect_style(&sess, label, (muted, false, false));
                }
                for count in [
                    format!("{} todos", statuses.len()),
                    "1 done".into(),
                    format!("{} open", statuses.len() - 1),
                ] {
                    expect_style(&sess, &count, (muted, true, false));
                }
                if statuses.len() > 1 {
                    expect_style(&sess, "1 in progress", (muted, true, false));
                    expect_style(&sess, " in progress, ", (muted, false, false));
                }
            }
            exit(&mut sess);
        }
    }
}

#[test]
fn resumed_status_uses_muted_without_dim_and_scopes_cache_colors() {
    use runtime::{ContentBlock, ConversationMessage, Session, TokenUsage};

    // No API needed: resume real persisted usage through the production REPL.
    // Exercise both palettes and the warning/error -> muted transition as well
    // as the healthy-cache -> muted transition.
    for (background, muted, hit_color, error_color) in [
        ("15;0", "Idx(243)", "Idx(79)", "Idx(9)"),
        ("0;15", "Idx(245)", "Idx(30)", "Idx(1)"),
    ] {
        for (read, creation, hit, write) in
            [(7500, 2500, "⚡75%", "✎25%"), (9000, 1000, "⚡90%", "✎10%")]
        {
            let env = TestEnv::new("chrome-status-spans");
            assert!(std::process::Command::new("git")
                .args(["init", "-b", "chrome-style-branch"])
                .current_dir(env.workspace_root())
                .output()
                .unwrap()
                .status
                .success());
            let mut session = Session::new().with_workspace_root(env.workspace_root());
            session
                .push_user_text("Saved conversation".to_string())
                .unwrap();
            let mut message = ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Saved answer".into(),
            }]);
            message.usage = Some(TokenUsage {
                cache_read_input_tokens: read,
                cache_creation_input_tokens: creation,
                output_tokens: 100,
                ..TokenUsage::default()
            });
            message.duration_ms = Some(74_000);
            session.push_message(message).unwrap();
            let path = env.workspace_root().join("styled-session.jsonl");
            session.save_to_path(&path).unwrap();
            let mut sess = env.spawn_with_env(
                &[
                    "--resume",
                    path.to_str().unwrap(),
                    "--permission-mode",
                    "read-only",
                ],
                &[
                    ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
                    ("NO_COLOR", ""),
                    ("TERM", "xterm-256color"),
                    ("COLORFGBG", background),
                ],
            );
            sess.resize(40, 240).unwrap();
            for width in [240, 100] {
                resize_idle_chrome(&mut sess, 40, width);
                for label in ["turn 1", "ctx ", "chrome-style-branch"] {
                    expect_style(&sess, label, (muted, false, false));
                }
                let healthy_color = if background == "15;0" {
                    "Idx(10)"
                } else {
                    "Idx(2)"
                };
                expect_style(
                    &sess,
                    hit,
                    (
                        if read == 7500 {
                            hit_color
                        } else {
                            healthy_color
                        },
                        false,
                        false,
                    ),
                );
                expect_style(
                    &sess,
                    write,
                    (
                        if creation == 2500 { error_color } else { muted },
                        false,
                        false,
                    ),
                );
            }
            exit(&mut sess);
        }
    }
}
