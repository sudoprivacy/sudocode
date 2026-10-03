//! Code colors follow openai/codex at b741e480e203f037ca726bc2a76d99a8e8668e66:
//! tui/src/render/highlight.rs and tui/src/markdown_render.rs. Both renderers
//! use the same two-face grammar/theme bundle; only the terminal adapter differs.

use std::sync::OnceLock;

use crossterm::style::{Attribute, Color, ContentStyle};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter, Theme};
use syntect::parsing::{Scope, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use two_face::theme::EmbeddedThemeName;

use super::styled_text::StyledText;
use super::{color_math, ColorSupport};

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

pub(super) fn theme(light: bool) -> &'static Theme {
    static THEMES: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();
    THEMES.get_or_init(two_face::theme::extra).get(if light {
        EmbeddedThemeName::CatppuccinLatte
    } else {
        EmbeddedThemeName::CatppuccinMocha
    })
}

/// Inline code uses the Markdown scope from the same theme as fenced code.
pub(super) fn inline_color(light: bool, support: ColorSupport) -> Color {
    static DARK: OnceLock<Option<syntect::highlighting::Color>> = OnceLock::new();
    static LIGHT: OnceLock<Option<syntect::highlighting::Color>> = OnceLock::new();
    if matches!(support, ColorSupport::NoColor | ColorSupport::Ansi16) {
        return Color::Reset;
    }
    let color = if light { &LIGHT } else { &DARK }.get_or_init(|| {
        let highlighter = Highlighter::new(theme(light));
        [
            "markup.inline.raw.string.markdown",
            "markup.raw.inline.markdown",
        ]
        .iter()
        .find_map(|name| {
            let scope = Scope::new(name).ok()?;
            highlighter.style_mod_for_stack(&[scope]).foreground
        })
    });
    color.map_or(Color::Reset, |color| foreground(color, support))
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
