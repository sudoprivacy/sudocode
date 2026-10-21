//! Choice surfaces and focus markers, without changing input or layout owners.
use crate::render::{ColorSupport, ColorTheme};
use crossterm::style::{Attribute, ContentStyle};

pub(super) struct ChoiceText<'a> {
    theme: &'a ColorTheme,
    support: ColorSupport,
    focused: bool,
    surface: ContentStyle,
}

impl<'a> ChoiceText<'a> {
    pub fn new(theme: &'a ColorTheme, support: ColorSupport, focused: bool) -> Self {
        Self {
            theme,
            support,
            focused,
            surface: if focused {
                theme.selection_style(support)
            } else {
                ContentStyle::default()
            },
        }
    }

    fn paint(&self, text: &str, style: ContentStyle) -> String {
        if text.is_empty() || self.support == ColorSupport::NoColor {
            text.to_string()
        } else {
            style.apply(text).to_string()
        }
    }

    pub fn prefix(&self, text: &str) -> String {
        let mut style = self.surface;
        if self.focused {
            style.foreground_color = Some(self.theme.primary);
            style.attributes.set(Attribute::Bold);
        }
        self.paint(text, style)
    }

    pub fn body(&self, text: &str) -> String {
        self.paint(text, self.surface)
    }

    pub fn muted(&self, text: &str) -> String {
        let mut style = self.surface;
        style.foreground_color = Some(self.theme.muted);
        self.paint(text, style)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::styled_text::StyledText;

    #[test]
    fn selected_surface_resets_before_unselected_body_and_hints_are_not_dim() {
        for theme in [ColorTheme::dark(), ColorTheme::light()] {
            for support in [ColorSupport::TrueColor, ColorSupport::Ansi256] {
                let selected = ChoiceText::new(&theme, support, true);
                let ordinary = ChoiceText::new(&theme, support, false);
                let wire = format!(
                    "{}{}{}\n{}{}",
                    selected.prefix("> [1] "),
                    selected.body("SelectedChoice"),
                    selected.muted(" RecommendedChoice"),
                    ordinary.prefix("  [2] "),
                    ordinary.body("OrdinaryChoice"),
                );
                let text = StyledText::from_ansi(&wire);
                for (needle, foreground, background, bold) in [
                    (
                        "> [1]",
                        Some(theme.primary),
                        Some(crossterm::style::Color::AnsiValue(theme.code_bg)),
                        true,
                    ),
                    (
                        "SelectedChoice",
                        theme.selection_style(support).foreground_color,
                        Some(crossterm::style::Color::AnsiValue(theme.code_bg)),
                        false,
                    ),
                    (
                        "RecommendedChoice",
                        Some(theme.muted),
                        Some(crossterm::style::Color::AnsiValue(theme.code_bg)),
                        false,
                    ),
                    ("OrdinaryChoice", None, None, false),
                ] {
                    let start = text.text.find(needle).unwrap();
                    for (style, _) in text.spans(start..start + needle.len()) {
                        assert_eq!(style.foreground_color, foreground, "{needle}");
                        assert_eq!(style.background_color, background, "{needle}");
                        assert_eq!(style.attributes.has(Attribute::Bold), bold, "{needle}");
                        assert!(!style.attributes.has(Attribute::Dim), "{needle}");
                    }
                }
            }
        }
    }

    #[test]
    fn selected_text_and_accents_have_readable_contrast_on_each_surface() {
        fn rgb(color: crossterm::style::Color) -> [u8; 3] {
            let crossterm::style::Color::AnsiValue(index) = color else {
                panic!("selection theme uses indexed colors");
            };
            if index >= 232 {
                [8 + (index - 232) * 10; 3]
            } else {
                let levels = [0, 95, 135, 175, 215, 255];
                let cube = usize::from(index - 16);
                [levels[cube / 36], levels[cube / 6 % 6], levels[cube % 6]]
            }
        }
        fn luminance(color: crossterm::style::Color) -> f64 {
            let [r, g, b] = rgb(color).map(|component| {
                let value = f64::from(component) / 255.0;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            });
            0.2126 * r + 0.7152 * g + 0.0722 * b
        }
        for theme in [ColorTheme::dark(), ColorTheme::light()] {
            let surface = theme.selection_style(ColorSupport::Ansi256);
            let background = luminance(surface.background_color.unwrap());
            for foreground in [
                surface.foreground_color.unwrap(),
                theme.primary,
                theme.muted,
            ] {
                let foreground = luminance(foreground);
                let contrast =
                    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
                assert!(contrast >= 4.5, "selection contrast: {contrast}");
            }
        }
    }

    #[test]
    fn no_color_keeps_selection_and_text_without_escape_sequences() {
        let theme = ColorTheme::dark();
        let choice = ChoiceText::new(&theme, ColorSupport::NoColor, true);
        assert_eq!(
            format!(
                "{}{}{}",
                choice.prefix("> [1] "),
                choice.body("Choice"),
                choice.muted(" (recommended)")
            ),
            "> [1] Choice (recommended)"
        );
        assert_eq!(
            theme.selection_style(ColorSupport::Ansi16),
            ContentStyle::default()
        );
    }
}
