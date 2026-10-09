//! Live and resumed transcript spacing through a real terminal and real tools.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use runtime::{ContentBlock, SessionStore};

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

fn assistant_position(rows: &[String], text: &str) -> usize {
    rows.iter()
        .rposition(|row| {
            let row = row.trim();
            row.strip_prefix("• ").unwrap_or(row) == text
        })
        .unwrap_or_else(|| panic!("missing assistant paragraph {text}: {rows:#?}"))
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
    let intro = assistant_position(&rows, "Spacing intro.");
    let end = assistant_position(&rows, "Spacing end.");
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
    assert_gap(
        &rows,
        caps[1],
        assistant_position(&rows, "Spacing done."),
        1,
    );
    for marker in ["ONE", "TWO"] {
        let first = position(&rows, &format!("{marker}_START"));
        let last = position(&rows, &format!("{marker}_END"));
        assert_eq!(last - first, 3, "log blank lines are content: {rows:#?}");
        assert!(rows[first + 1..last].iter().all(|row| row.trim() == "│"));
        let header = headers
            .iter()
            .enumerate()
            .find_map(|(index, &header)| (header < first && last < caps[index]).then_some(header))
            .unwrap_or_else(|| {
                panic!("file contents must stay inside their own tool frame: {rows:#?}")
            });
        let preamble = &rows[header + 1..first];
        if !preamble.is_empty() {
            // A model-supplied description can truncate even a short command's
            // title on a narrow terminal. Its command and separator are body
            // content; neither may introduce padding before the file output.
            assert_eq!(preamble.len(), 2, "unexpected tool body rows: {rows:#?}");
            assert_eq!(
                preamble[0].trim(),
                format!("│ $ cat spacing-{}.txt", marker.to_lowercase()),
                "a truncated title must reveal the actual command: {rows:#?}"
            );
            let rule = preamble[1]
                .trim()
                .strip_prefix("│ ")
                .unwrap_or_else(|| panic!("separator must stay inside its frame: {rows:#?}"));
            assert!(
                !rule.is_empty() && rule.chars().all(|ch| ch == '─'),
                "command/output separator must have no padding: {rows:#?}"
            );
        }
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
        TestEnv::new("block-spacing-sync")
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
    let prompt = env.prompt(&format!("Read spacing-one.txt and spacing-two.txt now using two separate Bash tool calls: `cat spacing-one.txt` and `cat spacing-two.txt`. Give them the descriptions `Read first spacing file` and `Read second spacing file`, respectively. Immediately before calling the tools, emit the exact plain standalone paragraph `Spacing intro.` including its final period, then continue with the tool calls in the same turn. Do not end the turn after that paragraph. Do not combine commands and do not repeat the file contents in your answer. After both tools finish reply with exactly this Markdown, without an outer code fence:\n{}", mock_anthropic_service::SPACING_FINAL), "transcript_spacing");
    // The sync editor also intercepts bracketed paste for OS clipboard images.
    // Type a short line there; live runs read the full instructions from a file.
    if queue {
        sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
        common::expect_screen(
            &sess,
            |s| {
                s.contains("Pasted") || s.contains("PARITY_SCENARIO:") || s.contains("Read spacing")
            },
            common::DEFAULT_TIMEOUT,
            "prompt pasted",
        );
    } else {
        let instructions = env.workspace_root().join("spacing-instructions.md");
        std::fs::write(&instructions, &prompt).unwrap();
        let input = if env.is_mock() {
            "PARITY_SCENARIO:transcript_spacing".to_string()
        } else {
            let path = instructions.to_string_lossy().replace('\\', "/");
            format!("Read the local file at {path:?} with read_file and follow its instructions.")
        };
        // Anchor the file instead of letting the provider guess a workspace.
        // Keep the input assertion on one row, then restore the tested width
        // before checking the live transcript and its replay.
        let input_width = u16::try_from(input.chars().count() + 8).unwrap().max(width);
        sess.resize(120, input_width).unwrap();
        sess.send(&input).unwrap();
        common::expect_input_line(&sess, &input, common::DEFAULT_TIMEOUT, "sync input");
    }
    let marker = common::turn_status_marker(&sess);
    sess.send("\r").unwrap();
    if !queue {
        sess.resize(120, width).unwrap();
    }
    common::expect_turn_complete_after(
        &sess,
        &marker,
        common::LIVE_TURN_BUDGET,
        "completed spacing turn",
    );
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Spacing end."),
        common::DEFAULT_TIMEOUT,
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

    // The final frame can look correct after the model first reads an unrelated
    // workspace and recovers. This fixture only asks for successful file reads
    // and two successful commands; retain earlier tool failures in acceptance.
    let store = SessionStore::from_cwd(env.workspace_root()).unwrap();
    let saved = store.load_session("latest").unwrap().session;
    let results: Vec<_> = saved
        .messages
        .iter()
        .flat_map(|message| &message.blocks)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .collect();
    assert!(results.len() >= 2, "both real command results must persist");
    let failures: Vec<_> = results
        .into_iter()
        .filter(|block| matches!(block, ContentBlock::ToolResult { is_error: true, .. }))
        .collect();
    assert!(
        failures.is_empty(),
        "spacing workflow recovered after unexpected tool errors: {failures:#?}"
    );
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
    let marker = common::turn_status_marker(&sess);
    sess.send("\r").unwrap();
    common::expect_turn_complete_after(
        &sess,
        &marker,
        common::DEFAULT_TIMEOUT,
        "completed thinking turn",
    );
    common::expect_screen_settled(
        &sess,
        |s| s.contains("The answer follows the reasoning."),
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
