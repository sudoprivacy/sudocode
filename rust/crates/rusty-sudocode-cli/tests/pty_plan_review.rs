//! Exercise the full question seam and bounded review viewport through a PTY.
//! CI uses a deterministic 64-line `write_plan` tool call; the same workflow can
//! run with `SCODE_TEST_BACKEND=live` (no mock-only renderer entry point).
mod common;

use common::TestEnv;

#[test]
fn long_plan_is_fully_reviewable_with_visible_controls() {
    let env = TestEnv::new("plan-review");
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    common::expect_screen_settled(&sess, |s| s.contains('❯'), env.timeout(), "input");
    let plan = (0..64)
        .map(|i| format!("- **ReviewStep{i:02}**: inspect the complete plan."))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = env.prompt(
        &format!("I am testing this CLI's plan-review UI with a supplied document. Please use write_plan to save the following Markdown test plan verbatim as its content, preserving all 64 labelled bullets. Supply brief context, constraints and acceptance describing this rendering test. Do not execute the plan or change repository code; wait for my review. This document is test data for paging and resize:\n{plan}"),
        "write_plan_review",
    );
    // Bracketed paste keeps the live prompt's literal newlines out of the
    // Enter-to-submit path; the engine receives the full expanded content.
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| !common::input_line_of(s).is_empty(),
        env.timeout(),
        "prompt ready",
    );
    sess.send("\r").unwrap();
    let screen = common::expect_screen_settled(
        &sess,
        |s| s.contains("Context") && s.contains("PgUp/PgDn"),
        env.timeout(),
        "bounded plan review",
    );
    read_every_page(&mut sess, screen, env.timeout());
    // Paging does not submit or ask the engine to regenerate the plan.
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 1);
    }

    let end_page = review_range(&sess.render(|s| s.contents())).expect("review pagination");
    sess.send("\x1b[5~").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| review_range(s).is_some_and(|(start, _, _)| start < end_page.0),
        env.timeout(),
        "previous review page",
    );
    sess.send("\x1b[1;5H").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Context") && review_range(s).is_some_and(|(start, _, _)| start == 1),
        env.timeout(),
        "review start",
    );
    sess.resize(18, 100).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| {
            s.contains("Context")
                && s.contains("PgUp/PgDn")
                && s.contains("Exit plan (don't execute)")
        },
        env.timeout(),
        "short review viewport",
    );
    sess.send("\x1b[1;5F").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| {
            s.contains("ReviewStep63:")
                && review_range(s).is_some_and(|(_, end, total)| end == total)
        },
        env.timeout(),
        "review end",
    );
    sess.resize(40, 100).unwrap();
    common::expect_screen_settled(
        &sess,
        |s| {
            s.contains("ReviewStep63:")
                && s.contains("Exit plan (don't execute)")
                && review_range(s).is_some_and(|(_, end, total)| end == total)
        },
        env.timeout(),
        "grown review viewport",
    );

    // Existing selection keys still belong to the picker, not the review body.
    sess.send("\x1b[B\x1b[B\x1b[B\x1b[BReviewedWholePlan")
        .unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("> [+] ReviewedWholePlan"),
        env.timeout(),
        "review feedback",
    );
    sess.send("\r").unwrap();
    if env.is_mock() {
        sess.expect("write_plan roundtrip complete").unwrap();
        assert!(
            env.captured_message_bodies()
                .iter()
                .any(|body| body.contains("ReviewedWholePlan")),
            "full feedback crosses the engine seam"
        );
        sess.send("/exit\r").unwrap();
        assert_eq!(sess.expect_eof().unwrap(), 0);
    } else {
        // A real model may re-open plan review in response to the feedback.
        // Cancellation is the normal user path; don't interpret it as a skip.
        sess.send_ctrl('c').unwrap();
        sess.send_ctrl('c').unwrap();
        sess.expect_eof().unwrap();
    }
}

fn read_every_page(
    sess: &mut pty_expect::PtySession,
    mut screen: String,
    timeout: std::time::Duration,
) {
    let initial = screen.clone();
    let mut seen = [false; 64];
    for _ in 0..64 {
        assert!(
            screen.contains("Exit plan (don't execute)"),
            "choices remain visible:\n{screen}"
        );
        assert!(
            screen.contains("[+]"),
            "custom input remains visible:\n{screen}"
        );
        for (i, reached) in seen.iter_mut().enumerate() {
            *reached |= screen.contains(&format!("ReviewStep{i:02}:"));
        }
        let (start, end, total) = review_range(&screen).expect("visible review pagination");
        if end == total {
            break;
        }
        sess.send("\x1b[6~").unwrap();
        screen = common::expect_screen_settled(
            sess,
            |s| review_range(s).is_some_and(|(next_start, _, _)| next_start > start),
            timeout,
            "next review page",
        );
    }
    let missing: Vec<_> = seen
        .iter()
        .enumerate()
        .filter_map(|(i, reached)| (!reached).then_some(i))
        .collect();
    assert!(missing.is_empty(), "every plan line must be reachable; missing {missing:?}\ninitial:\n{initial}\nlast:\n{screen}");
}

// The last labelled bullet can also be named in the model's introductory text.
// Only the renderer's row range establishes that we reached the end of the body.
fn review_range(screen: &str) -> Option<(usize, usize, usize)> {
    // vt100 can join a full-width body row to the next physical row. Anchor on
    // the actual paging control, not on a newline before its hint.
    let before_controls = screen.rsplit_once(" · PgUp/PgDn")?.0;
    let range = before_controls.rsplit_once("Review ")?.1;
    let (visible, total) = range.split_once('/')?;
    let (start, end) = visible.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (start >= 1 && start <= end && end <= total).then_some((start, end, total))
}
