//! Compare real terminal cells with output from Codex's original highlighter.
//! The fixture was generated from openai/codex b741e480 (two-face 0.5.1),
//! independently of scode's renderer. No Codex/ratatui dependency is required.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::Value;

fn showcase(env: &TestEnv, background: &str, no_color: bool) -> PtySession {
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", background),
            ("NO_COLOR", if no_color { "1" } else { "" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(100, 110).expect("reference viewport");
    sess.expect("❯").expect("prompt");
    let prompt = env.prompt(
        &format!(
            "Reply with exactly this Markdown, without wrapping the entire answer in a code fence:\n{}",
            mock_anthropic_service::CODEX_COLORS_SHOWCASE_DOC
        ),
        "codex_colors_showcase",
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .expect("paste prompt");
    common::expect_screen(
        &sess,
        |screen| screen.contains("Pasted") || screen.contains("PARITY_SCENARIO:"),
        common::DEFAULT_TIMEOUT,
        "pasted input",
    );
    sess.send("\r").expect("submit");
    common::expect_screen(
        &sess,
        |screen| screen.contains("Color reference done.") && screen.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "completed reference response",
    );
    sess
}

fn finish(sess: &mut PtySession) {
    sess.send("/exit").expect("type exit");
    common::expect_input_line(sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").expect("exit");
    assert_eq!(sess.expect_eof().expect("clean exit"), 0);
}

fn assert_reference(light: bool, no_color: bool) {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/codex_default_colors.json")).unwrap();
    let reference = &reference["themes"][usize::from(light)];
    let env = TestEnv::new("codex-colors");
    let mut sess = showcase(&env, if light { "0;15" } else { "15;0" }, no_color);
    sess.render(|screen| {
        let raw = screen.raw();
        let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
        let inline_row = rows
            .iter()
            .rposition(|row| row.contains("Inline tokens: source ~/.zshrc and cc-sudo."))
            .expect("rendered inline code without literal Markdown backticks");
        let inline_col = rows[inline_row].find("source ~/.zshrc").unwrap();
        let cell = raw
            .cell(
                u16::try_from(inline_row).unwrap(),
                u16::try_from(inline_col).unwrap(),
            )
            .unwrap();
        assert_eq!(
            format!("{:?}", cell.fgcolor()),
            if no_color {
                "Default"
            } else {
                reference["inline_foreground"].as_str().unwrap()
            }
        );
        assert_eq!(format!("{:?}", cell.bgcolor()), "Default");
        let mut cursor = inline_row + 1;
        for sample in reference["samples"].as_array().unwrap() {
            for spans in sample["rows"].as_array().unwrap() {
                let spans = spans.as_array().unwrap();
                let expected: String = spans
                    .iter()
                    .map(|span| span["text"].as_str().unwrap())
                    .collect();
                let row = (cursor..rows.len())
                    .find(|&row| rows[row].ends_with(&expected))
                    .unwrap_or_else(|| {
                        panic!("missing source line {expected:?}: {}", raw.contents())
                    });
                cursor = row + 1;
                let mut col = rows[row].len() - expected.len();
                for span in spans {
                    for ch in span["text"].as_str().unwrap().chars() {
                        let cell = raw
                            .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                            .unwrap();
                        // ConPTY may coalesce foreground-only changes on blank
                        // cells. Compare every visible source character; spaces
                        // still participate in text, width and background checks.
                        if !ch.is_whitespace() {
                            assert_eq!(
                                format!("{:?}", cell.fgcolor()),
                                if no_color {
                                    "Default"
                                } else {
                                    span["foreground"].as_str().unwrap()
                                },
                                "{} row {row} col {col}: {expected}",
                                reference["theme"]
                            );
                        }
                        assert_eq!(
                            format!("{:?}", cell.bgcolor()),
                            "Default",
                            "code uses terminal background"
                        );
                        assert_eq!(cell.bold(), !no_color && span["bold"].as_bool().unwrap());
                        assert!(!cell.italic(), "syntax italics are suppressed like Codex");
                        assert!(
                            !cell.underline(),
                            "syntax underlines are suppressed like Codex"
                        );
                        col += 1;
                    }
                }
            }
        }
        let row = rows
            .iter()
            .position(|row| row.contains("Unhighlighted fallback stays plain."))
            .unwrap();
        let col = rows[row].find("Unhighlighted").unwrap();
        assert_eq!(
            format!(
                "{:?}",
                raw.cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                    .unwrap()
                    .fgcolor()
            ),
            "Default"
        );
    });
    finish(&mut sess);
}

#[test]
fn dark_default_matches_codex_reference() {
    assert_reference(false, false);
}

#[test]
fn light_default_matches_codex_reference() {
    assert_reference(true, false);
}

#[test]
fn no_color_preserves_code_without_theme_escapes() {
    assert_reference(false, true);
}

fn colored_session(env: &TestEnv, light: bool) -> PtySession {
    env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", if light { "0;15" } else { "15;0" }),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    )
}

// ConPTY consumes OSC queries instead of exposing them to the PTY peer.
// Windows exercises unsupported-probe fallback and late input below.
#[cfg(unix)]
#[test]
fn palette_probe_preserves_early_keys_paste_and_ignores_late_replies() {
    let env = TestEnv::new("palette-input");
    let mut sess = colored_session(&env, false);
    sess.resize(40, 100).unwrap();
    sess.expect(r"\x1b\]11;\?")
        .expect("one startup palette query");
    // Fragment a reply across writes and place real input between replies.
    sess.send("early-\x1b]10;rgb:ffff/").unwrap();
    sess.send("ffff/ffff\x07\x1b[200~paste '$HOME' ~/.zshrc\x1b[201~")
        .unwrap();
    sess.send("\x1b]11;rgb:0000/0000/0000\x1b\\").unwrap();
    common::expect_input_line(
        &sess,
        "early-paste '$HOME' ~/.zshrc",
        common::DEFAULT_TIMEOUT,
        "queued input retained exactly",
    );
    let input = common::input_line_of(&sess.render(|s| s.contents()));
    assert_eq!(input.trim(), "early-paste '$HOME' ~/.zshrc");
    sess.send("\x15").unwrap();
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "clear early draft");
    // The startup window is now closed. Neither late nor malformed OSC
    // payloads may leak into a command the user is composing.
    sess.send("\x1b]11;rgb:ffff/ffff/ffff\x07\x1b]10;rgb:bad!\x07after")
        .unwrap();
    common::expect_input_line(
        &sess,
        "after",
        common::DEFAULT_TIMEOUT,
        "late reply ignored",
    );
    assert_eq!(
        common::input_line_of(&sess.render(|s| s.contents())).trim(),
        "after"
    );
    sess.send("\x15").unwrap();
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "clear draft");
    finish(&mut sess);
}

#[cfg(unix)]
#[test]
fn detected_background_overrides_colorfgbg() {
    let env = TestEnv::new("palette-override");
    let mut sess = colored_session(&env, false);
    sess.resize(100, 110).unwrap();
    sess.expect(r"\x1b\]11;\?").expect("palette query");
    sess.send("\x1b]10;rgb:0000/0000/0000\x07\x1b]11;rgb:ffff/ffff/ffff\x07")
        .unwrap();
    sess.expect("❯").unwrap();
    let prompt = env.prompt(
        &format!(
            "Reply with exactly this Markdown:\n{}",
            mock_anthropic_service::CODEX_COLORS_SHOWCASE_DOC
        ),
        "codex_colors_showcase",
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("PARITY_SCENARIO:"),
        common::DEFAULT_TIMEOUT,
        "prompt pasted",
    );
    sess.send("\r").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Color reference done.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "reference rendered",
    );
    assert_cell(&sess, "source ~/.zshrc", "Rgb(64, 160, 43)", "Default");
    finish(&mut sess);
}

#[test]
fn unsupported_probe_keeps_typing_and_late_replies_separate() {
    let env = TestEnv::new("palette-fallback-input");
    let mut sess = colored_session(&env, false);
    sess.resize(40, 100).unwrap();
    sess.expect("❯")
        .expect("unsupported query falls back to a usable prompt");
    sess.send("\x1b]11;rgb:ffff/ffff/ffff\x07after").unwrap();
    common::expect_input_line(
        &sess,
        "after",
        common::DEFAULT_TIMEOUT,
        "late response is not input",
    );
    sess.send("\x1b]").unwrap();
    sess.send("typing").unwrap();
    common::expect_input_line(
        &sess,
        "aftertyping",
        common::DEFAULT_TIMEOUT,
        "Alt bracket does not swallow keys",
    );
    assert_eq!(
        common::input_line_of(&sess.render(|s| s.contents())).trim(),
        "aftertyping"
    );
    sess.send("\x15").unwrap();
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "clear draft");
    finish(&mut sess);
}

fn assert_cell(sess: &PtySession, needle: &str, foreground: &str, background: &str) {
    sess.render(|screen| {
        let raw = screen.raw();
        let (row, text) = raw
            .rows(0, raw.size().1)
            .enumerate()
            .filter(|(_, s)| s.contains(needle))
            .last()
            .unwrap_or_else(|| panic!("{needle}: {}", raw.contents()));
        let col = unicode_width::UnicodeWidthStr::width(&text[..text.find(needle).unwrap()]);
        let cell = raw
            .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
            .unwrap();
        assert_eq!(
            format!("{:?}", cell.fgcolor()),
            foreground,
            "{needle} foreground"
        );
        assert_eq!(
            format!("{:?}", cell.bgcolor()),
            background,
            "{needle} background"
        );
    });
}

fn assert_diff(sess: &PtySession, light: bool) {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/codex_default_colors.json")).unwrap();
    let reference = &reference["themes"][usize::from(light)];
    let added_bg = if light {
        "Rgb(218, 251, 225)"
    } else {
        "Rgb(33, 58, 43)"
    };
    let removed_bg = if light {
        "Rgb(255, 235, 233)"
    } else {
        "Rgb(74, 34, 29)"
    };
    // The spinner intentionally never settles. Wait for the source cells, not
    // for the entire terminal to stop changing, including partial PTY writes.
    common::expect_screen(
        sess,
        |_| {
            sess.render(|screen| {
                let raw = screen.raw();
                let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
                for (needle, foreground, background) in [
                    ("OLD_MARKER", "diff_removed_string", removed_bg),
                    ("NEW_MARKER", "diff_added_string", added_bg),
                ] {
                    let Some(row) = rows.iter().rposition(|s| s.contains(needle)) else {
                        return false;
                    };
                    let col = unicode_width::UnicodeWidthStr::width(
                        &rows[row][..rows[row].find(needle).unwrap()],
                    );
                    let Some(cell) =
                        raw.cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                    else {
                        return false;
                    };
                    if format!("{:?}", cell.fgcolor()) != reference[foreground].as_str().unwrap()
                        || format!("{:?}", cell.bgcolor()) != background
                    {
                        return false;
                    }
                }
                let row = rows.iter().rposition(|s| s.contains("NEW_MARKER")).unwrap() + 1;
                rows.get(row).is_some_and(|s| s.starts_with("│ "))
                    && [2, raw.size().1 - 3].into_iter().all(|col| {
                        raw.cell(u16::try_from(row).unwrap(), col)
                            .is_some_and(|cell| format!("{:?}", cell.bgcolor()) == added_bg)
                    })
            })
        },
        common::DEFAULT_TIMEOUT,
        "Codex diff foreground, fill and wrapped padding",
    );
}

fn diff_roundtrip(light: bool) {
    use mock_anthropic_service::{CODEX_DIFF_NEW, CODEX_DIFF_OLD};
    let env = TestEnv::new("codex-diff");
    let original = format!("fn main() {{\n{CODEX_DIFF_OLD}\n}}\n");
    std::fs::write(env.workspace_root().join("colors.rs"), &original).unwrap();
    // Hold the real tool at a user hook so the running preview can be
    // inspected and resized without racing an instant filesystem edit.
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
    let mut sess = colored_session(&env, light);
    sess.resize(60, 100).unwrap();
    sess.expect("❯").unwrap();
    let prompt = env.prompt(&format!("First read colors.rs, then use edit_file to replace this exact line:\n{CODEX_DIFF_OLD}\nwith:\n{CODEX_DIFF_NEW}\nThen say Color diff done."), "codex_diff_showcase");
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("PARITY_SCENARIO:"),
        common::DEFAULT_TIMEOUT,
        "edit prompt pasted",
    );
    sess.send("\r").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Editing ") && s.contains("colors.rs") && s.contains("NEW_MARKER"),
        common::LIVE_TURN_BUDGET,
        "running diff",
    );
    assert_diff(&sess, light);
    sess.send("draft-copy-test").unwrap();
    common::expect_input_line(
        &sess,
        "draft-copy-test",
        common::DEFAULT_TIMEOUT,
        "input during colored preview",
    );
    assert_cell(&sess, "draft-copy-test", "Default", "Default");
    // Resize, input and async wakeups can share a native readiness batch.
    // Each key must appear without a second key waking a stalled reader.
    let mut draft = String::from("draft-copy-test");
    for (width, key) in [(94, "1"), (86, "2"), (100, "3")] {
        sess.resize(60, width).unwrap();
        sess.send(key).unwrap();
        draft.push_str(key);
        common::expect_input_line(
            &sess,
            &draft,
            common::DEFAULT_TIMEOUT,
            "input immediately after preview resize",
        );
    }
    sess.resize(60, 78).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("NEW_MARKER") && s.contains(&draft),
        common::DEFAULT_TIMEOUT,
        "reflowed preview",
    );
    assert_diff(&sess, light);
    sess.send("\x15").unwrap();
    common::expect_screen(
        &sess,
        |s| common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "clear pending draft",
    );
    std::fs::write(env.workspace_root().join("color-diff-release"), "release").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Color diff done.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "completed edit",
    );
    let output = sess
        .expect("Color diff done\\.")
        .expect("completed output bytes");
    assert!(!output.contains("\x1b[?1049h"), "native scrollback only");
    assert!(!output.contains("\x1b[3J"), "never purge scrollback");
    assert_eq!(
        std::fs::read_to_string(env.workspace_root().join("colors.rs")).unwrap(),
        original.replace(CODEX_DIFF_OLD, CODEX_DIFF_NEW)
    );
    assert_diff(&sess, light);
    // A source snippet copied from history must remain ordinary input text.
    sess.send("\x1b[200~source ~/.zshrc && cc-sudo\x1b[201~")
        .unwrap();
    common::expect_input_line(
        &sess,
        "source ~/.zshrc && cc-sudo",
        common::DEFAULT_TIMEOUT,
        "paste source snippet",
    );
    assert_cell(&sess, "source ~/.zshrc && cc-sudo", "Default", "Default");
    sess.send("\x15").unwrap();
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "clear pasted draft");
    finish(&mut sess);
}

#[test]
fn dark_diff_keeps_backgrounds_through_preview_resize_and_scrollback() {
    diff_roundtrip(false);
}

#[test]
fn light_diff_keeps_backgrounds_through_preview_resize_and_scrollback() {
    diff_roundtrip(true);
}

fn expect_bash_sample(sess: &PtySession, sample: &Value, prefix: &str, no_color: bool) {
    common::expect_screen(
        sess,
        |_| {
            sess.render(|screen| {
                let raw = screen.raw();
                let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
                for spans in sample["rows"].as_array().unwrap() {
                    let spans = spans.as_array().unwrap();
                    let source: String = spans
                        .iter()
                        .map(|span| span["text"].as_str().unwrap())
                        .collect();
                    let needle = format!("{prefix}{source}");
                    let Some(row) = rows.iter().rposition(|s| s.contains(&needle)) else {
                        return false;
                    };
                    let start = rows[row].find(&needle).unwrap() + prefix.len();
                    let mut col = unicode_width::UnicodeWidthStr::width(&rows[row][..start]);
                    for span in spans {
                        for ch in span["text"].as_str().unwrap().chars() {
                            let cell = raw
                                .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
                                .unwrap();
                            let foreground = if no_color {
                                "Default"
                            } else {
                                span["foreground"].as_str().unwrap()
                            };
                            if (!ch.is_whitespace()
                                && format!("{:?}", cell.fgcolor()) != foreground)
                                || format!("{:?}", cell.bgcolor()) != "Default"
                                || cell.bold() != (!no_color && span["bold"].as_bool().unwrap())
                                || cell.italic()
                                || cell.underline()
                            {
                                return false;
                            }
                            col += 1;
                        }
                    }
                }
                true
            })
        },
        common::DEFAULT_TIMEOUT,
        "Bash command cells match original Codex colors",
    );
}

fn expect_tool_border(sess: &PtySession, header: &str, foreground: &str, no_color: bool) {
    common::expect_screen(
        sess,
        |_| {
            sess.render(|screen| {
                let raw = screen.raw();
                let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
                let Some(start) = rows.iter().rposition(|row| row.contains(header)) else {
                    return false;
                };
                // No new icon or column: the original opening frame precedes
                // the tool name, and every following row shares its column.
                if !rows[start].starts_with(&format!("╭─ {header}")) {
                    return false;
                }
                let Some(end) = (start + 1..rows.len()).find(|&row| rows[row].starts_with("╰─"))
                else {
                    return false;
                };
                (start..=end).all(|row| {
                    let cap = row == start || row == end;
                    if !cap && !rows[row].starts_with('│') {
                        return false;
                    }
                    (0..if cap { 2 } else { 1 }).all(|col| {
                        let cell = raw.cell(u16::try_from(row).unwrap(), col).unwrap();
                        format!("{:?}", cell.fgcolor()) == foreground
                            && format!("{:?}", cell.bgcolor()) == "Default"
                            && cell.bold() == (cap && !no_color)
                            && !cell.dim()
                    })
                })
            })
        },
        common::DEFAULT_TIMEOUT,
        "tool status color, bold endpoints and normal vertical border",
    );
}

fn expect_completed_bash_borders(sess: &PtySession, light: bool, no_color: bool) {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/codex_default_colors.json")).unwrap();
    let success = if no_color {
        "Default"
    } else {
        reference["themes"][usize::from(light)]["inline_foreground"]
            .as_str()
            .unwrap()
    };
    expect_tool_border(
        sess,
        &format!("Bash({})", mock_anthropic_service::CODEX_BASH_SINGLE),
        success,
        no_color,
    );
    expect_tool_border(sess, "Bash(# Shell colors", success, no_color);
}

fn assert_running_title_resizes(sess: &mut PtySession, header: &str, light: bool, no_color: bool) {
    assert_cell(
        sess,
        "Bash(",
        if no_color {
            "Default"
        } else if light {
            "Rgb(28, 100, 200)"
        } else {
            "Rgb(99, 168, 248)"
        },
        "Default",
    );
    // A running title occupies exactly one row, and resizing can reveal the
    // original cached identity again without losing the user's draft.
    sess.resize(80, 28).unwrap();
    common::expect_screen(
        sess,
        |s| {
            s.lines()
                .any(|line| line.starts_with("╭─ Bash(") && line.ends_with('…'))
        },
        common::DEFAULT_TIMEOUT,
        "single-row clipped title",
    );
    sess.render(|screen| {
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let start = rows
            .iter()
            .rposition(|row| row.starts_with("╭─ Bash("))
            .unwrap();
        assert!(
            rows[start + 1].starts_with("╰─"),
            "title must not occupy body rows: {rows:?}"
        );
    });
    sess.resize(80, 100).unwrap();
    common::expect_screen(
        sess,
        |s| s.contains(header),
        common::DEFAULT_TIMEOUT,
        "full title restored on widening",
    );
}

fn bash_roundtrip(light: bool, no_color: bool) {
    use mock_anthropic_service::{CODEX_BASH_MULTI, CODEX_BASH_SINGLE};
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/codex_bash_colors.json")).unwrap();
    let samples = reference["themes"][usize::from(light)]["samples"]
        .as_array()
        .unwrap();
    assert_eq!(samples[0]["code"], CODEX_BASH_SINGLE);
    assert_eq!(samples[1]["code"], CODEX_BASH_MULTI);
    let env = TestEnv::new("codex-bash");
    let config = env.workspace_root().join(".nexus/sudocode");
    std::fs::create_dir_all(&config).unwrap();
    let hook = if cfg!(windows) {
        r#"if /I "%HOOK_TOOL_NAME%"=="bash" (for /L %i in (1,1,60) do @if not exist color-bash-release (ping -n 2 127.0.0.1 >nul))"#
    } else {
        r#"case "$HOOK_TOOL_NAME" in bash|Bash) while [ ! -f color-bash-release ]; do sleep 0.05; done;; esac"#
    };
    std::fs::write(
        config.join("settings.json"),
        serde_json::json!({"hooks":{"PreToolUse":[hook]}}).to_string(),
    )
    .unwrap();
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", if light { "0;15" } else { "15;0" }),
            ("NO_COLOR", if no_color { "1" } else { "" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(80, 100).unwrap();
    sess.expect("❯").unwrap();
    let prompt = env.prompt(
        &format!(
            "Run these two scripts with Bash, in separate calls and exactly as written. First:\n{CODEX_BASH_SINGLE}\nSecond:\n{CODEX_BASH_MULTI}\nThen say only Color bash done."
        ),
        "codex_bash_showcase",
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~")).unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("PARITY_SCENARIO:"),
        common::DEFAULT_TIMEOUT,
        "Bash prompt pasted",
    );
    sess.send("\r").unwrap();
    common::expect_screen(
        &sess,
        |s| s.contains(&format!("Bash({CODEX_BASH_SINGLE})")),
        common::LIVE_TURN_BUDGET,
        "running Bash header",
    );
    expect_bash_sample(&sess, &samples[0], "Bash(", no_color);
    let header = format!("Bash({CODEX_BASH_SINGLE})");
    let running = if no_color {
        "Default"
    } else if light {
        "Idx(94)"
    } else {
        "Idx(214)"
    };
    expect_tool_border(&sess, &header, running, no_color);
    assert_running_title_resizes(&mut sess, &header, light, no_color);
    sess.send("\x1b[200~draft '$HOME' && echo hi\x1b[201~")
        .unwrap();
    common::expect_input_line(
        &sess,
        "draft '$HOME' && echo hi",
        common::DEFAULT_TIMEOUT,
        "paste beside highlighted running command",
    );
    assert_cell(&sess, "draft '$HOME' && echo hi", "Default", "Default");
    sess.resize(80, 78).unwrap();
    expect_bash_sample(&sess, &samples[0], "Bash(", no_color);
    expect_tool_border(&sess, &header, running, no_color);
    sess.send("\x15").unwrap();
    common::expect_screen(
        &sess,
        |s| common::input_line_of(s).is_empty(),
        common::DEFAULT_TIMEOUT,
        "clear Bash draft while the spinner remains active",
    );
    std::fs::write(env.workspace_root().join("color-bash-release"), "release").unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("Color bash done.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "completed Bash calls",
    );
    expect_bash_sample(&sess, &samples[0], "Bash(", no_color);
    expect_bash_sample(&sess, &samples[1], "│ $ ", no_color);
    expect_completed_bash_borders(&sess, light, no_color);
    // Actual process output stays plain; command styles cannot bleed into it.
    for output in ["CODEX_SHELL_OUTPUT", "MULTILINE", "CONTINUATION"] {
        assert_cell(&sess, output, "Default", "Default");
    }
    finish(&mut sess);
}

#[test]
fn dark_bash_commands_match_codex_in_preview_and_scrollback() {
    bash_roundtrip(false, false);
}

#[test]
fn light_bash_commands_match_codex_in_preview_and_scrollback() {
    bash_roundtrip(true, false);
}

#[test]
fn no_color_bash_commands_preserve_plain_source_and_output() {
    bash_roundtrip(false, true);
}

#[test]
fn denied_tool_keeps_a_red_border_without_a_new_status_icon() {
    let env = TestEnv::new_mock("tool-border-denied");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", "15;0"),
            ("NO_COLOR", ""),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
        ],
    );
    sess.resize(60, 100).unwrap();
    sess.expect("❯").unwrap();
    sess.send("PARITY_SCENARIO:bash_stdout_roundtrip\r")
        .unwrap();
    common::expect_screen_settled(
        &sess,
        |s| s.contains("bash completed:") && s.contains("ctx ") && s.contains("Bash("),
        common::DEFAULT_TIMEOUT,
        "denied Bash result",
    );
    expect_tool_border(&sess, "Bash(printf 'alpha from bash')", "Idx(9)", false);
    finish(&mut sess);
}
