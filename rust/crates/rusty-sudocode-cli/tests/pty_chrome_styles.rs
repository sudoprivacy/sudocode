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

fn exit(sess: &mut PtySession) {
    sess.send("/exit").expect("type exit");
    common::expect_input_line(
        sess,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "exit must reach input",
    );
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
    sess.expect("❯").expect("input ready");
    expect_style(&sess, "✓", ("Idx(10)", false, false));
    expect_style(&sess, "■", ("Idx(36)", false, false));
    expect_style(&sess, "Checking styles", ("Idx(8)", true, false));
    expect_style(&sess, "Finished parser", ("Idx(8)", false, true));
    expect_style(&sess, "Review output", ("Idx(8)", false, false));
    // Reflow must not merge differently styled spans or lose their attributes.
    sess.resize(30, 68).expect("resize smaller");
    expect_style(&sess, "Checking styles", ("Idx(8)", true, false));
    sess.send("/exit").expect("type exit");
    common::expect_input_line(
        &sess,
        "/exit",
        common::DEFAULT_TIMEOUT,
        "draft remains editable",
    );
    expect_style(&sess, "❯ /exit", ("Default", false, false));
    sess.send("\r").expect("submit exit");
    assert_eq!(sess.expect_eof().expect("exit cleanly"), 0);
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
