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
        // Inline code keeps the syntax asset's exact foreground, just like
        // fenced code and Codex. Only UI metadata is softened/contrast-adjusted.
        if index == 0 {
            return foreground(color, support);
        }
        // Codex's status accents retain 85% saturation; no faint attribute.
        let luma =
            (77 * u16::from(color.r) + 150 * u16::from(color.g) + 29 * u16::from(color.b)) / 256;
        let soften = |channel| {
            u8::try_from((u16::from(channel) * 85 + luma * 15 + 50) / 100)
                .expect("softened channel")
        };
        let rgb = (soften(color.r), soften(color.g), soften(color.b));
        color_math::foreground(rgb, terminal_palette::background(), support)
    })
}

/// Table labels use the same syntax scopes as Codex's Markdown table header.
pub(super) fn table_header_color(light: bool, support: ColorSupport) -> Color {
    static DARK: OnceLock<Option<syntect::highlighting::Color>> = OnceLock::new();
    static LIGHT: OnceLock<Option<syntect::highlighting::Color>> = OnceLock::new();
    if matches!(support, ColorSupport::NoColor | ColorSupport::Ansi16) {
        return Color::Reset;
    }
    let color = if light { &LIGHT } else { &DARK }.get_or_init(|| {
        with_theme(light, |theme| {
            let highlighter = Highlighter::new(theme);
            ["entity.name.type", "support.type", "variable"]
                .iter()
                .find_map(|name| {
                    let scope = Scope::new(name).ok()?;
                    highlighter.style_mod_for_stack(&[scope]).foreground
                })
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
        || !within_limits(code)
    {
        return None;
    }
    let mut highlighter = HighlightLines::new(syntax(language)?, theme(light));
    highlight_chunk(&mut highlighter, code, support)
}

pub(super) fn within_limits(code: &str) -> bool {
    code.len() <= 512 * 1024
        && code.lines().count() <= 10_000
        && code.lines().all(|line| line.len() <= 4 * 1024)
}

/// Shared conversion for finalized blocks and append-only live code previews.
fn highlight_chunk(
    highlighter: &mut HighlightLines<'_>,
    code: &str,
    support: ColorSupport,
) -> Option<StyledText> {
    let mut output = StyledText::default();
    for line in LinesWithEndings::from(code) {
        for (style, text) in highlighter.highlight_line(line, syntax_set()).ok()? {
            let mut content_style = ContentStyle {
                foreground_color: Some(foreground(style.foreground, support)),
                ..ContentStyle::default()
            };
            if style.font_style.contains(FontStyle::BOLD) {
                content_style.attributes.set(Attribute::Bold);
            }
            output.push(content_style, text);
        }
    }
    Some(output)
}

/// A Send handle to one bounded parser cache on the painting thread.
/// Oniguruma's capture region is not Send. Keep it where it is used rather
/// than moving parser internals through the shared UI context or using unsafe.
pub(super) struct StreamingCodeHighlighter {
    id: u64,
    language: String,
    light: bool,
    support: ColorSupport,
}

struct CachedParser {
    id: u64,
    bytes: usize,
    parser: HighlightLines<'static>,
}
thread_local! {
    static STREAM_PARSER: std::cell::RefCell<Option<CachedParser>> = const { std::cell::RefCell::new(None) };
}

impl StreamingCodeHighlighter {
    pub(super) fn new(language: &str, light: bool, support: ColorSupport) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            language: language.to_string(),
            light,
            support,
        }
    }

    pub(super) fn append(&mut self, prefix: &str, added: &str) -> Option<StyledText> {
        if matches!(self.support, ColorSupport::NoColor | ColorSupport::Ansi16) {
            return None;
        }
        STREAM_PARSER.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache
                .as_ref()
                .is_none_or(|cache| cache.id != self.id || cache.bytes != prefix.len())
            {
                let mut parser = HighlightLines::new(syntax(&self.language)?, theme(self.light));
                // Reconstruct only after another context/thread owned the one cache slot.
                for line in LinesWithEndings::from(prefix) {
                    parser.highlight_line(line, syntax_set()).ok()?;
                }
                *cache = Some(CachedParser {
                    id: self.id,
                    bytes: prefix.len(),
                    parser,
                });
            }
            let cache = cache.as_mut()?;
            let output = highlight_chunk(&mut cache.parser, added, self.support)?;
            cache.bytes += added.len();
            Some(output)
        })
    }
}
impl Drop for StreamingCodeHighlighter {
    fn drop(&mut self) {
        let _ = STREAM_PARSER.try_with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.as_ref().is_some_and(|cache| cache.id == self.id) {
                *cache = None;
            }
        });
    }
}
