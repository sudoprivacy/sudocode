//! Live and resumed transcript spacing through a real terminal and real tools.
mod common;

use common::TestEnv;
use pty_expect::PtySession;

fn start(env: &TestEnv, resume: bool, queue: bool, width: u16) -> PtySession {
    let mut args = vec!["--permission-mode", "danger-full-access"];
    if resume {
        args.push("--resume");
    }
    let mut sess = env.spawn_with_env(
        &args,
        &[
            (
                "SUDOCODE_INTERRUPT_QUEUE_MODE",
                if queue { "queue" } else { "off" },
            ),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(120, width).unwrap();
    common::expect_screen(
        &sess,
        |screen| screen.contains("❯"),
        common::at_least(std::time::Duration::from_secs(30)),
        if resume {
            "resumed prompt"
        } else {
            "initial prompt"
        },
    );
    sess
}

fn finish(sess: &mut PtySession) {
    sess.send("/exit").unwrap();
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
}

fn rows(sess: &PtySession) -> Vec<String> {
    sess.render(|screen| screen.raw().rows(0, screen.raw().size().1).collect())
}

fn position(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .rposition(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("missing {needle}: {rows:#?}"))
}

fn assert_gap(rows: &[String], before: usize, after: usize, blank_rows: usize) {
    assert_eq!(after - before - 1, blank_rows, "wrong gap: {rows:#?}");
    assert!(
        rows[before + 1..after]
            .iter()
            .all(|row| row.trim().is_empty()),
        "gap has content: {rows:#?}"
    );
}

fn assert_transcript(sess: &PtySession) {
    let rows = rows(sess);
    let intro = position(&rows, "Spacing intro.");
    let end = position(&rows, "Spacing end.");
    let headers: Vec<_> = (intro + 1..end)
        .filter(|&row| rows[row].starts_with("╭─ Bash("))
        .collect();
    let caps: Vec<_> = (intro + 1..end)
        .filter(|&row| rows[row].trim() == "╰─")
        .collect();
    assert_eq!(
        headers.len(),
        2,
        "two real, separately framed calls: {rows:#?}"
    );
    assert_eq!(caps.len(), 2, "one completed frame per call: {rows:#?}");
    for (before, header) in [(intro, headers[0]), (caps[0], headers[1])] {
        // Live progress is transient and is absent from replay. Each visible
        // block still has one blank row around it.
        let progress: Vec<_> = (before + 1..header)
            .filter(|&row| rows[row].contains('⟳'))
            .collect();
        if let (Some(&first), Some(&last)) = (progress.first(), progress.last()) {
            assert_gap(&rows, before, first, 1);
            assert_gap(&rows, last, header, 1);
        } else {
            assert_gap(&rows, before, header, 1);
        }
    }
    assert_gap(&rows, caps[1], position(&rows, "Spacing done."), 1);
    for marker in ["ONE", "TWO"] {
        let first = position(&rows, &format!("{marker}_START"));
        let last = position(&rows, &format!("{marker}_END"));
        assert_eq!(last - first, 3, "log blank lines are content: {rows:#?}");
        assert!(rows[first + 1..last].iter().all(|row| row.trim() == "│"));
        let header = headers
            .iter()
            .copied()
            .find(|&row| row < first && first < row + 3)
            .unwrap();
        assert_eq!(first, header + 1, "no padding between tool title and body");
    }
    assert_gap(
        &rows,
        position(&rows, "CODE_START"),
        position(&rows, "CODE_END"),
        2,
    );
}

fn roundtrip(queue: bool, width: u16) {
    let env = if queue {
        TestEnv::new("block-spacing")
    } else {
        TestEnv::new_mock("block-spacing-sync")
    };
    for (file, body) in [
        ("spacing-one.txt", "ONE_START\n\n\nONE_END\n"),
        ("spacing-two.txt", "TWO_START\n\n\nTWO_END\n"),
    ] {
        std::fs::write(env.workspace_root().join(file), body).unwrap();
    }
    let config = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings.json"), r#"{"thinking":false}"#).unwrap();
    let mut sess = start(&env, false, queue, width);
    let prompt = env.prompt(&format!("First say exactly Spacing intro. Then run two separate Bash tool calls: `cat spacing-one.txt` and `cat spacing-two.txt`. Do not combine commands and do not repeat the file contents in your answer. After both tools finish reply with exactly this Markdown, without an outer code fence:\n{}", mock_anthropic_service::SPACING_FINAL), "transcript_spacing");
    // The sync editor also intercepts bracketed paste for OS clipboard images.
    // A short mock marker avoids reading the developer's clipboard there.
    if queue {
        sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
        common::expect_screen(
            &sess,
            |s| s.contains("Pasted") || s.contains("PARITY_SCENARIO:") || s.contains("First say"),
            common::DEFAULT_TIMEOUT,
            "prompt pasted",
        );
    } else {
        assert!(
            env.is_mock(),
            "the sync case uses a deterministic short input"
        );
        sess.send("PARITY_SCENARIO:transcript_spacing").unwrap();
        common::expect_input_line(
            &sess,
            "PARITY_SCENARIO:transcript_spacing",
            common::DEFAULT_TIMEOUT,
            "sync input",
        );
    }
    sess.send("\r").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Spacing end.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "completed transcript",
    );
    assert_transcript(&sess);
    finish(&mut sess);
    let mut resumed = start(&env, true, queue, width);
    common::expect_screen_settled(
        &resumed,
        |s| s.contains("Spacing end."),
        common::DEFAULT_TIMEOUT,
        "replayed transcript",
    );
    assert_transcript(&resumed);
    finish(&mut resumed);
}

#[test]
fn live_and_replay_share_block_spacing() {
    roundtrip(true, 100);
}

#[test]
fn narrow_live_and_replay_keep_spacing_and_content() {
    roundtrip(true, 52);
}

#[test]
fn sync_renderer_uses_the_same_spacing() {
    roundtrip(false, 100);
}

fn assert_reasoning(sess: &PtySession) {
    let rows = rows(sess);
    let one = position(&rows, "Reasoning step one.");
    let two = position(&rows, "Reasoning step two continues the same line.");
    assert_gap(&rows, one, two, 0);
    assert_gap(
        &rows,
        two,
        position(&rows, "The answer follows the reasoning."),
        1,
    );
}

#[test]
fn split_reasoning_stays_contiguous_before_separated_answer() {
    let env = TestEnv::new_mock("reasoning-spacing");
    let mut sess = start(&env, false, true, 100);
    sess.send("PARITY_SCENARIO:thinking_then_text").unwrap();
    common::expect_input_line(
        &sess,
        "PARITY_SCENARIO:thinking_then_text",
        common::DEFAULT_TIMEOUT,
        "thinking prompt",
    );
    sess.send("\r").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("The answer follows the reasoning.") && s.contains("ctx "),
        common::DEFAULT_TIMEOUT,
        "completed thinking",
    );
    assert_reasoning(&sess);
    finish(&mut sess);
    let mut resumed = start(&env, true, true, 100);
    common::expect_screen_settled(
        &resumed,
        |s| s.contains("The answer follows the reasoning."),
        common::DEFAULT_TIMEOUT,
        "replayed thinking",
    );
    assert_reasoning(&resumed);
    finish(&mut resumed);
}
