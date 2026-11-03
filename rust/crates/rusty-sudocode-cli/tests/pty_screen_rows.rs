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
    let rows = common::expect_screen_rows_settled(
        &session,
        |view| {
            view.lines().any(|line| line == HEADER)
                && view.lines().any(|line| line == "> [1] First")
                && view.lines().any(|line| line == "  [2] Second")
                && view.contains("RowsReady")
        },
        common::DEFAULT_TIMEOUT,
        "physical question rows after a full-width rule",
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
    assert_eq!(session.expect_eof().unwrap(), 0);
}
