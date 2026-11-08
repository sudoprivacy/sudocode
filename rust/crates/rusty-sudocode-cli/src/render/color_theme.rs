//! Semantic terminal styles. Syntax assets supply prose, code and metadata accents;
//! terminal-owned ANSI colors supply status, and amber identifies the brand.
//! Markup scopes follow Codex 079e17a4's MarkdownStyles; renderers consume roles.

use super::{ansi_fg, code_theme, color_math, terminal_palette, ColorSupport};
use crossterm::style::{Attribute, Color, ContentStyle};
use std::sync::OnceLock;

/// Choose a meaning at the call site; palette and capability decisions live here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum TextRole {
    Body,
    Reasoning,
    ProseStrong,
    Emphasis,
    Strikethrough,
    Heading1,
    Heading2,
    Heading3,
    Heading4,
    Heading5,
    Heading6,
    Link,
    Strong,
    Secondary,
    Border,
    Brand,
    Accent,
    Info,
    Success,
    Error,
    Warning,
    Notice,
    Code,
    TableHeader,
    Quote,
    OrderedListMarker,
    UnorderedListMarker,
    DiagramNode,
    DiagramEdge,
    Model,
    Path,
    Usage,
    Branch,
    Added,
    Removed,
}
const ROLE_COUNT: usize = TextRole::Removed as usize + 1;
type Rgb = (u8, u8, u8);
type ScopeForegrounds = [OnceLock<Option<Rgb>>; ROLE_COUNT];

/// One adaptive theme shared by transcript, input, status and syntax rendering.
/// Each role resolves once per palette/capability, outside subsequent redraws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTheme {
    pub light_background: bool,
}

impl ColorTheme {
    /// Sudoprivacy's registered amber, #F59E0B. Light surfaces use a readable shade.
    const AMBER: (u8, u8, u8) = (245, 158, 11);

    #[must_use]
    pub const fn dark() -> Self {
        Self {
            light_background: false,
        }
    }
    #[must_use]
    pub const fn light() -> Self {
        Self {
            light_background: true,
        }
    }
    #[must_use]
    pub fn detect() -> Self {
        if Self::is_light_background() {
            Self::light()
        } else {
            Self::dark()
        }
    }

    #[inline]
    pub fn style(self, role: TextRole) -> ContentStyle {
        self.style_for(role, ColorSupport::detect())
    }

    pub fn style_for(self, role: TextRole, support: ColorSupport) -> ContentStyle {
        static STYLES: [[OnceLock<ContentStyle>; ROLE_COUNT]; 8] =
            [const { [const { OnceLock::new() }; ROLE_COUNT] }; 8];
        let tier = match support {
            ColorSupport::NoColor => 0,
            ColorSupport::Ansi16 => 1,
            ColorSupport::Ansi256 => 2,
            ColorSupport::TrueColor => 3,
        } + usize::from(self.light_background) * 4;
        *STYLES[tier][role as usize].get_or_init(|| {
            let mut style = ContentStyle::default();
            if support != ColorSupport::NoColor {
                let color = self.resolve_color(role, support);
                if color != Color::Reset {
                    style.foreground_color = Some(color);
                }
            }
            if matches!(
                role,
                TextRole::ProseStrong
                    | TextRole::Strong
                    | TextRole::TableHeader
                    | TextRole::Heading1
                    | TextRole::Heading2
                    | TextRole::Heading3
            ) {
                style.attributes.set(Attribute::Bold);
            }
            if matches!(
                role,
                TextRole::Emphasis
                    | TextRole::Heading3
                    | TextRole::Heading4
                    | TextRole::Heading5
                    | TextRole::Heading6
            ) {
                style.attributes.set(Attribute::Italic);
            }
            if matches!(role, TextRole::Heading1 | TextRole::Link) {
                style.attributes.set(Attribute::Underlined);
            }
            if role == TextRole::Strikethrough {
                style.attributes.set(Attribute::CrossedOut);
            }
            if role == TextRole::Reasoning {
                style.attributes.set(Attribute::Dim);
            }
            style
        })
    }

    /// Inner semantic foregrounds override outer ones; modifiers accumulate.
    /// The parser composes only when entering a tag, not for each text token.
    #[inline]
    pub(crate) fn overlay(
        self,
        mut base: ContentStyle,
        role: TextRole,
        support: ColorSupport,
    ) -> ContentStyle {
        let overlay = self.style_for(role, support);
        if overlay.foreground_color.is_some() {
            base.foreground_color = overlay.foreground_color;
        }
        if overlay.background_color.is_some() {
            base.background_color = overlay.background_color;
        }
        if overlay.underline_color.is_some() {
            base.underline_color = overlay.underline_color;
        }
        base.attributes.extend(overlay.attributes);
        base
    }

    pub(crate) fn heading_style(
        self,
        level: pulldown_cmark::HeadingLevel,
        support: ColorSupport,
        base: ContentStyle,
    ) -> ContentStyle {
        use pulldown_cmark::HeadingLevel;
        self.overlay(
            base,
            match level {
                HeadingLevel::H1 => TextRole::Heading1,
                HeadingLevel::H2 => TextRole::Heading2,
                HeadingLevel::H3 => TextRole::Heading3,
                HeadingLevel::H4 => TextRole::Heading4,
                HeadingLevel::H5 => TextRole::Heading5,
                HeadingLevel::H6 => TextRole::Heading6,
            },
            support,
        )
    }

    #[inline]
    pub fn color(self, role: TextRole) -> Color {
        self.color_for(role, ColorSupport::detect())
    }
    #[inline]
    pub fn color_for(self, role: TextRole, support: ColorSupport) -> Color {
        self.style_for(role, support)
            .foreground_color
            .unwrap_or(Color::Reset)
    }

    /// The brand amber before palette reduction; light surfaces use a readable shade.
    const fn brand_rgb(self) -> Rgb {
        if self.light_background {
            (148, 93, 5)
        } else {
            Self::AMBER
        }
    }

    /// The quiet point of a running tool card's frame pulse: the brand amber
    /// half faded into the painted background. `None` where the palette has no
    /// such shade (16 colors or none), so those frames stay steady.
    pub(crate) fn brand_pulse_low(self, support: ColorSupport) -> Option<Color> {
        let surface = terminal_palette::background().unwrap_or(if self.light_background {
            (255, 255, 255)
        } else {
            (0, 0, 0)
        });
        color_math::faded(self.brand_rgb(), surface, support)
    }

    fn resolve_color(self, role: TextRole, support: ColorSupport) -> Color {
        let limited = support == ColorSupport::Ansi16;
        let readable = |rgb| color_math::foreground(rgb, terminal_palette::background(), support);
        match role {
            TextRole::Body | TextRole::Strong | TextRole::Reasoning | TextRole::Strikethrough => {
                Color::Reset
            }
            TextRole::Brand => {
                if limited {
                    Color::DarkYellow
                } else {
                    readable(self.brand_rgb())
                }
            }
            TextRole::Accent => {
                if limited {
                    Color::DarkBlue
                } else {
                    readable(if self.light_background {
                        (28, 100, 200)
                    } else {
                        (99, 168, 248)
                    })
                }
            }
            // Regular terminal red/green slots, as in Codex, preserve the user's
            // palette instead of imposing a bright or hardcoded RGB status color.
            TextRole::Error | TextRole::Removed => Color::DarkRed,
            TextRole::Success | TextRole::Added => Color::DarkGreen,
            TextRole::Warning => {
                if self.light_background {
                    Color::Reset
                } else {
                    Color::DarkYellow
                }
            }
            TextRole::Notice => {
                if limited {
                    Color::DarkYellow
                } else {
                    readable(if self.light_background {
                        (139, 98, 20)
                    } else {
                        (196, 167, 103)
                    })
                }
            }
            TextRole::Secondary | TextRole::Border => {
                if limited {
                    Color::Reset
                } else {
                    let preferred = match (
                        terminal_palette::foreground(),
                        terminal_palette::background(),
                    ) {
                        (Some(fg), Some(bg)) => {
                            let blend = |a, b| {
                                u8::try_from((u16::from(a) * 60 + u16::from(b) * 40) / 100)
                                    .expect("blended channel")
                            };
                            (blend(fg.0, bg.0), blend(fg.1, bg.1), blend(fg.2, bg.2))
                        }
                        _ if self.light_background => (118, 118, 118),
                        _ => (158, 158, 158),
                    };
                    readable(preferred)
                }
            }
            TextRole::ProseStrong
            | TextRole::Emphasis
            | TextRole::Heading1
            | TextRole::Heading2
            | TextRole::Heading3
            | TextRole::Heading4
            | TextRole::Heading5
            | TextRole::Heading6
            | TextRole::Link
            | TextRole::Quote
            | TextRole::OrderedListMarker
            | TextRole::UnorderedListMarker
            | TextRole::DiagramNode
            | TextRole::DiagramEdge
            | TextRole::Code
            | TextRole::Info
            | TextRole::TableHeader
            | TextRole::Model
            | TextRole::Path
            | TextRole::Usage
            | TextRole::Branch => self.syntax_color(role, support),
        }
    }

    /// All TextMate scope-to-role decisions live beside UI palette policy.
    fn syntax_scopes(role: TextRole) -> &'static [&'static str] {
        match role {
            TextRole::Code | TextRole::Info => &[
                "markup.inline.raw.string.markdown",
                "markup.raw.inline.markdown",
            ],
            TextRole::ProseStrong => &["markup.bold.markdown"],
            TextRole::Emphasis => &["markup.italic.markdown"],
            TextRole::Heading1 => &["markup.heading.1.markdown", "markup.heading.markdown"],
            TextRole::Heading2 => &["markup.heading.2.markdown", "markup.heading.markdown"],
            TextRole::Heading3 => &["markup.heading.3.markdown", "markup.heading.markdown"],
            TextRole::Heading4 => &["markup.heading.4.markdown", "markup.heading.markdown"],
            TextRole::Heading5 => &["markup.heading.5.markdown", "markup.heading.markdown"],
            TextRole::Heading6 => &["markup.heading.6.markdown", "markup.heading.markdown"],
            TextRole::Link => &["markup.underline.link.markdown"],
            TextRole::Quote => &["markup.quote.markdown"],
            TextRole::OrderedListMarker => &["markup.list.numbered.markdown"],
            TextRole::UnorderedListMarker => &[
                "markup.list.unnumbered.markdown",
                "markup.list.bullet.markdown",
            ],
            TextRole::TableHeader | TextRole::Model | TextRole::DiagramNode => {
                &["entity.name.type", "support.type", "variable"]
            }
            TextRole::DiagramEdge => &["comment"],
            TextRole::Path => &["string", "markup.underline.link"],
            TextRole::Usage => &["constant.numeric", "constant"],
            TextRole::Branch => &["entity.name.function", "entity.name.tag"],
            _ => &[],
        }
    }

    fn syntax_color(self, role: TextRole, support: ColorSupport) -> Color {
        // Raw scope queries happen once per role/light-dark palette. A different
        // terminal color depth reuses the same assets without reparsing scopes.
        static FOREGROUNDS: [ScopeForegrounds; 2] =
            [const { [const { OnceLock::new() }; ROLE_COUNT] }; 2];
        let fallback = match role {
            TextRole::Link | TextRole::Code => self.color_for(TextRole::Accent, support),
            TextRole::Info | TextRole::Model | TextRole::DiagramNode => Color::DarkCyan,
            TextRole::Path | TextRole::Quote | TextRole::Usage => Color::DarkGreen,
            TextRole::TableHeader => self.color_for(TextRole::ProseStrong, support),
            TextRole::Branch => Color::DarkBlue,
            TextRole::OrderedListMarker => Color::Blue,
            _ => Color::Reset,
        };
        if support == ColorSupport::Ansi16 {
            return fallback;
        }
        let foreground = FOREGROUNDS[usize::from(self.light_background)][role as usize]
            .get_or_init(|| {
                code_theme::foreground_for_scopes(self.light_background, Self::syntax_scopes(role))
            });
        let Some((r, g, b)) = *foreground else {
            return fallback;
        };
        if matches!(
            role,
            TextRole::Code
                | TextRole::Info
                | TextRole::TableHeader
                | TextRole::DiagramNode
                | TextRole::DiagramEdge
        ) {
            return support.color(Color::Rgb { r, g, b });
        }
        if !matches!(
            role,
            TextRole::Model | TextRole::Path | TextRole::Usage | TextRole::Branch
        ) {
            return color_math::foreground((r, g, b), terminal_palette::background(), support);
        }
        // Metadata follows Codex's 85% saturation, without the faint attribute.
        let luma = (77 * u16::from(r) + 150 * u16::from(g) + 29 * u16::from(b)) / 256;
        let soften = |channel| {
            u8::try_from((u16::from(channel) * 85 + luma * 15 + 50) / 100)
                .expect("softened channel")
        };
        color_math::foreground(
            (soften(r), soften(g), soften(b)),
            terminal_palette::background(),
            support,
        )
    }

    /// Shared syntax policy for final blocks, streaming code and diff tokens.
    #[inline]
    pub(super) const fn supports_syntax(support: ColorSupport) -> bool {
        matches!(support, ColorSupport::TrueColor | ColorSupport::Ansi256)
    }

    /// Source-code modifiers intentionally differ from Markdown typography.
    #[inline]
    pub(super) fn syntax_style(
        style: syntect::highlighting::Style,
        support: ColorSupport,
    ) -> ContentStyle {
        if support == ColorSupport::NoColor {
            return ContentStyle::default();
        }
        let mut resolved = ContentStyle {
            foreground_color: Some(support.color(Color::Rgb {
                r: style.foreground.r,
                g: style.foreground.g,
                b: style.foreground.b,
            })),
            ..ContentStyle::default()
        };
        // Codex retains bold but suppresses theme-provided italic/underline in
        // source code. Markdown modifiers are resolved separately above.
        if style
            .font_style
            .contains(syntect::highlighting::FontStyle::BOLD)
        {
            resolved.attributes.set(Attribute::Bold);
        }
        resolved
    }

    /// ANSI is a serialization boundary, never the internal style model.
    #[inline]
    pub fn fg_seq(self, role: TextRole) -> String {
        ansi_fg(self.color(role))
    }

    /// One prompt surface for live input, committed prompts and restored history.
    /// Resolve the terminal palette once, outside redraw and typing hot paths.
    pub(crate) fn prompt_style(self) -> ContentStyle {
        static STYLES: [OnceLock<ContentStyle>; 2] = [const { OnceLock::new() }; 2];
        *STYLES[usize::from(self.light_background)].get_or_init(|| {
            let support = ColorSupport::detect();
            if support == ColorSupport::NoColor {
                return ContentStyle::default();
            }
            let light = self.light_background;
            let fallback = if light { (255, 255, 255) } else { (24, 24, 24) };
            let (r, g, b) = terminal_palette::background().unwrap_or(fallback);
            let alpha: u16 = if light { 4 } else { 12 };
            let target: u16 = if light { 0 } else { 255 };
            let blend = |channel: u8| {
                u8::try_from((u16::from(channel) * (100 - alpha) + target * alpha) / 100)
                    .expect("blended channel")
            };
            ContentStyle {
                // A fallback background must remain legible when the terminal
                // cannot report its default foreground on Windows.
                foreground_color: Some(if light { Color::Black } else { Color::White }),
                background_color: Some(support.color(Color::Rgb {
                    r: blend(r),
                    g: blend(g),
                    b: blend(b),
                })),
                ..ContentStyle::default()
            }
        })
    }

    pub fn selection_style(self, support: ColorSupport) -> ContentStyle {
        match support {
            ColorSupport::TrueColor | ColorSupport::Ansi256 => ContentStyle {
                foreground_color: Some(Color::AnsiValue(if self.light_background {
                    234
                } else {
                    255
                })),
                background_color: Some(Color::AnsiValue(self.code_background())),
                ..ContentStyle::default()
            },
            ColorSupport::Ansi16 | ColorSupport::NoColor => ContentStyle::default(),
        }
    }

    /// A focused choice marker keeps the brand's readable color and neutral bold.
    #[inline]
    pub fn choice_marker_style(self, support: ColorSupport) -> ContentStyle {
        self.overlay(
            self.selected_style(TextRole::Brand, support),
            TextRole::Strong,
            support,
        )
    }

    /// Resolve a selected label against its painted surface, not the terminal.
    pub fn selected_style(self, role: TextRole, support: ColorSupport) -> ContentStyle {
        let mut style = self.selection_style(support);
        let foreground = self.color_for(role, support);
        if foreground != Color::Reset {
            style.foreground_color = Some(Self::readable_on(
                foreground,
                style.background_color,
                support,
            ));
        }
        style
    }

    fn readable_on(foreground: Color, background: Option<Color>, support: ColorSupport) -> Color {
        let rgb = |color| match color {
            Color::Rgb { r, g, b } => Some((r, g, b)),
            Color::AnsiValue(index @ 16..) => Some(color_math::xterm(index)),
            _ => None,
        };
        rgb(foreground).map_or(foreground, |foreground| {
            color_math::foreground(foreground, background.and_then(rgb), support)
        })
    }

    #[inline]
    pub(super) fn diff_prefix_style(self, sign: char, support: ColorSupport) -> ContentStyle {
        ContentStyle {
            foreground_color: self.diff_sign_color(sign, support),
            background_color: self.diff_background(sign, support),
            ..ContentStyle::default()
        }
    }

    /// Resolve syntax colors against the diff fill before reducing color depth.
    #[inline]
    pub(super) fn diff_content_style(
        self,
        surface: ContentStyle,
        source: Option<ContentStyle>,
        support: ColorSupport,
    ) -> ContentStyle {
        if let Some(mut style) = source {
            style.foreground_color = style
                .foreground_color
                .map(|color| Self::readable_on(color, surface.background_color, support));
            style.background_color = surface.background_color;
            style
        } else {
            let mut style = surface;
            if self.light_background
                && matches!(support, ColorSupport::TrueColor | ColorSupport::Ansi256)
            {
                style.foreground_color = None;
            }
            style
        }
    }

    pub fn code_background(self) -> u8 {
        if self.light_background {
            255
        } else {
            236
        }
    }
    pub fn code_bg_seq(self) -> String {
        if ColorSupport::detect() == ColorSupport::NoColor {
            String::new()
        } else {
            format!("\x1b[48;5;{}m", self.code_background())
        }
    }

    fn diff_sign_color(self, sign: char, support: ColorSupport) -> Option<Color> {
        let role = match sign {
            '+' => TextRole::Added,
            '-' => TextRole::Removed,
            _ => return None,
        };
        (support != ColorSupport::NoColor).then(|| self.color_for(role, support))
    }
    pub fn diff_header_fg(self, is_file: bool) -> String {
        self.fg_seq(if is_file {
            TextRole::Accent
        } else {
            TextRole::Secondary
        })
    }
    fn diff_background(self, sign: char, support: ColorSupport) -> Option<Color> {
        let rgb = match (sign, self.light_background) {
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
            ColorSupport::Ansi256 => Some(Color::AnsiValue(match (sign, self.light_background) {
                ('+', false) => 22,
                ('-', false) => 52,
                ('+', true) => 194,
                _ => 224,
            })),
            ColorSupport::Ansi16 | ColorSupport::NoColor => None,
        }
    }
    pub(super) fn is_light_background() -> bool {
        // An explicit `theme` wins over detection, for terminals that answer
        // neither the palette query nor COLORFGBG.
        let configured = std::env::current_dir()
            .ok()
            .and_then(|cwd| runtime::ConfigLoader::default_for(cwd).load().ok())
            .and_then(|config| config.light_theme());
        if let Some(is_light) = configured {
            return is_light;
        }
        if let Some(background) = terminal_palette::background() {
            return terminal_palette::is_light(background);
        }
        std::env::var("COLORFGBG")
            .ok()
            .and_then(|val| val.rsplit(';').next()?.parse::<u8>().ok())
            .is_some_and(|bg| bg > 8)
    }
}
impl Default for ColorTheme {
    fn default() -> Self {
        Self::detect()
    }
}
#[inline]
pub fn theme() -> &'static ColorTheme {
    static THEME: OnceLock<ColorTheme> = OnceLock::new();
    THEME.get_or_init(ColorTheme::detect)
}
