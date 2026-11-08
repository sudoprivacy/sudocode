//! A conversation looks the same after a restart.
//!
//! Live turns and a restored transcript are drawn by the same renderer from
//! the same saved record. This drives two turns on a real terminal (a prompt
//! with a paste, reasoning, an answer, a tool card), restarts with `--resume`,
//! and compares the transcript rows (text, dim and bold per cell) before and
//! after.
//!
//! Only the transcript is compared. The chrome under it is excluded by name:
//! the status line (live only; it reports the turn that just ran), the todo
//! panel, and the input box with its footer.

mod common;

use common::TestEnv;
use pty_expect::PtySession;

const ROWS: u16 = 120;
/// Wide enough that the status line is one row.
const COLUMNS: u16 = 160;

/// The first row of chrome under the transcript.
fn is_chrome(text: &str) -> bool {
    let is_status = text.starts_with('[') && text.contains("] · turn ");
    let is_todo_panel = text
        .split_once(" todos (")
        .is_some_and(|(count, _)| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()));
    is_status || is_todo_panel || text.starts_with("────")
}

/// One rendered row: its text and, per cell, whether it is dim and bold.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    text: String,
    dim: Vec<bool>,
    bold: Vec<bool>,
}

/// The transcript rows on screen: from the first prompt echo to the chrome.
fn transcript(sess: &PtySession, first_prompt: &str) -> Vec<Row> {
    sess.render(|screen| {
        let raw = screen.raw();
        let (rows, columns) = raw.size();
        let texts: Vec<String> = raw
            .rows(0, columns)
            .map(|row| row.trim_end().to_string())
            .collect();
        let all: Vec<Row> = (0..rows)
            .zip(texts)
            .map(|(row, text)| {
                let width = u16::try_from(text.chars().count()).unwrap_or(columns);
                let (dim, bold) = (0..width)
                    .map(|col| {
                        raw.cell(row, col)
                            .map_or((false, false), |cell| (cell.dim(), cell.bold()))
                    })
                    .unzip();
                Row { text, dim, bold }
            })
            .collect();
        let start = all
            .iter()
            .position(|row| row.text.starts_with(&format!("❯ {first_prompt}")))
            .unwrap_or_else(|| panic!("first prompt echo on screen:\n{}", dump(&all)));
        let mut end = start
            + all[start..]
                .iter()
                .position(|row| is_chrome(&row.text))
                .expect("chrome under the transcript");
        while end > start && all[end - 1].text.is_empty() {
            end -= 1;
        }
        all.into_iter().take(end).skip(start).collect()
    })
}

fn dump(rows: &[Row]) -> String {
    rows.iter()
        .map(|row| {
            let marks: String = row
                .dim
                .iter()
                .zip(&row.bold)
                .map(|(dim, bold)| match (dim, bold) {
                    (true, true) => '*',
                    (true, false) => 'd',
                    (false, true) => 'b',
                    (false, false) => '.',
                })
                .collect();
            format!("{:<160}|{marks}", row.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for(sess: &PtySession, needle: &str) {
    common::expect_screen(
        sess,
        |screen| screen.contains(needle),
        common::DEFAULT_TIMEOUT,
        needle,
    );
}

#[test]
fn a_conversation_looks_the_same_after_a_restart() {
    let env = TestEnv::new("restart-parity");
    if !env.is_mock() {
        return;
    }
    let first = env.prompt("Read this paste:", "thinking_then_text");
    let mut sess = env.spawn_with_env_at_size(
        &["--permission-mode", "workspace-write"],
        &[],
        ROWS,
        COLUMNS,
    );
    sess.expect("❯").expect("input ready");
    sess.send(&format!("{first} ")).unwrap();
    sess.send("\u{1b}[200~alpha line\nbeta line\ngamma line\u{1b}[201~")
        .unwrap();
    wait_for(&sess, "[Pasted text #1");
    sess.send(" thanks\r").unwrap();
    wait_for(&sess, "gamma line");
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "first turn done");

    let second = env.prompt("Make a todo list.", "todo_write_roundtrip");
    sess.send(&format!("{second}\r")).unwrap();
    wait_for(&sess, "todo_write roundtrip complete");
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "second turn done");
    let live = transcript(&sess, "Read this paste:");
    sess.send("/exit\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);

    let mut resumed = env.spawn_with_env_at_size(
        &["--resume", "latest", "--permission-mode", "workspace-write"],
        &[],
        ROWS,
        COLUMNS,
    );
    wait_for(&resumed, "todo_write roundtrip complete");
    common::expect_input_line_cleared(&resumed, common::DEFAULT_TIMEOUT, "replay drawn");
    let replayed = transcript(&resumed, "Read this paste:");
    resumed.send("/exit\r").unwrap();
    assert_eq!(resumed.expect_eof().unwrap(), 0);

    assert!(
        live.iter()
            .any(|row| row.dim.contains(&true) && row.text.contains("beta line")),
        "the paste is dimmed live:\n{}",
        dump(&live)
    );
    assert!(
        live == replayed,
        "the transcript changed across a restart\n=== live\n{}\n=== replayed\n{}",
        dump(&live),
        dump(&replayed)
    );
}
