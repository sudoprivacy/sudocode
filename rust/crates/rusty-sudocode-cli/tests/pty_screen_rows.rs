//! Real terminal wrapping must not erase row boundaries used by UI waits.
mod common;

use pty_expect::PtySession;

#[test]
fn row_wait_rejects_echo_and_keeps_a_question_after_a_full_width_rule() {
    const WIDTH: u16 = 40;
    const HEADER: &str = "1/1  Physical question";
    let script = r"
import sys
width = int(sys.argv[1])
sys.stdin.readline()
sys.stdout.write('\x1b[2J\x1b[HEcho 1/1  Physical question [2] Second\r\nEchoReady\r\n')
sys.stdout.flush()
sys.stdin.readline()
sys.stdout.write('\x1b[2J\x1b[H' + '-' * width + '1/1  Physical question\r\n> [1] First\r\n  [2] Second\r\nRowsReady\r\n')
sys.stdout.flush()
sys.stdin.readline()
sys.stdout.write('\x1b[2J\x1b[H\x1b[31mRepainting\x1b[0m')
sys.stdout.flush()
sys.stdin.readline()
";
    let width = WIDTH.to_string();
    let mut session =
        PtySession::spawn(&common::resolve_python(), &["-u", "-c", script, &width]).unwrap();
    session.resize(10, WIDTH).unwrap();
    session.send("\r").unwrap();
    let echo = common::expect_screen_rows_settled(
        &session,
        |view| view.contains("EchoReady"),
        common::DEFAULT_TIMEOUT,
        "echoed prompt painted",
    );
    assert!(!echo.lines().any(|line| line == HEADER));

    session.send("\r").unwrap();
    let (rows, snapshot) = common::expect_screen_rows_settled_snapshot(
        &session,
        |view| {
            view.lines().any(|line| line == HEADER)
                && view.lines().any(|line| line == "> [1] First")
                && view.lines().any(|line| line == "  [2] Second")
                && view.contains("RowsReady")
        },
        common::DEFAULT_TIMEOUT,
        "physical question rows after a full-width rule",
        |screen| screen.raw().clone(),
    );
    let rule = "-".repeat(usize::from(WIDTH));
    assert!(rows.lines().any(|line| line == rule));
    let logical = session.render(common::screen_contents);
    assert!(logical.contains(&format!("{rule}{HEADER}")), "{logical}");
    assert!(
        !logical.lines().any(|line| line == HEADER),
        "the old logical-line guard must reject this real wrapped terminal frame"
    );
    session.send("\r").unwrap();
    let (repaint, changed) = common::expect_screen_rows_settled_snapshot(
        &session,
        |view| view.contains("Repainting") && !view.contains(HEADER),
        common::DEFAULT_TIMEOUT,
        "a later incomplete frame replaced the settled question",
        |screen| screen.raw().clone(),
    );
    assert_ne!(repaint, rows);
    assert_eq!(
        snapshot.rows(0, WIDTH).collect::<Vec<_>>().join("\n"),
        rows,
        "physical rows and styled cells belong to one retained observation"
    );
    let original = snapshot.cell(0, 0).unwrap();
    let later = changed.cell(0, 0).unwrap();
    assert_eq!(original.contents(), "-");
    assert_eq!(later.contents(), "R");
    assert_ne!(
        format!("{:?}", original.fgcolor()),
        format!("{:?}", later.fgcolor()),
        "a later SGR repaint must not alter the retained cell styles"
    );
    session.send("\r").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}
