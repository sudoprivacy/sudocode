//! Exercise saved-plan integrity, bounded controls and feedback through a PTY.
//! Native document traversal is covered by the pinned `pty_review_scrollback` host.
//! CI uses a deterministic 64-line `write_plan` tool call; the same workflow can
//! run with `SCODE_TEST_BACKEND=live` (no mock-only renderer entry point).
mod common;

use common::TestEnv;

#[test]
fn long_plan_is_fully_reviewable_with_visible_controls() {
    let env = TestEnv::new("plan-review");
    let plan_path = env.workspace_root().join("full-reviewed-plan.md");
    let plan_path_text = plan_path.to_string_lossy().into_owned();
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan",
        ],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("SUDOCODE_PLAN_FILE", plan_path_text.as_str()),
        ],
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
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Ctrl+O view full text") && s.contains("Exit plan (don't execute)"),
        env.timeout(),
        "history-backed plan review",
    );
    let frozen_plan = std::fs::read_to_string(&plan_path).unwrap();
    for i in 0..64 {
        assert!(
            frozen_plan.contains(&format!("ReviewStep{i:02}")),
            "complete saved plan"
        );
    }
    for rows in [18, 40] {
        sess.resize(rows, 100).unwrap();
        common::expect_screen_settled(
            &sess,
            |s| s.contains("Ctrl+O view full text") && s.contains("Exit plan (don't execute)"),
            env.timeout(),
            "review controls survive resize",
        );
        assert_eq!(std::fs::read_to_string(&plan_path).unwrap(), frozen_plan);
    }
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            1,
            "review never regenerates the plan"
        );
    }

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
        common::expect_input_line_cleared(&sess, env.timeout(), "feedback returns input");
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
