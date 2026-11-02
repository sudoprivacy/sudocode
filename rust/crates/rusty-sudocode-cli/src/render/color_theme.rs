//! Semantic terminal styles. Syntax assets supply code and metadata accents;
//! terminal-owned ANSI colors supply status, and amber identifies the brand.

use super::{ansi_fg, code_theme, color_math, terminal_palette, ColorSupport};
use crossterm::style::{Attribute, Color, ContentStyle};
use std::sync::OnceLock;

/// Choose a meaning at the call site; palette and capability decisions live here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum TextRole {
    Body,
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
    Quote,
    ListMarker,
    Model,
    Path,
    Usage,
    Branch,
    Added,
    Removed,
}
const ROLE_COUNT: usize = TextRole::Removed as usize + 1;

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
            if role == TextRole::Strong {
                style.attributes.set(Attribute::Bold);
            }
            style
        })
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

    fn resolve_color(self, role: TextRole, support: ColorSupport) -> Color {
        let limited = support == ColorSupport::Ansi16;
        let readable = |rgb| color_math::foreground(rgb, terminal_palette::background(), support);
        match role {
            TextRole::Body | TextRole::Strong => Color::Reset,
            TextRole::Brand => {
                if limited {
                    Color::DarkYellow
                } else {
                    readable(if self.light_background {
                        (148, 93, 5)
                    } else {
                        Self::AMBER
                    })
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
            TextRole::Success | TextRole::Added | TextRole::Quote => Color::DarkGreen,
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
            TextRole::Code => code_theme::role_color(role, self.light_background, support),
            TextRole::Info => {
                if limited {
                    Color::DarkCyan
                } else {
                    code_theme::role_color(TextRole::Code, self.light_background, support)
                }
            }
            TextRole::ListMarker => Color::Blue,
            TextRole::Model | TextRole::Path | TextRole::Usage | TextRole::Branch => {
                code_theme::role_color(role, self.light_background, support)
            }
        }
    }

    /// ANSI is a serialization boundary, never the internal style model.
    #[inline]
    pub fn fg_seq(self, role: TextRole) -> String {
        ansi_fg(self.color(role))
    }

    /// Shared prompt surfaces for live input, committed prompts and context headers.
    /// Resolve the terminal palette once, outside redraw and typing hot paths.
    pub(crate) fn prompt_style(self, live: bool) -> ContentStyle {
        static STYLES: [OnceLock<[ContentStyle; 2]>; 2] = [const { OnceLock::new() }; 2];
        STYLES[usize::from(self.light_background)].get_or_init(|| {
            let support = ColorSupport::detect();
            if support == ColorSupport::NoColor {
                return [ContentStyle::default(); 2];
            }
            let light = self.light_background;
            let fallback = if light { (255, 255, 255) } else { (24, 24, 24) };
            let (r, g, b) = terminal_palette::background().unwrap_or(fallback);
            [false, true].map(|live| {
                let alpha: u16 = if light {
                    if live {
                        8
                    } else {
                        4
                    }
                } else if live {
                    16
                } else {
                    10
                };
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
        })[usize::from(live)]
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

    /// Resolve a selected label against its painted surface, not the terminal.
    pub fn selected_style(self, role: TextRole, support: ColorSupport) -> ContentStyle {
        let mut style = self.selection_style(support);
        let foreground = self.color_for(role, support);
        style.foreground_color =
            match (foreground, style.background_color) {
                (Color::Rgb { r, g, b }, Some(Color::AnsiValue(bg))) => Some(
                    color_math::foreground((r, g, b), Some(color_math::xterm(bg)), support),
                ),
                (Color::AnsiValue(index @ 16..), Some(Color::AnsiValue(bg))) => {
                    Some(color_math::foreground(
                        color_math::xterm(index),
                        Some(color_math::xterm(bg)),
                        support,
                    ))
                }
                (Color::Reset, _) => style.foreground_color,
                (color, _) => Some(color),
            };
        style
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

    pub(super) fn diff_sign_color(self, sign: char, support: ColorSupport) -> Option<Color> {
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
    pub(super) fn diff_background(self, sign: char, support: ColorSupport) -> Option<Color> {
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
