//! Codex default diff colors on scode's existing inline tool cards.
//! Source: openai/codex b741e480, tui/src/diff_render.rs and style/contrast.rs.
//! Copyright 2025 OpenAI. Apache-2.0; see third_party/codex.

use crossterm::style::{Color, ContentStyle};

use super::{code_theme, color_math, styled_text::StyledText, ColorSupport, ColorTheme};

/// Highlight a complete hunk, preserving parser state across its source rows.
pub(crate) fn render_rows(rows: &[(char, &str)], language: &str) -> Vec<String> {
    let support = ColorSupport::detect();
    let light = ColorTheme::is_light_background();
    let code = rows
        .iter()
        .map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join("\n");
    // Contrast must be resolved before palette reduction, against the actual fill.
    let highlighted = if support == ColorSupport::NoColor {
        None
    } else {
        code_theme::highlight(&code, language, light, ColorSupport::TrueColor)
    };
    let mut offset = 0;
    rows.iter()
        .map(|(sign, text)| {
            let bg = background(*sign, light, support);
            let sign_color = match (sign, support) {
                (_, ColorSupport::NoColor) => None,
                ('+', _) => Some(Color::DarkGreen),
                ('-', _) => Some(Color::DarkRed),
                _ => None,
            };
            let mut output = StyledText::default();
            output.push(
                ContentStyle {
                    foreground_color: sign_color,
                    background_color: bg,
                    ..ContentStyle::default()
                },
                &format!("{sign} "),
            );
            if let Some(source) = &highlighted {
                for (mut style, content) in source.spans(offset..offset + text.len()) {
                    if let Some(Color::Rgb { r, g, b }) = style.foreground_color {
                        let background = bg.and_then(|bg| match bg {
                            Color::Rgb { r, g, b } => Some((r, g, b)),
                            Color::AnsiValue(index) => Some(color_math::xterm(index)),
                            _ => None,
                        });
                        style.foreground_color =
                            Some(color_math::foreground((r, g, b), background, support));
                    }
                    style.background_color = bg;
                    output.push(style, content);
                }
            } else {
                let foreground_color = if light
                    && matches!(support, ColorSupport::TrueColor | ColorSupport::Ansi256)
                {
                    None
                } else {
                    sign_color
                };
                output.push(
                    ContentStyle {
                        foreground_color,
                        background_color: bg,
                        ..ContentStyle::default()
                    },
                    text,
                );
            }
            offset += text.len() + 1;
            let mut ansi = String::new();
            output.write_ansi(0..output.text.len(), &mut ansi);
            ansi
        })
        .collect()
}

fn background(sign: char, light: bool, support: ColorSupport) -> Option<Color> {
    let rgb = match (sign, light) {
        ('+', false) => (33, 58, 43),
        ('-', false) => (74, 34, 29),
        ('+', true) => (218, 251, 225),
        ('-', true) => (255, 235, 233),
        _ => return None,
    };
    match support {
        ColorSupport::TrueColor => Some(Color::Rgb {
            r: rgb.0,
            g: rgb.1,
            b: rgb.2,
        }),
        ColorSupport::Ansi256 => Some(Color::AnsiValue(match (sign, light) {
            ('+', false) => 22,
            ('-', false) => 52,
            ('+', true) => 194,
            _ => 224,
        })),
        ColorSupport::Ansi16 | ColorSupport::NoColor => None,
    }
}
