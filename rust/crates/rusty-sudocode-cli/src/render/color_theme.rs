//! UI palette and terminal background selection. Code colors use Codex's
//! bundled Catppuccin themes through the sibling code_theme module.

use super::{ansi_fg, terminal_palette};
use crossterm::style::Color;

/// Semantic color theme — coder picks a scenario token, never a raw color.
///
/// Two built-in palettes: `dark()` (default) and `light()` for light
/// terminal backgrounds.  All rendering code references `theme.xxx`;
/// switching palette changes every color at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
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
    /// Selects the adaptive default syntax theme.
    pub light_background: bool,
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
    //   warning = yellow/ochre; links = blue; code uses Catppuccin
    //   muted = readable grey; decorative borders stay subordinate to text
    // Light mode deepens amber/teal instead of reusing low-contrast brand swatches.

    /// Dark terminal background (default).
    #[must_use]
    pub fn dark() -> Self {
        let primary = 214;
        let muted = 247;
        let link = 75;
        Self {
            primary: Color::AnsiValue(primary),  // amber #F59E0B
            success: Color::Green,               // semantic
            error: Color::Red,                   // semantic
            warning: Color::AnsiValue(220),      // yellow, distinct from info/success
            info: Color::AnsiValue(36),          // teal #0D9488
            muted: Color::AnsiValue(muted),      // readable muted grey
            emphasis: Color::AnsiValue(primary), // amber (italic text)
            strong: Color::White,                // bold text
            link: Color::AnsiValue(link),        // blue
            light_background: false,
            code_bg: 236,                             // dark grey bg
            border: Color::AnsiValue(248),            // light grey
            diff_added: Color::AnsiValue(70),         // semantic green
            diff_removed: Color::AnsiValue(203),      // semantic red
            hook_feedback: Color::AnsiValue(primary), // amber
            logo: Color::AnsiValue(primary),          // amber
            logo_accent: Color::AnsiValue(36),        // teal
            quote: Color::AnsiValue(muted),           // grey
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
        Self {
            primary: Color::AnsiValue(primary), // deep amber, readable on light backgrounds
            success: Color::DarkGreen,          // semantic
            error: Color::DarkRed,              // semantic
            warning: Color::AnsiValue(130),     // ochre on a light background
            info: Color::AnsiValue(23),         // deep teal
            muted: Color::AnsiValue(muted),     // readable muted grey
            emphasis: Color::AnsiValue(primary), // darker amber
            strong: Color::Black,               // bold text
            link: Color::AnsiValue(link),       // dark blue
            light_background: true,
            code_bg: 255,                             // light grey bg
            border: Color::AnsiValue(muted),          // light grey
            diff_added: Color::AnsiValue(22),         // semantic dark green
            diff_removed: Color::AnsiValue(124),      // semantic dark red
            hook_feedback: Color::AnsiValue(primary), // darker amber
            logo: Color::AnsiValue(primary),          // darker amber
            logo_accent: Color::AnsiValue(23),        // deep teal
            quote: Color::AnsiValue(muted),           // grey
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

    pub(super) fn is_light_background() -> bool {
        if let Some(background) = terminal_palette::background() {
            return terminal_palette::is_light(background);
        }
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

/// Process-wide UI theme, detected once after the startup palette probe.
static THEME: std::sync::OnceLock<ColorTheme> = std::sync::OnceLock::new();

#[inline]
pub fn theme() -> &'static ColorTheme {
    THEME.get_or_init(ColorTheme::detect)
}
