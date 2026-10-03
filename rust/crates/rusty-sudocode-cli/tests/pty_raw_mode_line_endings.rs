//! PTY regression tests for iocraft-REPL terminal rendering: raw-mode line
//! endings and markdown block spacing.
//!
//! **Line endings.** The iocraft render loop holds the terminal in raw mode,
//! where `OPOST`/`ONLCR` are off and a bare `\n` moves the cursor down
//! **without** returning it to column 0. iocraft terminates its own canvas
//! rows correctly, and `StdoutHandle::println` terminates the *end* of the
//! message it is handed — but interior newlines pass through untouched, and
//! `StdoutHandle::print` writes its argument completely verbatim. Tool
//! results arrive as multi-line `println` messages and streaming markdown as
//! raw `print` chunks, so before `split_for_iocraft` every line after the
//! first started where the previous one ended and the output walked off the
//! right edge of the screen as a staircase.
//!
//! **Markdown spacing.** The renderer used to stack block separators (two
//! blank lines before headings), drop unordered-list markers entirely, glue
//! nested lists onto their parent's row, and misalign nested/continuation
//! lines. The `markdown_rendering_showcase` mock scenario streams a document
//! exercising each rule; the test asserts the rendered VT100 screen.
//!
//! Both tests assert on the rendered screen — "what the user actually sees" —
//! because that is the layer these bugs live at. Synchronization is on the
//! turn status line (`· turn `), which the CLI prints after all turn output has
//! been flushed through the same FIFO channel; no sleeps.
//!
//! ```bash
//! cargo test --test pty_raw_mode_line_endings                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_raw_mode_line_endings  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

/// Spawn the iocraft REPL — the only path that puts the terminal in raw
/// mode. The shared harness defaults `SUDOCODE_INTERRUPT_QUEUE_MODE` to
/// `off` (the rustyline REPL), so `queue` must be set explicitly.
fn spawn_iocraft_repl(env: &TestEnv, permission_mode: &str) -> pty_expect::PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", permission_mode],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    // Generous timeout for CI VMs where PTY output can be slow.
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    // A tall, wide screen keeps the turn's output from scrolling away and
    // gives a runaway staircase room to be unmistakable.
    sess.resize(50, 100).expect("resize pty");
    sess.expect("❯").expect("initial prompt");
    sess
}

/// Leading-space counts of every rendered row containing `needle`.
fn indents_of_rows_containing(sess: &pty_expect::PtySession, needle: &str) -> Vec<usize> {
    sess.render(|screen| {
        screen
            .contents()
            .lines()
            .filter(|line| line.contains(needle))
            .map(|line| line.len() - line.trim_start().len())
            .collect()
    })
}

/// The rendered screen as trimmed rows.
fn screen_rows(sess: &pty_expect::PtySession) -> Vec<String> {
    sess.render(|screen| {
        screen
            .contents()
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect()
    })
}

/// One bash turn through the raw-mode REPL, asserting all line-ending
/// properties of the same final screen:
///
/// 1. the tool-call box's interior newlines are CRLF on the raw stream;
/// 2. the tool-result body's interior newline is CRLF on the raw stream;
/// 3. neither stair-steps on the rendered screen;
/// 4. iocraft's own full-width separator rules stay at column 0 (the fix
///    must not add carriage returns that shift iocraft's canvas).
///
/// Anchors are chosen to be absent from the echoed prompt: the prompt
/// contains `printf 'alpha from bash'`, so `alpha from bash` would match the
/// echo — `╭─`, `│`, and `· turn ` cannot.
#[test]
fn bash_turn_uses_crlf_and_does_not_staircase() {
    let env = TestEnv::new("raw-lf-bash");
    let mut sess = spawn_iocraft_repl(&env, "danger-full-access");

    let prompt = env.prompt(
        "Run this bash command: printf 'alpha from bash'",
        "bash_stdout_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    // Raw-stream assertions double as sync points: with a bare LF (the bug)
    // `[^\n]*` cannot reach a `\r\n` and the expect times out.
    //
    // Both the command header and the result render as the same L-frame card:
    // a header line opening with `╭─` and body lines with `│`, each written
    // through `split_for_iocraft` so every line must be CRLF-terminated. The
    // frame glyph is followed by an ANSI reset before the space, so the anchors
    // intentionally do not require a literal following space.
    sess.expect("╭─[^\n]*\r\n")
        .expect("tool card header line should end with CRLF, not a bare LF");
    sess.expect("│[^\n]*\r\n")
        .expect("tool card body line should end with CRLF, not a bare LF");
    // The status line and the separator are CHROME — drawn by the renderer, not
    // appended to a log — so they are waited for on the SCREEN. Asking the byte
    // stream for them asks about redraw timing instead: matching the status line
    // consumes the cursor past it, and the separator is then only still in the
    // stream if something happened to trigger another redraw. On CI nothing did,
    // and this timed out at 30s on a separator that was on screen the whole
    // time, with every earlier assertion in this test passing.
    common::expect_screen(
        &sess,
        |screen| screen.contains("· turn "),
        Duration::from_secs(30),
        "turn status line",
    );
    common::expect_screen(
        &sess,
        |screen| screen.contains(&"─".repeat(20)),
        Duration::from_secs(30),
        "separator drawn with the status line",
    );

    let mut box_indents = indents_of_rows_containing(&sess, "╭─ ");
    box_indents.extend(indents_of_rows_containing(&sess, "│ "));
    assert!(
        !box_indents.is_empty(),
        "expected the tool card rows on screen"
    );
    for indent in &box_indents {
        assert!(
            *indent <= 4,
            "tool output row indented {indent} columns instead of ~2 — the \
             raw-mode staircase is back (all indents: {box_indents:?})"
        );
    }

    // Only iocraft's full-width separator rules: rows made up entirely of
    // `─`. (A bare "────" substring would also match the tool box's
    // `╰────────╯` border, which is legitimately indented.)
    let rule_indents: Vec<usize> = sess.render(|screen| {
        screen
            .contents()
            .lines()
            .map(str::trim_end)
            .filter(|line| {
                let t = line.trim_start();
                !t.is_empty() && t.chars().all(|c| c == '─')
            })
            .map(|line| line.len() - line.trim_start().len())
            .collect()
    });
    assert!(
        !rule_indents.is_empty(),
        "expected iocraft separator rules on screen"
    );
    for indent in &rule_indents {
        assert_eq!(
            *indent, 0,
            "iocraft separator rule drifted off column 0 (indents: {rule_indents:?})"
        );
    }

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// Index of the first row containing `needle`, or panic with the screen.
fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|r| r.contains(needle))
        .unwrap_or_else(|| panic!("row containing {needle:?} not on screen: {rows:#?}"))
}

/// Streams `MARKDOWN_SHOWCASE_DOC` (see mock-anthropic-service) and asserts
/// the block-spacing and list-marker rules on the rendered screen:
///
/// * a label is bound to the list it introduces (no injected blank line);
/// * an author-written blank line before a list survives;
/// * a heading is preceded by exactly one blank row;
/// * unordered items carry a `•` marker;
/// * a bullet nested under an ordered item aligns under the parent's text;
/// * a nested list does not open a blank hole before the next sibling.
/// * the response prefix is a text bullet, not an emoji-capable record symbol,
///   and subsequent streamed blocks retain the two-column response margin.
#[test]
fn markdown_showcase_renders_without_spacing_artifacts() {
    let env = TestEnv::new("md-showcase");

    // Mock-only by nature: every assertion below is about how the renderer
    // lays out one specific document (`MARKDOWN_SHOWCASE_DOC`) — its labels,
    // its list items, the blank rows between its blocks. A live model writes
    // whatever markdown it likes, so there is nothing to compare against.
    if env.is_live() {
        eprintln!(
            "markdown_showcase_renders_without_spacing_artifacts: \
             skipped in live mode (asserts on the mock showcase document)"
        );
        return;
    }

    let mut sess = spawn_iocraft_repl(&env, "read-only");

    let prompt = env.prompt("Show me formatted markdown", "markdown_rendering_showcase");
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.expect("second").expect("last list item");
    sess.expect("ctx ").expect("turn status line");

    let rows = screen_rows(&sess);

    // Binding: "Intro:" introduces the list, so "• alpha" is the very next
    // row — the blank line the renderer used to inject is gone.
    let intro = row_of(&rows, "Intro:");
    assert_eq!(
        rows[intro], "• Intro:",
        "response must start with a text bullet at column zero: {rows:#?}"
    );
    assert!(
        rows.iter().all(|row| !row.contains('\u{23fa}')),
        "response prefix must not use the emoji-capable record symbol: {rows:#?}"
    );
    let done = row_of(&rows, "Done.");
    assert_eq!(
        rows[done], "  Done.",
        "later streamed blocks must keep the two-column response margin: {rows:#?}"
    );
    assert!(
        rows[intro + 1].contains("• alpha"),
        "adjacent list must bind to its label: {rows:#?}"
    );
    assert!(
        rows[intro + 2].contains("• beta"),
        "list items must be adjacent: {rows:#?}"
    );

    // Heading: exactly one blank row above "Section".
    let section = row_of(&rows, "Section");
    assert!(
        rows[section - 1].trim().is_empty() && !rows[section - 2].trim().is_empty(),
        "heading must be preceded by exactly one blank row: {rows:#?}"
    );

    // Author-written blank line before a list survives.
    let after_blank = row_of(&rows, "After blank:");
    assert!(
        rows[after_blank + 1].trim().is_empty() && rows[after_blank + 2].contains("• gamma"),
        "author-written blank line before a list must survive: {rows:#?}"
    );

    // A bullet nested under an ordered item starts at the parent's content
    // column ("1. " is three columns wide).
    let parent = row_of(&rows, "1. parent");
    let child = row_of(&rows, "• child");
    let parent_indent = rows[parent].len() - rows[parent].trim_start().len();
    let child_indent = rows[child].len() - rows[child].trim_start().len();
    assert_eq!(
        child_indent,
        parent_indent + 3,
        "nested bullet must align under the ordered parent's text: {rows:#?}"
    );

    // Nested list with a following sibling: no blank hole.
    let outer = row_of(&rows, "• outer");
    assert!(
        rows[outer + 1].contains("• inner") && rows[outer + 2].contains("• second"),
        "nested list must not open a blank hole before the next sibling: {rows:#?}"
    );
    let outer_indent = rows[outer].len() - rows[outer].trim_start().len();
    let inner_indent = rows[outer + 1].len() - rows[outer + 1].trim_start().len();
    assert_eq!(
        inner_indent,
        outer_indent + 2,
        "nested bullet must be indented under its parent: {rows:#?}"
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}

/// Chrome is findable on the SCREEN after the stream cursor has passed it — and
/// is NOT findable with `expect()`.
///
/// This pins the MECHANISM behind the flake rather than its symptom. `expect()`
/// holds a forward-only cursor into the PTY byte stream, so once a match has
/// consumed past a chrome row, that row is reachable again only if something
/// triggers another redraw. Nothing in the product guarantees one, so the wait
/// is decided by redraw timing and no budget can rescue it — which is why
/// `bash_turn_uses_crlf_and_does_not_staircase` timed out at 30s on a separator
/// that was on screen throughout.
///
/// The test drains the stream deliberately to put the cursor past the chrome,
/// then shows the two waits disagree. If `expect()` ever starts succeeding here,
/// this test has stopped describing the bug and the assertion says so.
#[test]
fn chrome_is_on_the_screen_after_the_stream_cursor_has_passed_it() {
    let env = TestEnv::new("chrome-after-cursor");
    let mut sess = spawn_iocraft_repl(&env, "workspace-write");

    // The permanent footer is chrome: drawn by the renderer, never appended.
    let footer = "/exit";
    common::expect_screen(
        &sess,
        |screen| screen.contains(footer),
        common::DEFAULT_TIMEOUT,
        "the footer should be drawn at startup",
    );

    // Drain every byte emitted so far, leaving the cursor at the end.
    let _ = sess.expect(r"(?s)[\s\S]+");

    // The screen still shows it, because the screen is state, not a log.
    common::expect_screen(
        &sess,
        |screen| screen.contains(footer),
        Duration::from_secs(2),
        "the footer is still on screen after the stream was drained",
    );

    // The stream does not, within a budget far larger than it would need if
    // this were merely slow. That asymmetry is why chrome waits exist.
    assert!(
        sess.expect_within(footer, Duration::from_secs(3)).is_err(),
        "expect() found chrome after the cursor passed it, so this test no \
         longer demonstrates the bug the screen waits avoid"
    );

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}
