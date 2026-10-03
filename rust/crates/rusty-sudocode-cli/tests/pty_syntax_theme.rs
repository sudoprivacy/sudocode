//! Compare real CLI terminal cells with captured, unmodified Codex style helpers.
//! The reference fixture is regenerated with e2e/codex-style/capture.py.
mod common;

use common::TestEnv;
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

const MARKDOWN_ROLES: &[(&str, &str)] = &[
    ("Theme prose.", "prose"),
    ("ThemeHeadingOne", "h1"),
    ("ThemeHeadingTwo", "h2"),
    ("ThemeHeadingThree", "h3"),
    ("ThemeHeadingFour", "h4"),
    ("ThemeHeadingFive", "h5"),
    ("ThemeHeadingSix", "h6"),
    ("ThemeStrong", "strong"),
    ("ThemeEmphasis", "emphasis"),
    ("ThemeStrike", "strike"),
    ("~/.zshrc.bak-20261001", "inline"),
    ("~/.config/scode/credentials.bak", "inline"),
    ("ThemeInline", "inline"),
    ("ThemeBoldCode", "bold_code"),
    ("ThemeWeb", "link"),
    ("https://example.com/reference", "link"),
    ("./src/main.rs:42", "inline"),
    ("./src/lib.rs", "inline"),
    ("ThemePath", "prose"),
    ("1.", "ordered"),
    ("ThemeQuote", "quote"),
    ("Highlight done.", "prose"),
];

fn showcase(light: bool, truecolor: bool, no_color: bool) {
    let env = TestEnv::new("syntax-theme");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            // The shared harness forces queueing off; exercise the same inline
            // path as the rendering acceptance and input-latency suites.
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("COLORFGBG", if light { "0;15" } else { "15;0" }),
            ("TERM", "xterm-256color"),
            ("COLORTERM", if truecolor { "truecolor" } else { "" }),
            ("NO_COLOR", if no_color { "1" } else { "" }),
        ],
    );
    sess.resize(110, 100).expect("resize");
    sess.expect("❯").expect("ready");
    let prompt = env.prompt(
        &format!(
            "Reply with exactly this Markdown, without an outer fence or explanation:\n{}",
            mock_anthropic_service::SYNTAX_SHOWCASE_DOC
        ),
        "syntax_highlight_showcase",
    );
    sess.send(&format!("\x1b[200~{prompt}\x1b[201~"))
        .expect("paste fixture request");
    common::expect_screen(
        &sess,
        |s| s.contains("Pasted") || s.contains("Reply with exactly"),
        common::DEFAULT_TIMEOUT,
        "paste consumed",
    );
    sess.send("\r").expect("submit");
    common::expect_screen(
        &sess,
        |s| s.contains("Highlight done.") && s.contains("ctx "),
        common::LIVE_TURN_BUDGET,
        "syntax response completed",
    );
    // Retain cells even if a raw-attribute assertion fails before inspection.
    sess.render(|screen| save_report(screen, light, truecolor, no_color));
    if !no_color {
        // vt100 exposes no strikethrough attribute; inspect the real PTY bytes.
        sess.expect("\\x1b\\[9mThemeStrike")
            .expect("Markdown strikethrough reaches the terminal");
    }
    sess.render(|screen| check_screen(screen, light, truecolor, no_color));
    sess.send("theme input").expect("type after highlight");
    common::expect_input_line(
        &sess,
        "theme input",
        common::DEFAULT_TIMEOUT,
        "input remains usable",
    );
    sess.send("\x15").expect("clear input");
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "input cleared");
    sess.send("/exit").expect("exit text");
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "exit input");
    sess.send("\r").expect("exit");
    assert_eq!(sess.expect_eof().expect("clean exit"), 0);
}

fn check_screen(screen: &pty_expect::Screen, light: bool, truecolor: bool, no_color: bool) {
    let reference: Value =
        serde_json::from_str(include_str!("fixtures/codex_styles.json")).unwrap();
    let key = format!(
        "{}-{}",
        if light { "light" } else { "dark" },
        if truecolor { "truecolor" } else { "indexed" }
    );
    let expected = &reference["variants"][key];
    let raw = screen.raw();
    let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
    let start = rows
        .iter()
        .rposition(|row| row.contains("Theme prose."))
        .expect("assistant response");
    let end = rows
        .iter()
        .rposition(|row| row.contains("Highlight done."))
        .expect("response end");
    let response = rows[start..=end].join("\n");
    assert!(
        !response.contains('`'),
        "inline backticks leaked into the transcript: {response}"
    );
    assert!(
        !response.contains("╭─") && !response.contains("╰─"),
        "code fence chrome differs from Codex: {response}"
    );
    for &(text, role) in MARKDOWN_ROLES {
        let (row, line) = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start + 1)
            .find(|(_, line)| line.contains(text))
            .unwrap_or_else(|| panic!("missing {text}: {response}"));
        let col = line[..line.find(text).unwrap()].width();
        for offset in 0..text.chars().count() {
            check_cell(
                screen,
                row,
                col + offset,
                &expected["styles"]
                    [usize::try_from(expected["roles"][role].as_u64().unwrap()).unwrap()],
                no_color,
            );
        }
    }
    let mut cursor = start;
    for document in expected["documents"].as_array().unwrap() {
        for line in document["lines"].as_array().unwrap() {
            let text = line["text"].as_str().unwrap();
            if text.trim().is_empty() {
                continue;
            }
            let (row, rendered) = rows
                .iter()
                .enumerate()
                .skip(cursor)
                .take(end - cursor + 1)
                .find(|(_, row)| row.contains(text))
                .unwrap_or_else(|| panic!("missing code {text:?}: {response}"));
            cursor = row + 1;
            let mut col = rendered[..rendered.find(text).unwrap()].width();
            for span in line["spans"].as_array().unwrap() {
                for character in span["text"].as_str().unwrap().chars() {
                    if !character.is_whitespace() {
                        check_cell(
                            screen,
                            row,
                            col,
                            &expected["styles"]
                                [usize::try_from(span["style"].as_u64().unwrap()).unwrap()],
                            no_color,
                        );
                    }
                    col += character.to_string().width();
                }
            }
        }
    }
}

fn check_cell(
    screen: &pty_expect::Screen,
    row: usize,
    col: usize,
    expected: &Value,
    no_color: bool,
) {
    let cell = screen
        .raw()
        .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
        .unwrap();
    let actual = json!({
        "fg": format!("{:?}", cell.fgcolor()),
        "bg": format!("{:?}", cell.bgcolor()),
        "bold": cell.bold(),
        "italic": cell.italic(),
        "underline": cell.underline(),
    });
    let wanted = if no_color {
        json!({"fg":"Default", "bg":"Default", "bold":false, "italic":false, "underline":false})
    } else {
        json!({"fg":expected["fg"], "bg":expected["bg"], "bold":expected["bold"], "italic":expected["italic"], "underline":expected["underline"]})
    };
    assert_eq!(
        actual,
        wanted,
        "Codex style mismatch at ({row}, {col}) {:?}",
        cell.contents()
    );
    assert!(!cell.dim(), "unexpected dim at ({row}, {col})");
}

fn save_report(screen: &pty_expect::Screen, light: bool, truecolor: bool, no_color: bool) {
    if let Ok(dir) = std::env::var("SCODE_THEME_REPORT_DIR") {
        let raw = screen.raw();
        let cells: Vec<_> = (0..raw.size().0).map(|row| {
            (0..raw.size().1).map(|col| {
                let cell = raw.cell(row, col).unwrap();
                json!({"text":cell.contents(), "fg":format!("{:?}",cell.fgcolor()), "bg":format!("{:?}",cell.bgcolor()), "bold":cell.bold(), "dim":cell.dim(), "italic":cell.italic(), "underline":cell.underline()})
            }).collect::<Vec<_>>()
        }).collect();
        let name = format!("theme-light-{light}-truecolor-{truecolor}-no-color-{no_color}.json");
        std::fs::write(
            std::path::Path::new(&dir).join(name),
            serde_json::to_vec_pretty(
                &json!({"light":light,"truecolor":truecolor,"no_color":no_color,"cells":cells}),
            )
            .unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn dark_truecolor_matches_codex() {
    showcase(false, true, false);
}

#[test]
fn light_truecolor_matches_codex() {
    showcase(true, true, false);
}

#[test]
fn dark_indexed_matches_codex() {
    showcase(false, false, false);
}

#[test]
fn light_indexed_matches_codex() {
    showcase(true, false, false);
}

#[test]
fn no_color_markdown_stays_plain() {
    showcase(false, true, true);
}

#[test]
fn oversized_code_falls_back_without_blocking_input() {
    // Exact byte counts require a deterministic response, independent of a model.
    let env = TestEnv::new_mock("syntax-highlight-limits");
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[
            ("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue"),
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", ""),
        ],
    );
    sess.resize(70, 100).expect("resize");
    sess.expect("❯").expect("ready");
    let prompt = env.prompt("Show the long-line fixture", "syntax_highlight_limits");
    sess.send(&format!("{prompt}\r")).expect("submit");
    common::expect_screen(
        &sess,
        |s| s.contains("Limit done.") && s.contains("ctx "),
        common::DEFAULT_TIMEOUT,
        "large code completed",
    );
    sess.render(|screen| {
        let rows: Vec<_> = screen.raw().rows(0, screen.raw().size().1).collect();
        let (row, text) = rows.iter().enumerate().rfind(|(_, row)| row.contains("fn limit_probe")).expect("plain fallback code");
        let col = text[..text.find("fn limit_probe").unwrap()].width();
        let plain = json!({"fg":"Default", "bg":"Default", "bold":false, "italic":false, "underline":false});
        check_cell(screen, row, col, &plain, false);
    });
    sess.send("input after large code").expect("type");
    common::expect_input_line(
        &sess,
        "input after large code",
        common::DEFAULT_TIMEOUT,
        "input remains responsive",
    );
    sess.send("\x15").expect("clear");
    common::expect_input_line_cleared(&sess, common::DEFAULT_TIMEOUT, "cleared");
    sess.send("/exit\r").expect("exit");
    assert_eq!(sess.expect_eof().expect("exit"), 0);
}
