//! Shared UI colors and the Codex-compatible Markdown syntax theme.
//!
//! Brand accents remain amber. Transcript syntax and inline-code colors come
//! from the same bundled Catppuccin assets as Codex, without local overrides.

use crossterm::style::Color;

use super::{ansi_fg, code_theme, terminal_palette, ColorSupport};

/// Tool frames carry execution status without competing with their content.
pub struct ToolBorderColors {
    pub running: Color,
    pub success: Color,
    pub error: Color,
}

/// Semantic color theme — coder picks a scenario token, never a raw color.
///
/// Two built-in palettes: `dark()` (default) and `light()` for light
/// terminal backgrounds.  All rendering code references `theme.xxx`;
/// switching palette changes every color at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    /// Selects the adaptive default syntax theme in code_theme.
    pub light_background: bool,
    // ── Semantic tokens ──────────────────────────────────────────────
    /// Brand emphasis — prompt glyph, logo and box title.
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
    /// Link — hyperlinks.
    pub link: Color,
    /// Shell-mode background (256-color index); Markdown code stays transparent.
    pub code_bg: u8,
    /// Border — box/table chrome.
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
    /// Ordered Markdown list markers use terminal ANSI bright blue.
    pub ordered_list_marker: Color,
    /// Markdown blockquote text and prefix (ANSI green, as in Codex).
    pub quote: Color,
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
    // Transcript links use the Codex blue accent; inline code follows its syntax theme.
    // Muted chrome remains readable without applying the terminal dim attribute.
    // Light mode deepens amber/teal instead of reusing low-contrast brand swatches.

    /// Dark terminal background (default).
    #[must_use]
    pub fn dark() -> Self {
        let primary = 214;
        let muted = 247;
        Self {
            light_background: false,
            primary: Color::AnsiValue(primary), // amber #F59E0B
            success: Color::Green,              // semantic
            error: Color::Red,                  // semantic
            warning: Color::AnsiValue(220),     // yellow, distinct from info/success
            info: Color::AnsiValue(36),         // teal #0D9488
            muted: Color::AnsiValue(muted),     // readable muted grey
            link: Color::Rgb {
                r: 99,
                g: 168,
                b: 248,
            }, // blue
            code_bg: 236,                       // dark grey bg
            border: Color::AnsiValue(248),      // light grey
            diff_added: Color::AnsiValue(70),   // semantic green
            diff_removed: Color::AnsiValue(203), // semantic red
            hook_feedback: Color::AnsiValue(primary), // amber
            logo: Color::AnsiValue(primary),    // amber
            logo_accent: Color::AnsiValue(36),  // teal
            ordered_list_marker: Color::Blue,
            quote: Color::DarkGreen, // terminal green
        }
    }

    /// Light terminal background.
    #[must_use]
    pub fn light() -> Self {
        let primary = 94;
        let muted = 241;
        Self {
            light_background: true,
            primary: Color::AnsiValue(primary), // deep amber, readable on light backgrounds
            success: Color::DarkGreen,          // semantic
            error: Color::DarkRed,              // semantic
            warning: Color::AnsiValue(130),     // ochre on a light background
            info: Color::AnsiValue(23),         // deep teal
            muted: Color::AnsiValue(muted),     // readable muted grey
            link: Color::Rgb {
                r: 28,
                g: 100,
                b: 200,
            }, // dark blue
            code_bg: 255,                       // light grey bg
            border: Color::AnsiValue(muted),    // light grey
            diff_added: Color::AnsiValue(22),   // semantic dark green
            diff_removed: Color::AnsiValue(124), // semantic dark red
            hook_feedback: Color::AnsiValue(primary), // darker amber
            logo: Color::AnsiValue(primary),    // darker amber
            logo_accent: Color::AnsiValue(23),  // deep teal
            ordered_list_marker: Color::Blue,
            quote: Color::DarkGreen, // terminal green
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

    /// Resolve tool-specific roles from the shared brand and syntax palettes.
    /// Syntax colors are loaded lazily, only when a tool card needs them.
    pub fn tool_borders(&self) -> ToolBorderColors {
        let support = ColorSupport::detect();
        ToolBorderColors {
            running: support.color(self.primary),
            success: if support == ColorSupport::Ansi16 {
                self.success
            } else {
                code_theme::inline_color(self.light_background, support)
            },
            error: support.color(self.error),
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

    /// ANSI escape for the shell-mode background.
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

/// Process-wide theme, detected once for both chrome and syntax highlighting.
static THEME: std::sync::OnceLock<ColorTheme> = std::sync::OnceLock::new();

#[inline]
pub fn theme() -> &'static ColorTheme {
    THEME.get_or_init(ColorTheme::detect)
}
