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
//! turn status line (`ctx `), which the CLI prints after all turn output has
//! been flushed through the same FIFO channel; no sleeps.
//!
//! ```bash
//! cargo test --test pty_raw_mode_line_endings                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_raw_mode_line_endings  # real API
//! ```

mod common;

use common::TestEnv;

/// Spawn the iocraft REPL — the only path that puts the terminal in raw
/// mode. The shared harness defaults `SUDOCODE_INTERRUPT_QUEUE_MODE` to
/// `off` (the rustyline REPL), so `queue` must be set explicitly.
fn spawn_iocraft_repl(env: &TestEnv, permission_mode: &str) -> pty_expect::PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", permission_mode],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
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
/// echo — `─╮`, `└ `, and `ctx ` cannot.
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
    sess.expect("─╮[^\n]*\r\n")
        .expect("tool-call box top line should end with CRLF, not a bare LF");
    sess.expect("└ [^\n]*\r\n")
        .expect("tool-result body line should end with CRLF, not a bare LF");
    // The status line is the last turn output through the FIFO channel —
    // once it is on the stream, the whole turn has rendered.
    sess.expect("ctx ").expect("turn status line");

    let mut box_indents = indents_of_rows_containing(&sess, "╭─ ");
    box_indents.extend(indents_of_rows_containing(&sess, "$ printf"));
    box_indents.extend(indents_of_rows_containing(&sess, "└ "));
    assert!(
        !box_indents.is_empty(),
        "expected the tool-call box and tool-result rows on screen"
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
#[test]
fn markdown_showcase_renders_without_spacing_artifacts() {
    let env = TestEnv::new("md-showcase");
    let mut sess = spawn_iocraft_repl(&env, "read-only");

    let prompt = env.prompt("Show me formatted markdown", "markdown_rendering_showcase");
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.expect("second").expect("last list item");
    sess.expect("ctx ").expect("turn status line");

    let rows = screen_rows(&sess);

    // Binding: "Intro:" introduces the list, so "• alpha" is the very next
    // row — the blank line the renderer used to inject is gone.
    let intro = row_of(&rows, "Intro:");
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
