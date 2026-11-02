//! Code colors follow openai/codex at b741e480e203f037ca726bc2a76d99a8e8668e66:
//! tui/src/render/highlight.rs and tui/src/markdown_render.rs. Both renderers
//! use the same two-face grammar/theme bundle; only the terminal adapter differs.
//! This is an internal syntax adapter and asset cache, not another UI theme.
//! Semantic UI color selection belongs to `ColorTheme`. Metadata scope selection
//! and softening follow Codex a09812981d, tui/src/bottom_pane/status_line_style.rs.
//! See third_party/codex for the upstream Apache-2.0 license.

use std::sync::OnceLock;

use crossterm::style::{Attribute, Color, ContentStyle};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter, Theme};
use syntect::parsing::{Scope, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use super::styled_text::StyledText;
use super::{color_math, terminal_palette, ColorSupport, TextRole};

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn with_theme<T>(light: bool, read: impl FnOnce(&Theme) -> T) -> T {
    let themes = two_face::theme::extra();
    read(themes.get(if light {
        EmbeddedThemeName::CatppuccinLatte
    } else {
        EmbeddedThemeName::CatppuccinMocha
    }))
}

pub(super) fn theme(light: bool) -> &'static Theme {
    // Keep the selected syntax asset, not every unused serialized theme.
    static DARK: OnceLock<Theme> = OnceLock::new();
    static LIGHT: OnceLock<Theme> = OnceLock::new();
    let selected = if light { &LIGHT } else { &DARK };
    selected.get_or_init(|| with_theme(light, Clone::clone))
}

/// UI accents come from the same syntax asset, resolved together without
/// retaining an unused grammar catalog or every embedded theme in memory.
pub(super) fn role_color(role: TextRole, light: bool, support: ColorSupport) -> Color {
    type Accents = [Option<syntect::highlighting::Color>; 5];
    static DARK: OnceLock<Accents> = OnceLock::new();
    static LIGHT: OnceLock<Accents> = OnceLock::new();

    let (index, fallback) = match role {
        TextRole::Code => (0, Color::Reset),
        TextRole::Model => (1, Color::DarkCyan),
        TextRole::Path => (2, Color::DarkGreen),
        TextRole::Usage => (3, Color::DarkGreen),
        TextRole::Branch => (4, Color::DarkBlue),
        _ => unreachable!("only syntax-backed roles enter the syntax adapter"),
    };
    if support == ColorSupport::NoColor {
        return Color::Reset;
    }
    if support == ColorSupport::Ansi16 {
        return fallback;
    }
    let colors = if light { &LIGHT } else { &DARK }.get_or_init(|| {
        with_theme(light, |theme| {
            let highlighter = Highlighter::new(theme);
            let scopes: [&[&str]; 5] = [
                &[
                    "markup.inline.raw.string.markdown",
                    "markup.raw.inline.markdown",
                ],
                &["entity.name.type", "support.type", "variable"],
                &["string", "markup.underline.link"],
                &["constant.numeric", "constant"],
                &["entity.name.function", "entity.name.tag"],
            ];
            scopes.map(|scopes| {
                scopes.iter().find_map(|name| {
                    let scope = Scope::new(name).ok()?;
                    highlighter.style_mod_for_stack(&[scope]).foreground
                })
            })
        })
    });
    colors[index].map_or(fallback, |color| {
        let rgb = if index == 0 {
            (color.r, color.g, color.b)
        } else {
            // Codex's status accents retain 85% saturation; no faint attribute.
            let luma =
                (77 * u16::from(color.r) + 150 * u16::from(color.g) + 29 * u16::from(color.b))
                    / 256;
            let soften = |channel| {
                u8::try_from((u16::from(channel) * 85 + luma * 15 + 50) / 100)
                    .expect("softened channel")
            };
            (soften(color.r), soften(color.g), soften(color.b))
        };
        color_math::foreground(rgb, terminal_palette::background(), support)
    })
}

fn foreground(color: syntect::highlighting::Color, support: ColorSupport) -> Color {
    if support == ColorSupport::Ansi256 {
        return Color::AnsiValue(super::rgb_to_ansi256(color.r, color.g, color.b));
    }
    color_math::foreground((color.r, color.g, color.b), None, support)
}

fn syntax(language: &str) -> Option<&'static SyntaxReference> {
    // CommonMark info strings can include metadata after the language token.
    let language = language.split([',', ' ', '\t']).next().unwrap_or("");
    let syntaxes = syntax_set();
    let lower = language.to_ascii_lowercase();
    let token = match lower.as_str() {
        "csharp" | "c-sharp" => "c#",
        "cu" | "cuh" | "cppm" | "cxxm" | "ixx" => "cpp",
        "golang" => "go",
        "python3" => "python",
        "shell" => "bash",
        _ => language,
    };
    syntaxes
        .find_syntax_by_token(token)
        .or_else(|| syntaxes.find_syntax_by_name(token))
        .or_else(|| {
            syntaxes
                .syntaxes()
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(token))
        })
        .or_else(|| syntaxes.find_syntax_by_extension(language))
}

/// Highlight an entire block so multiline strings/comments retain parser state.
/// Unknown languages and inputs beyond Codex's limits stay unstyled.
pub(super) fn highlight(
    code: &str,
    language: &str,
    light: bool,
    support: ColorSupport,
) -> Option<StyledText> {
    if matches!(support, ColorSupport::NoColor | ColorSupport::Ansi16)
        || code.is_empty()
        || code.len() > 512 * 1024
        || code.lines().count() > 10_000
        || code.lines().any(|line| line.len() > 4 * 1024)
    {
        return None;
    }
    let mut highlighter = HighlightLines::new(syntax(language)?, theme(light));
    let mut output = StyledText::default();
    for line in LinesWithEndings::from(code) {
        for (style, text) in highlighter.highlight_line(line, syntax_set()).ok()? {
            let mut content_style = ContentStyle {
                foreground_color: Some(foreground(style.foreground, support)),
                ..ContentStyle::default()
            };
            // Codex retains bold, but deliberately omits syntax italics,
            // underlines and the theme background in terminal code blocks.
            if style.font_style.contains(FontStyle::BOLD) {
                content_style.attributes.set(Attribute::Bold);
            }
            output.push(content_style, text);
        }
    }
    Some(output)
}
