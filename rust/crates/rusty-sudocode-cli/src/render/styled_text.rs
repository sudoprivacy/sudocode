//! Structured terminal text shared by layout and UI adapters.
//! Decode external ANSI once; keep owned styles as data until the output boundary.

use crossterm::style::{Attribute, Color, ContentStyle};
use std::{fmt::Write as _, ops::Range};
use vte::{Params, Perform};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StyledText {
    pub(crate) text: String,
    runs: Vec<(Range<usize>, ContentStyle)>,
}

impl StyledText {
    pub(crate) fn from_ansi(input: &str) -> Self {
        if !input
            .bytes()
            .any(|byte| byte == 0x1b || (byte < 0x20 && !matches!(byte, b'\n' | b'\r' | b'\t')))
        {
            let mut text = Self::default();
            text.push(ContentStyle::default(), input);
            return text;
        }
        let mut decoder = AnsiDecoder::new();
        vte::Parser::new().advance(&mut decoder, input.as_bytes());
        decoder.output
    }

    pub(crate) fn push(&mut self, style: ContentStyle, text: &str) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if let Some((range, previous)) = self.runs.last_mut() {
            if *previous == style {
                range.end = self.text.len();
                return;
            }
        }
        self.runs.push((start..self.text.len(), style));
    }

    /// Iterate only the intersecting runs, even for a slice through a style change.
    pub(crate) fn spans(&self, range: Range<usize>) -> impl Iterator<Item = (ContentStyle, &str)> {
        let first = self.runs.partition_point(|(run, _)| run.end <= range.start);
        self.runs[first..]
            .iter()
            .take_while(move |(run, _)| run.start < range.end)
            .map(move |(run, style)| {
                (
                    *style,
                    &self.text[run.start.max(range.start)..run.end.min(range.end)],
                )
            })
    }

    pub(crate) fn write_ansi(&self, range: Range<usize>, output: &mut String) {
        for (style, text) in self.spans(range) {
            if style == ContentStyle::default() {
                output.push_str(text);
            } else {
                let _ = write!(output, "{}{}", super::RESET, style.apply(text));
            }
        }
    }
}

pub(crate) struct AnsiDecoder {
    pub(crate) output: StyledText,
    style: ContentStyle,
}

impl AnsiDecoder {
    pub(crate) fn new() -> Self {
        Self {
            output: StyledText::default(),
            style: ContentStyle::default(),
        }
    }

    fn reset(&mut self) {
        self.style = ContentStyle::default();
    }

    fn apply_sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        let mut index = 0;
        while let Some(group) = groups.get(index) {
            let code = group[0];
            match code {
                0 => self.reset(),
                1 => self.style.attributes.set(Attribute::Bold),
                2 => self.style.attributes.set(Attribute::Dim),
                3 => self.style.attributes.set(Attribute::Italic),
                4 => self.style.attributes.set(Attribute::Underlined),
                7 => self.style.attributes.set(Attribute::Reverse),
                9 => self.style.attributes.set(Attribute::CrossedOut),
                22 => {
                    self.style.attributes.unset(Attribute::Bold);
                    self.style.attributes.unset(Attribute::Dim);
                }
                23 => self.style.attributes.unset(Attribute::Italic),
                24 => self.style.attributes.unset(Attribute::Underlined),
                27 => self.style.attributes.unset(Attribute::Reverse),
                29 => self.style.attributes.unset(Attribute::CrossedOut),
                30..=37 => self.style.foreground_color = Some(indexed_color(code - 30)),
                90..=97 => self.style.foreground_color = Some(indexed_color(code - 90 + 8)),
                39 => self.style.foreground_color = None,
                40..=47 => self.style.background_color = Some(indexed_color(code - 40)),
                100..=107 => self.style.background_color = Some(indexed_color(code - 100 + 8)),
                49 => self.style.background_color = None,
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
                    if let Some(color) = color {
                        if code == 38 {
                            self.style.foreground_color = Some(color);
                        } else {
                            self.style.background_color = Some(color);
                        }
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }
}

impl Perform for AnsiDecoder {
    fn print(&mut self, ch: char) {
        self.output.push(self.style, ch.encode_utf8(&mut [0; 4]));
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.output.push(self.style, "\n"),
            b'\r' => self.output.push(self.style, "\r"),
            b'\t' => self.output.push(self.style, "\t"),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action == 'm' && intermediates.is_empty() && !ignore {
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
