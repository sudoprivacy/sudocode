//! Shared UI and syntax palette. Color choices live here; renderers consume roles.
//!
//! Syntax colors use the fixed xterm 256-color cube/ramp so truecolor and
//! indexed output have the same contrast. Code text, including comments,
//! targets at least 4.5:1 against each built-in code background. The terminal
//! still owns its surrounding background and may customize its palette.

use crossterm::style::Color;
use syntect::highlighting::{
    Color as SyntaxColor, FontStyle, StyleModifier, Theme, ThemeItem, ThemeSettings,
};

use super::ansi_fg;

/// Syntax roles in the fixed xterm palette (indices 16–255).
/// Shared hues are assigned from the same tokens as UI chrome in each palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxPalette {
    pub foreground: u8,
    pub comment: u8,
    pub keyword: u8,
    pub string: u8,
    pub constant: u8,
    pub function: u8,
    pub type_name: u8,
    pub punctuation: u8,
    pub invalid: u8,
}

impl SyntaxPalette {
    /// Build once with the lazily loaded syntax resources, never per frame.
    pub(super) fn to_syntect(self, background: u8) -> Theme {
        let roles = [
            ("comment", self.comment),
            ("string", self.string),
            ("constant, support.constant", self.constant),
            ("keyword, storage, entity.name.tag", self.keyword),
            (
                "entity.name.function, support.function, entity.other.attribute-name",
                self.function,
            ),
            (
                "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class",
                self.type_name,
            ),
            ("punctuation, keyword.operator", self.punctuation),
            ("punctuation.definition.string", self.string),
            ("markup.inserted", self.string),
            ("markup.deleted", self.invalid),
            ("meta.diff.header, markup.heading", self.function),
            ("invalid", self.invalid),
        ];
        Theme {
            name: Some("Sudo Code semantic palette".into()),
            settings: ThemeSettings {
                foreground: Some(syntax_color(self.foreground)),
                background: Some(syntax_color(background)),
                ..ThemeSettings::default()
            },
            scopes: roles
                .into_iter()
                .map(|(selector, index)| ThemeItem {
                    scope: selector.parse().expect("built-in syntax selector"),
                    style: StyleModifier {
                        foreground: Some(syntax_color(index)),
                        // Keep emphasis consistent in truecolor and 256-color output.
                        font_style: Some(FontStyle::empty()),
                        ..StyleModifier::default()
                    },
                })
                .collect(),
            ..Theme::default()
        }
    }
}

/// Expand fixed palette values exactly, without depending on the user's ANSI
/// 0–15 colors. These values also round-trip through the indexed output path.
fn syntax_color(index: u8) -> SyntaxColor {
    assert!(index >= 16, "syntax colors must use the fixed palette");
    let (r, g, b) = if index >= 232 {
        let value = 8 + (index - 232) * 10;
        (value, value, value)
    } else {
        let cube = [0, 95, 135, 175, 215, 255];
        let n = usize::from(index - 16);
        (cube[n / 36], cube[n / 6 % 6], cube[n % 6])
    };
    SyntaxColor { r, g, b, a: 255 }
}

/// Semantic color theme — coder picks a scenario token, never a raw color.
///
/// Two built-in palettes: `dark()` (default) and `light()` for light
/// terminal backgrounds.  All rendering code references `theme.xxx`;
/// switching palette changes every color at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    /// Syntax colors share the UI palette and its background selection.
    pub syntax: SyntaxPalette,
    // ── Semantic tokens ──────────────────────────────────────────────
    /// Primary emphasis — prompt glyph, H1 heading, box title.
    pub primary: Color,
    /// Success — tool done, write result, spinner complete.
    pub success: Color,
    /// Error — failed, denied, cancelled.
    pub error: Color,
    /// Warning — permission prompt title, edit label, stalled spinner.
    pub warning: Color,
    /// Info — spinner active, thinking indicator.
    pub info: Color,
    /// Muted — footer hints, dim text, labels, separators.
    pub muted: Color,
    /// Emphasis — italic text in markdown.
    pub emphasis: Color,
    /// Strong — bold text in markdown.
    pub strong: Color,
    /// Link — hyperlinks.
    pub link: Color,
    /// Inline code — backtick spans.
    pub code: Color,
    /// Code block background (256-color index).
    pub code_bg: u8,
    /// Border — box/table/code fence chrome.
    pub border: Color,
    /// Diff added line.
    pub diff_added: Color,
    /// Diff removed line.
    pub diff_removed: Color,
    /// Hook feedback text.
    pub hook_feedback: Color,
    /// Logo primary color.
    pub logo: Color,
    /// Logo accent ("Code" wordmark).
    pub logo_accent: Color,
    /// Blockquote prefix.
    pub quote: Color,
    /// H2 heading.
    pub heading_h2: Color,
    /// H3 heading.
    pub heading_h3: Color,
    /// H4+ heading.
    pub heading_h4: Color,
}

impl ColorTheme {
    // ── Sudoprivacy VI palette ─────────────────────────────────────
    //
    // Brand colors (from sudowork.sudoprivacy.com):
    //   amber  #F59E0B  → ANSI 214  primary brand touch-point
    //   teal   #0D9488  → ANSI 36   secondary brand touch-point
    //
    // Semantic colors (industry standard, not brand-specific):
    //   success = green, error = red, diff = green/red
    //
    // Derived:
    //   warning = yellow/ochre; links = blue; inline code = violet
    //   muted = readable grey; decorative borders stay subordinate to text
    // Light mode deepens amber/teal instead of reusing low-contrast brand swatches.

    /// Dark terminal background (default).
    #[must_use]
    pub fn dark() -> Self {
        let primary = 214;
        let muted = 247;
        let link = 75;
        let code = 177;
        Self {
            syntax: SyntaxPalette {
                foreground: 252,
                comment: muted,
                keyword: primary,
                string: 108,
                constant: code,
                function: link,
                type_name: code,
                punctuation: muted,
                invalid: 210,
            },
            primary: Color::AnsiValue(primary),  // amber #F59E0B
            success: Color::Green,               // semantic
            error: Color::Red,                   // semantic
            warning: Color::AnsiValue(220),      // yellow, distinct from info/success
            info: Color::AnsiValue(36),          // teal #0D9488
            muted: Color::AnsiValue(muted),      // readable muted grey
            emphasis: Color::AnsiValue(primary), // amber (italic text)
            strong: Color::White,                // bold text
            link: Color::AnsiValue(link),        // blue
            code: Color::AnsiValue(code),        // violet
            code_bg: 236,                        // dark grey bg
            border: Color::AnsiValue(248),       // light grey
            diff_added: Color::AnsiValue(70),    // semantic green
            diff_removed: Color::AnsiValue(203), // semantic red
            hook_feedback: Color::AnsiValue(primary), // amber
            logo: Color::AnsiValue(primary),     // amber
            logo_accent: Color::AnsiValue(36),   // teal
            quote: Color::AnsiValue(muted),      // grey
            heading_h2: Color::White,
            heading_h3: Color::AnsiValue(36),    // teal
            heading_h4: Color::AnsiValue(muted), // grey
        }
    }

    /// Light terminal background.
    #[must_use]
    pub fn light() -> Self {
        let primary = 94;
        let muted = 241;
        let link = 25;
        let code = 90;
        Self {
            syntax: SyntaxPalette {
                foreground: 236,
                comment: muted,
                keyword: primary,
                string: 22,
                constant: code,
                function: link,
                type_name: code,
                punctuation: muted,
                invalid: 124,
            },
            primary: Color::AnsiValue(primary), // deep amber, readable on light backgrounds
            success: Color::DarkGreen,          // semantic
            error: Color::DarkRed,              // semantic
            warning: Color::AnsiValue(130),     // ochre on a light background
            info: Color::AnsiValue(23),         // deep teal
            muted: Color::AnsiValue(muted),     // readable muted grey
            emphasis: Color::AnsiValue(primary), // darker amber
            strong: Color::Black,               // bold text
            link: Color::AnsiValue(link),       // dark blue
            code: Color::AnsiValue(code),       // dark violet
            code_bg: 255,                       // light grey bg
            border: Color::AnsiValue(muted),    // light grey
            diff_added: Color::AnsiValue(22),   // semantic dark green
            diff_removed: Color::AnsiValue(124), // semantic dark red
            hook_feedback: Color::AnsiValue(primary), // darker amber
            logo: Color::AnsiValue(primary),    // darker amber
            logo_accent: Color::AnsiValue(23),  // deep teal
            quote: Color::AnsiValue(muted),     // grey
            heading_h2: Color::Black,
            heading_h3: Color::AnsiValue(23),    // deep teal
            heading_h4: Color::AnsiValue(muted), // grey
        }
    }

    /// Detect terminal background and pick the appropriate palette.
    #[must_use]
    pub fn detect() -> Self {
        if Self::is_light_background() {
            Self::light()
        } else {
            Self::dark()
        }
    }

    /// ANSI escape for the border color.
    #[inline]
    pub fn border_fg(&self) -> String {
        ansi_fg(self.border)
    }

    /// ANSI escape for the muted color.
    #[inline]
    pub fn muted_fg(&self) -> String {
        ansi_fg(self.muted)
    }

    /// ANSI escape for the code block background.
    #[inline]
    pub fn code_bg_seq(&self) -> String {
        format!("\x1b[48;5;{}m", self.code_bg)
    }

    fn is_light_background() -> bool {
        // Check COLORFGBG (format: "fg;bg", light if bg >= 8).
        if let Ok(val) = std::env::var("COLORFGBG") {
            if let Some(bg) = val.rsplit(';').next().and_then(|s| s.parse::<u8>().ok()) {
                return bg > 8; // 8 = dark grey, not light
            }
        }
        false
    }
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self::detect()
    }
}

/// Process-wide theme, detected once for both chrome and syntax highlighting.
static THEME: std::sync::OnceLock<ColorTheme> = std::sync::OnceLock::new();

#[inline]
pub fn theme() -> &'static ColorTheme {
    THEME.get_or_init(ColorTheme::detect)
}
