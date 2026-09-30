//! Bridge the CLI's existing ANSI formatters to iocraft's structured styles.
//!
//! `Text` and `MixedText` deliberately strip embedded ANSI. Decode SGR into
//! spans first, so layout still measures plain text and the canvas owns all
//! terminal writes. Cursor movement, erase commands and OSC are never replayed.
//! The pinned iocraft version has no strikethrough or per-span background,
//! and represents bold/dim as mutually exclusive weights. Those attributes
//! require an iocraft extension; this bridge does not claim to preserve them.

use iocraft::prelude::*;
use vte::{Params, Perform};

#[derive(Default, Props)]
pub(super) struct AnsiTextProps {
    pub content: String,
    pub color: Option<Color>,
}

#[component]
pub(super) fn AnsiText(props: &AnsiTextProps) -> impl Into<AnyElement<'static>> {
    let mut parsed = StyledText::new(props.color);
    vte::Parser::new().advance(&mut parsed, props.content.as_bytes());
    parsed.flush();
    element! { MixedText(contents: parsed.spans) }
}

struct StyledText {
    spans: Vec<MixedTextContent>,
    text: String,
    style: MixedTextContent,
    default_color: Option<Color>,
}

impl StyledText {
    fn new(default_color: Option<Color>) -> Self {
        let mut value = Self {
            spans: Vec::new(),
            text: String::new(),
            style: MixedTextContent::default(),
            default_color,
        };
        value.reset();
        value
    }

    fn reset(&mut self) {
        self.style = MixedTextContent::default();
        self.style.color = self.default_color;
    }

    fn flush(&mut self) {
        if !self.text.is_empty() {
            let mut span = self.style.clone();
            span.text = std::mem::take(&mut self.text);
            self.spans.push(span);
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut index = 0;
        while let Some(group) = groups.get(index) {
            let code = group[0];
            match code {
                0 => self.reset(),
                1 => self.style.weight = Weight::Bold,
                2 => self.style.weight = Weight::Light,
                3 => self.style.italic = true,
                4 => self.style.decoration = TextDecoration::Underline,
                7 => self.style.invert = true,
                22 => self.style.weight = Weight::Normal,
                23 => self.style.italic = false,
                24 => self.style.decoration = TextDecoration::None,
                27 => self.style.invert = false,
                30..=37 => self.style.color = Some(indexed_color(code - 30)),
                90..=97 => self.style.color = Some(indexed_color(code - 90 + 8)),
                39 => self.style.color = self.default_color,
                38 | 48 => {
                    // Consume the entire color even when its target is not
                    // supported, rather than treating RGB values as SGR codes.
                    let color = if group.len() > 1 {
                        extended_color(&group[1..])
                    } else {
                        let count = match groups.get(index + 1).map(|g| g[0]) {
                            Some(5) => 2,
                            Some(2) => 4,
                            _ => 1,
                        };
                        let values: Vec<u16> = groups
                            .iter()
                            .skip(index + 1)
                            .take(count)
                            .map(|g| g[0])
                            .collect();
                        index += values.len();
                        extended_color(&values)
                    };
                    if code == 38 {
                        if let Some(color) = color {
                            self.style.color = Some(color);
                        }
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
}

impl Perform for StyledText {
    fn print(&mut self, ch: char) {
        self.text.push(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.text.push('\n'),
            b'\t' => self.text.push(' '),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action == 'm' && intermediates.is_empty() && !ignore {
            self.flush();
            self.apply_sgr(params);
        }
    }
}

fn extended_color(values: &[u16]) -> Option<Color> {
    match values {
        [5, index] if *index <= 255 => Some(indexed_color(*index)),
        [2, r, g, b] | [2, 0, r, g, b] => Some(Color::Rgb {
            r: u8::try_from(*r).ok()?,
            g: u8::try_from(*g).ok()?,
            b: u8::try_from(*b).ok()?,
        }),
        _ => None,
    }
}

fn indexed_color(index: u16) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::DarkRed,
        2 => Color::DarkGreen,
        3 => Color::DarkYellow,
        4 => Color::DarkBlue,
        5 => Color::DarkMagenta,
        6 => Color::DarkCyan,
        7 => Color::Grey,
        8 => Color::DarkGrey,
        9 => Color::Red,
        10 => Color::Green,
        11 => Color::Yellow,
        12 => Color::Blue,
        13 => Color::Magenta,
        14 => Color::Cyan,
        15 => Color::White,
        value => Color::AnsiValue(u8::try_from(value).unwrap_or(255)),
    }
}
