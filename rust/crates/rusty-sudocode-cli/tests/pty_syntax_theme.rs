//! Check the colors users receive after syntax highlighting, wrapping and PTY
//! rendering. Contrast is measured on emitted cells, including comments.
mod common;

use common::TestEnv;
use serde_json::json;

fn palette_rgb(index: u8) -> [u8; 3] {
    assert!(index >= 16, "expected a fixed-palette color, got {index}");
    if index >= 232 {
        return [8 + (index - 232) * 10; 3];
    }
    let cube = [0, 95, 135, 175, 215, 255];
    let n = usize::from(index - 16);
    [cube[n / 36], cube[n / 6 % 6], cube[n % 6]]
}

// pty-expect intentionally does not re-export its terminal parser's Color type.
fn rgb(color: &str) -> [u8; 3] {
    if let Some(index) = color.strip_prefix("Idx(").and_then(|s| s.strip_suffix(')')) {
        return palette_rgb(index.parse().unwrap());
    }
    let values: Vec<u8> = color
        .strip_prefix("Rgb(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or_else(|| panic!("expected an explicit code color, got {color}"))
        .split(',')
        .map(|v| v.trim().parse().unwrap())
        .collect();
    values.try_into().unwrap()
}

fn luminance(rgb: [u8; 3]) -> f64 {
    let linear = rgb.map(|channel| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    });
    linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
}

fn contrast(foreground: [u8; 3], background: [u8; 3]) -> f64 {
    let a = luminance(foreground);
    let b = luminance(background);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

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
    sess.resize(70, 100).expect("resize");
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
    // Retain the terminal on assertion failure as well as successful samples.
    save_report(screen, light, truecolor, no_color, &[]);
    let raw = screen.raw();
    let rows: Vec<_> = raw.rows(0, raw.size().1).collect();
    let response_start = rows
        .iter()
        .rposition(|row| row.contains("╭─ rust"))
        .expect("assistant code block");
    let mut samples = Vec::new();
    for (line, token) in [
        ("// ThemeComment", "ThemeComment"),
        ("fn palette_probe", "fn"),
        ("fn palette_probe", "palette_probe"),
        ("let message =", "ThemeString"),
        ("count >= 42", "42"),
        ("# PythonComment", "PythonComment"),
        ("def python_probe", "def"),
        ("def python_probe", "python_probe"),
        ("return \"PythonString\"", "PythonString"),
        ("{\"theme_key\":", "JsonString"),
        ("{\"theme_key\":", "42"),
        ("plain_identifier", "plain_identifier"),
        ("struct ThemeType", "ThemeType"),
        ("-removed_value", "removed_value"),
        ("+added_value", "added_value"),
    ] {
        let (row, text) = rows
            .iter()
            .enumerate()
            .skip(response_start)
            .rfind(|(_, r)| r.contains(line))
            .unwrap_or_else(|| panic!("missing {line:?}: {}", raw.contents()));
        // All fixture characters before each token are ASCII, so byte and
        // terminal column offsets coincide, apart from the response bullet.
        let col = unicode_width::UnicodeWidthStr::width(&text[..text.find(token).unwrap()]);
        let cell = raw
            .cell(u16::try_from(row).unwrap(), u16::try_from(col).unwrap())
            .unwrap();
        let fg = format!("{:?}", cell.fgcolor());
        let bg = format!("{:?}", cell.bgcolor());
        if no_color {
            assert_eq!((&*fg, &*bg), ("Default", "Default"), "{token}");
        } else {
            assert!(
                fg.starts_with(if truecolor { "Rgb(" } else { "Idx(" }),
                "{token}: {fg}"
            );
            let ratio = contrast(rgb(&fg), rgb(&bg));
            assert!(
                ratio >= 4.5,
                "{token}: {fg} on {bg} has {ratio:.3}:1 contrast"
            );
            assert_eq!(
                bg,
                format!("Idx({})", if light { 255 } else { 236 }),
                "{token} background"
            );
            assert!(!cell.dim(), "{token} must not dim a readable palette");
            samples.push((token, rgb(&fg), ratio));
        }
    }
    if !no_color {
        // Amber is shared by Rust/Python keywords; comments, strings,
        // functions and constants remain independently recognizable.
        assert_eq!(samples[1].1, samples[6].1);
        assert_eq!(samples[1].1, palette_rgb(if light { 94 } else { 214 }));
        let unique: std::collections::HashSet<_> = samples[..5].iter().map(|s| s.1).collect();
        assert_eq!(unique.len(), 5, "syntax roles collapsed: {samples:?}");
        assert_eq!(
            samples[4].1, samples[12].1,
            "types and constants share violet"
        );
        assert_ne!(
            samples[13].1, samples[14].1,
            "diff additions and removals must stay distinct"
        );
        check_all_code_cells(screen);
    }
    let (row, text) = rows
        .iter()
        .enumerate()
        .rfind(|(_, r)| r.contains("Highlight done."))
        .unwrap();
    let cell = raw
        .cell(
            u16::try_from(row).unwrap(),
            u16::try_from(text.find("Highlight").unwrap()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        format!("{:?}", cell.bgcolor()),
        "Default",
        "code background leaked into prose"
    );
    save_report(screen, light, truecolor, no_color, &samples);
}

fn save_report(
    screen: &pty_expect::Screen,
    light: bool,
    truecolor: bool,
    no_color: bool,
    samples: &[(&str, [u8; 3], f64)],
) {
    let raw = screen.raw();
    if let Ok(dir) = std::env::var("SCODE_THEME_REPORT_DIR") {
        let cells: Vec<_> = (0..raw.size().0).map(|row| {
            (0..raw.size().1).map(|col| {
                let cell = raw.cell(row, col).unwrap();
                json!({"text":cell.contents(), "fg":format!("{:?}",cell.fgcolor()), "bg":format!("{:?}",cell.bgcolor()), "bold":cell.bold(), "dim":cell.dim()})
            }).collect::<Vec<_>>()
        }).collect();
        let name = format!("theme-light-{light}-truecolor-{truecolor}-no-color-{no_color}.json");
        std::fs::write(std::path::Path::new(&dir).join(name), serde_json::to_vec_pretty(&json!({"light":light,"truecolor":truecolor,"no_color":no_color,"samples":samples,"cells":cells})).unwrap()).unwrap();
    }
}

fn check_all_code_cells(screen: &pty_expect::Screen) {
    let raw = screen.raw();
    // Punctuation and operators must stay readable too, not just the named roles.
    for row in 0..raw.size().0 {
        for col in 0..raw.size().1 {
            let cell = raw.cell(row, col).unwrap();
            let bg = format!("{:?}", cell.bgcolor());
            if cell.contents().trim().is_empty() || bg == "Default" {
                continue;
            }
            let fg = format!("{:?}", cell.fgcolor());
            assert!(
                contrast(rgb(&fg), rgb(&bg)) >= 4.5,
                "unreadable code cell ({row}, {col}): {fg} on {bg}"
            );
        }
    }
}

#[test]
fn dark_truecolor_syntax_is_readable() {
    showcase(false, true, false);
}

#[test]
fn light_truecolor_syntax_is_readable() {
    showcase(true, true, false);
}

#[test]
fn dark_indexed_syntax_is_readable() {
    showcase(false, false, false);
}

#[test]
fn light_indexed_syntax_is_readable() {
    showcase(true, false, false);
}

#[test]
fn no_color_code_stays_plain() {
    showcase(false, true, true);
}
