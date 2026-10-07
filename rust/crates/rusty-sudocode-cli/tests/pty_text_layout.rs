//! Terminal acceptance for shared column layout and structured styles.
mod common;
use common::render_measurement::{measure_keys, report_resources};

use std::time::{Duration, Instant};

use common::TestEnv;
use pty_expect::PtySession;
use unicode_width::UnicodeWidthStr;

fn start(env: &TestEnv, background: &str) -> PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", background),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(60, 40).expect("resize");
    sess.expect("❯").expect("prompt");
    sess
}

fn finish(sess: &mut PtySession) {
    sess.send("/exit").expect("exit text");
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").expect("submit exit");
    assert_eq!(sess.expect_eof().expect("exit"), 0);
}

fn showcase(background: &str, link_color: &str, code_color: &str) {
    let env = TestEnv::new("unicode-layout");
    let mut sess = start(&env, background);
    // Live mode asks for the exact document too; both paths exercise the
    // production engine, Markdown renderer, inline history and terminal.
    let prompt = env.prompt(
        &format!(
            "Reply with exactly this Markdown, without an enclosing code fence:\n{}\n\n{}\n\nLists done.",
            mock_anthropic_service::UNICODE_SHOWCASE_DOC,
            mock_anthropic_service::LIST_WRAP_DOC
        ),
        "unicode_rendering_showcase",
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .expect("paste prompt");
    // Wait for paste consumption before Enter, including its collapsed preview.
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("Reply with exactly"),
        common::DEFAULT_TIMEOUT,
        "prompt paste",
    );
    sess.send("\r").expect("submit");
    sess.set_default_timeout(if env.is_live() {
        common::LIVE_TURN_BUDGET
    } else {
        common::DEFAULT_TIMEOUT
    });
    // A live model may emit a thinking block before the requested text.
    sess.expect("•").expect("assistant response");
    sess.expect("CJK:").expect("CJK block after response start");
    sess.expect("👩🏽‍💻END")
        .expect("emoji cluster must not be split by a line break");
    common::expect_screen(
        &sess,
        |s| s.contains("Lists done.") && s.contains("ctx "),
        Duration::from_secs(90),
        "completed unicode turn",
    );
    sess.render(|screen| {
        let contents = screen.raw().contents();
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let first = rows
            .iter()
            .rposition(|r| r.contains("CJK:"))
            .expect("response start");
        let prefix = if env.is_mock() { "• " } else { "  " };
        // In live mode thinking may own the initial bullet.
        assert!(
            rows[first] == format!("{prefix}CJK:{}", "界".repeat(17))
                || (env.is_live() && rows[first] == format!("• CJK:{}", "界".repeat(17)))
        );
        // ConPTY may fill unused columns with spaces. Keep the leading
        // margin and visible continuation text exact.
        assert_eq!(
            rows[first + 1].trim_end_matches(' '),
            format!("  {}", "界".repeat(13)),
            "wide continuation must retain its margin: {contents}"
        );
        let combining = rows
            .iter()
            .rposition(|r| r.contains("Combining:"))
            .expect("combining line");
        assert!(
            rows[combining + 1].starts_with("  e\u{301}END")
                || (env.is_live() && rows[combining + 1].starts_with("  éEND")),
            "combining sequence must stay attached: {contents}"
        );
        let table: Vec<_> = rows[first..]
            .iter()
            .filter(|r| {
                r.contains('│') && (r.contains("Key") || r.contains("中文") || r.contains("ASCII"))
            })
            .collect();
        assert_eq!(table.len(), 3, "table rows: {contents}");
        let borders: Vec<Vec<usize>> = table
            .iter()
            .map(|row| {
                row.match_indices('│')
                    .map(|(index, _)| row[..index].width())
                    .collect()
            })
            .collect();
        assert!(
            borders.windows(2).all(|pair| pair[0] == pair[1]),
            "table borders must align in columns: {borders:?}"
        );
        for (label, color) in [("LINK", link_color), ("CODE", code_color)] {
            let (row, text) = rows
                .iter()
                .enumerate()
                .rfind(|(_, r)| r.contains(label))
                .expect("styled label");
            let col = text[..text.find(label).unwrap()].width();
            let cell = screen
                .raw()
                .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                .unwrap();
            assert_eq!(format!("{:?}", cell.fgcolor()), color, "{label} color");
        }
    });
    assert_list_continuations(&sess, 2);
    finish(&mut sess);
}

fn assert_list_continuations(sess: &PtySession, margin: usize) {
    sess.render(|screen| {
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        for (marker, content_col, expected) in [
            (
                "9. ListWrap: ",
                5,
                "ListWrap: ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz界界界界👩🏽‍💻END",
            ),
            (
                "• NestedWrap: ",
                7,
                "NestedWrap: ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyzEND",
            ),
            (
                "10. NextWrap: ",
                6,
                "NextWrap: ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyzEND",
            ),
        ] {
            let start = rows
                .iter()
                .rposition(|row| row.contains(marker))
                .expect("list item");
            let label = marker.split_once(' ').unwrap().1;
            let label_start = rows[start].find(label).unwrap();
            assert_eq!(rows[start][..label_start].width(), content_col + margin);
            let mut text = rows[start][label_start..].to_string();
            let padding = " ".repeat(content_col + margin);
            let mut count = 0;
            for row in &rows[start + 1..] {
                if text.ends_with("END") {
                    break;
                }
                assert!(
                    row.starts_with(&padding),
                    "continuation aligned to content: {row:?} in {rows:?}"
                );
                text.push_str(row[padding.len()..].trim_end());
                count += 1;
            }
            assert!(count > 0, "exercise terminal wrapping");
            assert_eq!(text, expected, "styled list preserves every grapheme");
        }
    });
}

#[test]
fn resumed_cards_keep_full_commands_and_one_blank_separator() {
    resumed_card_layout(false);
}

#[test]
fn resumed_failed_card_keeps_the_full_command() {
    resumed_card_layout(true);
}

fn resumed_card_layout(is_error: bool) {
    use runtime::{ContentBlock, ConversationMessage, Session};
    let env = TestEnv::new_mock("resumed-card-layout");
    let mut saved = Session::new().with_workspace_root(env.workspace_root().to_path_buf());
    saved.push_user_text("Review layout").unwrap();
    let command = "printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz'\necho COMMAND_END";
    saved
        .push_message(ConversationMessage::assistant(vec![
            ContentBlock::Text {
                text: format!("{}\n\nBefore tool.", mock_anthropic_service::LIST_WRAP_DOC),
            },
            ContentBlock::ToolUse {
                id: "layout-tool".into(),
                name: "bash".into(),
                input: serde_json::json!({"command":command,"description":"Inspect full input"})
                    .to_string(),
                thought_signature: None,
            },
        ]))
        .unwrap();
    saved
        .push_message(ConversationMessage::tool_result(
            "layout-tool",
            "bash",
            if is_error {
                "OUTPUT_END: command denied".into()
            } else {
                serde_json::json!({"stdout":"OUTPUT_END","stderr":""}).to_string()
            },
            is_error,
        ))
        .unwrap();
    saved
        .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "After tool.".into(),
        }]))
        .unwrap();
    let path = env.workspace_root().join("layout-session.jsonl");
    saved.save_to_path(&path).unwrap();
    let mut sess = env.spawn_with_env(
        &["--resume", path.to_str().unwrap()],
        &[
            ("NO_COLOR", ""),
            ("COLORFGBG", "15;0"),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(80, 40).unwrap();
    sess.expect("❯").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("After tool."),
        common::DEFAULT_TIMEOUT,
        "replayed transcript",
    );
    assert_list_continuations(&sess, 0);
    sess.render(|screen| {
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let start = rows
            .iter()
            .position(|row| row.starts_with("╭─ Bash("))
            .unwrap();
        assert!(
            rows[start].ends_with('…'),
            "one-row clipped header: {rows:?}"
        );
        assert!(
            rows[start + 1].starts_with("│ $ printf"),
            "body starts immediately after title"
        );
        let before = rows.iter().position(|row| row == "Before tool.").unwrap();
        assert_eq!(start, before + 2, "one blank row before card: {rows:?}");
        let end = (start + 1..rows.len())
            .find(|&row| rows[row].starts_with("╰─"))
            .unwrap();
        let after = rows.iter().position(|row| row == "After tool.").unwrap();
        assert_eq!(after, end + 2, "one blank row after card: {rows:?}");
        let body: String = rows[start + 1..end]
            .iter()
            .map(|row| row.strip_prefix("│ ").unwrap())
            .collect();
        assert!(
            body.contains("$ printf 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz'"),
            "full first command: {body}"
        );
        assert!(body.contains("$ echo COMMAND_END"));
        assert!(body.contains("OUTPUT_END"));
    });
    finish(&mut sess);
}

#[test]
fn unicode_columns_and_semantic_colors_dark() {
    showcase("15;0", "Rgb(99, 168, 248)", "Rgb(166, 227, 161)");
}

#[test]
fn unicode_columns_and_semantic_colors_light() {
    showcase("0;15", "Rgb(28, 100, 200)", "Rgb(64, 160, 43)");
}

#[test]
fn wrapped_read_card_keeps_highlight_on_continuations() {
    let env = TestEnv::new("wrapped-read-style");
    std::fs::write(env.workspace_root().join("fixture.txt"), "X".repeat(160)).expect("fixture");
    let mut sess = start(&env, "15;0");
    let prompt = env.prompt("Read fixture.txt with the Read tool", "read_file_roundtrip");
    sess.send(&format!("{prompt}\r")).expect("submit read");
    common::expect_screen(
        &sess,
        |s| s.contains("XXXXX") && s.contains("ctx "),
        Duration::from_secs(90),
        "completed read",
    );
    sess.render(|screen| {
        let contents = screen.raw().contents();
        let mut colors = Vec::new();
        for (row, text) in screen.raw().rows(0, screen.raw().size().1).enumerate() {
            if text.starts_with("│ ") {
                if let Some(col) = text.find("XXXXX") {
                    let col = text[..col].width();
                    colors.push(format!(
                        "{:?}",
                        screen
                            .raw()
                            .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                            .unwrap()
                            .fgcolor()
                    ));
                }
            }
        }
        assert!(colors.len() >= 4, "wrapped card rows: {contents}");
        assert!(
            colors
                .iter()
                .all(|color| color != "Default" && color == &colors[0]),
            "continuation styles: {colors:?}"
        );
    });
    finish(&mut sess);
}

/// Run explicitly against each immutable release binary, outside a busy build.
/// This measures key -> parsed terminal cell, not physical display latency.
#[test]
#[ignore = "manual release A/B measurement"]
fn release_render_input_measurement() {
    let env = TestEnv::new("render-measurement");
    let mut sess = start(&env, "15;0");
    sess.resize(40, 120).expect("benchmark size");
    measure_keys(&mut sess, "idle");
    report_resources("idle");
    let prompt = env.prompt("Say hello", "delayed_text");
    sess.send(&format!("{prompt}\r")).expect("busy turn");
    // Wait until the request is in flight before measuring typing beside it.
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "submitted input");
    measure_keys(&mut sess, "request_in_flight");
    common::expect_screen(
        &sess,
        |s| s.contains("· turn 1"),
        Duration::from_secs(30),
        "first turn",
    );
    let started = Instant::now();
    for _ in 0..10 {
        let marker = common::turn_status_marker(&sess);
        let prompt = env.prompt(
            "Show a short Rust program in a code fence",
            "syntax_highlight_showcase",
        );
        sess.send(&format!("{prompt}\r")).expect("highlight turn");
        common::expect_turn_complete_after(
            &sess,
            &marker,
            Duration::from_secs(30),
            "highlight turn",
        );
    }
    println!(
        "RENDER_TURNS ten_highlight_turns_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    report_resources("after_ten_highlights");
    measure_keys(&mut sess, "after_ten_highlights");
    finish(&mut sess);
}

#[test]
#[ignore = "manual release A/B measurement"]
fn release_pending_diff_measurement() {
    let env = TestEnv::new("diff-measurement");
    std::fs::write(
        env.workspace_root().join("colors.rs"),
        format!(
            "fn main() {{\n{}\n}}\n",
            mock_anthropic_service::CODEX_DIFF_OLD
        ),
    )
    .unwrap();
    let config = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&config).unwrap();
    let hook = if cfg!(windows) {
        r#"if /I "%HOOK_TOOL_NAME%"=="edit_file" (for /L %i in (1,1,60) do @if not exist color-diff-release (ping -n 2 127.0.0.1 >nul))"#
    } else {
        r#"case "$HOOK_TOOL_NAME" in edit_file|Edit) while [ ! -f color-diff-release ]; do sleep 0.05; done;; esac"#
    };
    std::fs::write(
        config.join("settings.json"),
        serde_json::json!({"hooks":{"PreToolUse":[hook]}}).to_string(),
    )
    .unwrap();
    let started = Instant::now();
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", "15;0"),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(50, 120).unwrap();
    sess.expect("❯").unwrap();
    println!(
        "RENDER_STARTUP no_palette_reply_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    let prompt = env.prompt(
        "Replace old_message with new_message in colors.rs and say Color diff done.",
        "codex_diff_showcase",
    );
    sess.send(&format!("{prompt}\r")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Editing colors.rs") && s.contains("NEW_MARKER"),
        common::LIVE_TURN_BUDGET,
        "pending diff",
    );
    report_resources("pending_diff_before_input");
    measure_keys(&mut sess, "pending_diff");
    report_resources("pending_diff_after_input");
    std::fs::write(env.workspace_root().join("color-diff-release"), "release").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Color diff done.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "edit finished",
    );
    finish(&mut sess);
}
