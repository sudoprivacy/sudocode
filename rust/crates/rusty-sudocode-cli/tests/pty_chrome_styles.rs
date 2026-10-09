//! Exercise the production REPL's style bridge through a real terminal, not
//! just formatter strings: the old Text path kept the words but lost SGR.
mod common;

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use unicode_width::UnicodeWidthChar;

// Lowest matching row: a todo/queued label can also occur in scrollback above
// the live chrome. Coordinates use terminal columns, not UTF-8 byte offsets.
fn attributes(sess: &PtySession, needle: &str) -> Option<(String, bool, bool)> {
    sess.render(|screen| {
        // Search logical text across wrapping, but retain physical coordinates
        // for the cell assertion. rows() alone misses a needle split by a
        // wrap; contents() alone gives the wrong row/column after soft-wraps.
        let lines: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let mut text = String::new();
        let mut positions = Vec::new();
        for (row, line) in lines.iter().enumerate() {
            let row = u16::try_from(row).ok()?;
            let mut col = 0;
            for ch in line.chars() {
                let normalized = if ch.is_whitespace() { ' ' } else { ch };
                if normalized != ' ' || !text.ends_with(' ') {
                    positions.push((text.len(), row, col));
                    text.push(normalized);
                }
                col += u16::try_from(ch.width().unwrap_or(0)).ok()?;
            }
            // Word wrapping can consume the separating space; soft-wraps can
            // instead split a word. Preserve that distinction when searching.
            if !screen.raw().row_wrapped(row) && !text.ends_with(' ') {
                text.push(' ');
            }
        }
        let needle = needle.split_whitespace().collect::<Vec<_>>().join(" ");
        let (start, _) = text.rmatch_indices(needle.as_str()).next()?;
        let (_, row, col) = positions.iter().find(|(byte, _, _)| *byte == start)?;
        let cell = screen.raw().cell(*row, *col)?;
        Some((format!("{:?}", cell.fgcolor()), cell.bold(), cell.dim()))
    })
}

fn expect_style(sess: &PtySession, needle: &str, expected: (&str, bool, bool)) {
    let deadline = Instant::now() + common::at_least(Duration::from_secs(30));
    loop {
        let actual = attributes(sess, needle);
        if actual.as_ref().is_some_and(|(color, bold, dim)| {
            common::colors_equal(color, expected.0)
                && *bold == expected.1
                && ((cfg!(windows) && expected.2) || *dim == expected.2)
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
        let screen = sess.render(|s| s.raw().contents());
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
    // ConPTY drops strikethrough; application-side tests cover it on Windows.
    #[cfg(not(windows))]
    sess.expect(r"\x1b\[9mFinished parser")
        .expect("completed label is crossed out");
    sess.expect("❯").expect("input ready");
    expect_style(&sess, "✓", ("Idx(10)", false, false));
    expect_style(&sess, "■", ("Rgb(166, 227, 161)", false, false));
    expect_style(&sess, "Checking styles", ("Idx(8)", true, false));
    expect_style(&sess, "Finished parser", ("Idx(8)", false, true));
    expect_style(&sess, "Review output", ("Idx(8)", false, false));
    // Reflow must not merge differently styled spans or lose their attributes.
    resize_idle_chrome(&mut sess, 30, 68);
    expect_style(&sess, "Checking styles", ("Idx(8)", true, false));
    exit(&mut sess);
}

fn info_uses_shared_green(light: bool, truecolor: bool, no_color: bool) {
    let reference: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/codex_styles.json")).unwrap();
    let variant = format!(
        "{}-{}",
        if light { "light" } else { "dark" },
        if truecolor { "truecolor" } else { "indexed" }
    );
    let palette = &reference["variants"][variant];
    let index = usize::try_from(palette["roles"]["inline"].as_u64().unwrap()).unwrap();
    let green = if no_color {
        "Default"
    } else {
        palette["styles"][index]["fg"].as_str().unwrap()
    };
    let env = TestEnv::new("info-shared-green");
    let store = env.workspace_root().join("todos.json");
    std::fs::write(&store, r#"[{"content":"Check info colors","status":"in_progress","activeForm":"Checking info colors"}]"#).unwrap();
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_TODO_STORE", store.to_str().unwrap()),
            ("NO_COLOR", if no_color { "1" } else { "" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", if truecolor { "truecolor" } else { "" }),
            ("COLORFGBG", if light { "0;15" } else { "15;0" }),
        ],
    );
    sess.resize(50, 100).unwrap();
    common::expect_input_line_cleared(&sess, env.timeout(), "info input ready");
    // Both a persistent info label and the active status slot use the role.
    expect_style(&sess, "■", (green, false, false));
    let marker = common::turn_status_marker(&sess);
    let prompt = env.prompt(
        "What is 17 times 23? Work it out, then give the number.",
        "delayed_text",
    );
    sess.send(&prompt).unwrap();
    common::expect_input_line(&sess, &prompt, common::DEFAULT_TIMEOUT, "info prompt input");
    sess.send("\r").unwrap();
    common::expect_screen(
        &sess,
        |_| {
            ["Thinking...", "Reasoning..."].iter().any(|label| {
                attributes(&sess, label).is_some_and(|(color, bold, dim)| {
                    common::colors_equal(&color, green) && !bold && !dim
                })
            })
        },
        common::LIVE_TURN_BUDGET,
        "active status uses theme info green",
    );
    sess.send("info-draft").unwrap();
    common::expect_input_line(
        &sess,
        "info-draft",
        common::DEFAULT_TIMEOUT,
        "typing beside info status",
    );
    expect_style(&sess, "info-draft", ("Default", false, false));
    sess.send("\x15").unwrap();
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "clear draft");
    common::expect_turn_complete_after(
        &sess,
        &marker,
        common::LIVE_TURN_BUDGET,
        "info turn completed",
    );
    let marker = common::turn_status_marker(&sess);
    let prompt = env.prompt(
        "Bash: printf 'alpha from bash'. Then say done.",
        "bash_stdout_roundtrip",
    );
    sess.send(&prompt).unwrap();
    common::expect_input_line(&sess, &prompt, common::DEFAULT_TIMEOUT, "info prompt input");
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(&sess, &marker, common::LIVE_TURN_BUDGET, "tool completed");
    expect_style(&sess, "╰─", (green, !no_color, false));
    exit(&mut sess);
}

#[test]
fn dark_info_matches_the_shared_green() {
    info_uses_shared_green(false, true, false);
}

#[test]
fn light_info_matches_the_shared_green() {
    info_uses_shared_green(true, true, false);
}

#[test]
fn indexed_info_matches_the_shared_green() {
    info_uses_shared_green(false, false, false);
    info_uses_shared_green(true, false, false);
}

#[test]
fn no_color_info_uses_the_default_foreground() {
    info_uses_shared_green(false, true, true);
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
    #[cfg(not(windows))]
    {
        let wire = sess
            .expect("BoldDimSample")
            .expect("combined styles appear");
        let normalized = wire.replace("\r\n", "");
        assert!(
            normalized.contains("\x1b[1m\x1b[2mBoldDimSample")
                || normalized.contains("\x1b[1;2mBoldDimSample"),
            "rich text must retain bold and dim: {wire:?}"
        );
    }
    sess.expect("❯").expect("input ready");
    #[cfg(windows)]
    expect_style(&sess, "BoldDimSample", ("Idx(8)", true, true));
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
    for (background, muted) in [("15;0", "Idx(247)"), ("0;15", "Idx(241)")] {
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
        ("15;0", "Idx(247)", "Idx(220)", "Idx(9)"),
        ("0;15", "Idx(241)", "Idx(130)", "Idx(1)"),
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
                for label in [
                    "turn 1",
                    "ctx ",
                    "tokens · +1m14s Σ1m14s · ctx 10.0k",
                    "chrome-style-branch",
                ] {
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
