//! Structured terminal text shared by layout and UI adapters.
//! Decode external ANSI once; keep owned styles as data until the output boundary.

use crossterm::style::{Attribute, Color, ContentStyle};
use std::{fmt::Write as _, ops::Range, sync::Arc};
use vte::{Params, Perform};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    range: Range<usize>,
    style: ContentStyle,
    hyperlink: Option<Arc<str>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StyledText {
    pub(crate) text: String,
    runs: Vec<Run>,
}

impl StyledText {
    /// Account retained allocations conservatively for bounded layout caches.
    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.text.capacity()
            + self.runs.capacity() * std::mem::size_of::<Run>()
            + self
                .runs
                .iter()
                .filter_map(|run| run.hyperlink.as_ref())
                .map(|link| link.len())
                .sum::<usize>()
    }

    pub(crate) fn from_ansi(input: &str) -> Self {
        Self::from_ansi_with_style(input, ContentStyle::default())
    }

    /// Control bytes other than ordinary layout whitespace require decoding.
    #[inline]
    pub(crate) fn needs_decoding(input: &str) -> bool {
        input
            .bytes()
            .any(|byte| byte == 0x1b || (byte < 0x20 && !matches!(byte, b'\n' | b'\r' | b'\t')))
    }

    /// An initial style is content state, not a fallback applied after decoding.
    /// SGR 0 and 39 therefore restore the terminal default, even when the slot
    /// starts in a semantic label color.
    pub(crate) fn from_ansi_with_style(input: &str, initial_style: ContentStyle) -> Self {
        if !Self::needs_decoding(input) {
            let mut text = Self::default();
            text.push(initial_style, input);
            return text;
        }
        let mut decoder = AnsiDecoder::new();
        decoder.style = initial_style;
        decoder.advance(&mut vte::Parser::new(), input);
        decoder.output
    }

    pub(crate) fn push(&mut self, style: ContentStyle, text: &str) {
        self.push_link(style, text, None);
    }

    pub(crate) fn push_link(
        &mut self,
        style: ContentStyle,
        text: &str,
        hyperlink: Option<&Arc<str>>,
    ) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if let Some(previous) = self.runs.last_mut() {
            if previous.style == style && previous.hyperlink.as_ref() == hyperlink {
                previous.range.end = self.text.len();
                return;
            }
        }
        self.runs.push(Run {
            range: start..self.text.len(),
            style,
            hyperlink: hyperlink.cloned(),
        });
    }

    /// Iterate only the intersecting runs, even for a slice through a style change.
    pub(crate) fn spans(&self, range: Range<usize>) -> impl Iterator<Item = (ContentStyle, &str)> {
        self.rich_spans(range).map(|(style, text, _)| (style, text))
    }

    pub(crate) fn rich_spans(
        &self,
        range: Range<usize>,
    ) -> impl Iterator<Item = (ContentStyle, &str, Option<&Arc<str>>)> {
        let first = self
            .runs
            .partition_point(|run| run.range.end <= range.start);
        self.runs[first..]
            .iter()
            .take_while(move |run| run.range.start < range.end)
            .map(move |run| {
                (
                    run.style,
                    &self.text[run.range.start.max(range.start)..run.range.end.min(range.end)],
                    run.hyperlink.as_ref(),
                )
            })
    }

    pub(crate) fn write_ansi(&self, range: Range<usize>, output: &mut String) {
        let mut active: Option<&Arc<str>> = None;
        for (style, text, hyperlink) in self.rich_spans(range) {
            if hyperlink != active {
                let _ = write!(
                    output,
                    "{}",
                    super::hyperlink::SetHyperlink(hyperlink.map(AsRef::as_ref))
                );
                active = hyperlink;
            }
            if style == ContentStyle::default() {
                output.push_str(text);
            } else {
                let _ = write!(output, "{}{}", super::RESET, style.apply(text));
            }
        }
        if active.is_some() {
            let _ = write!(output, "{}", super::hyperlink::SetHyperlink(None));
        }
    }

    pub(crate) fn clear_styles(&mut self) {
        for run in &mut self.runs {
            run.style = ContentStyle::default();
        }
    }
}

pub(crate) struct AnsiDecoder {
    pub(crate) output: StyledText,
    style: ContentStyle,
    hyperlink: Option<Arc<str>>,
    pending: String,
    discarding_link: bool,
}

impl AnsiDecoder {
    pub(crate) fn new() -> Self {
        Self {
            output: StyledText::default(),
            style: ContentStyle::default(),
            hyperlink: None,
            pending: String::new(),
            discarding_link: false,
        }
    }

    /// Preserve the complete OSC 8 URI before VTE splits OSC into its fixed
    /// parameter array. A semicolon is valid URI data, not another parameter.
    /// Incomplete sequences are bounded and carried across streaming chunks.
    pub(crate) fn advance(&mut self, parser: &mut vte::Parser, input: &str) {
        const OPEN: &str = "\x1b]8;";
        let combined;
        let mut input = if self.pending.is_empty() {
            input
        } else {
            combined = std::mem::take(&mut self.pending) + input;
            &combined
        };
        loop {
            if self.discarding_link {
                let Some((end, length)) = osc_end(input) else {
                    if input.ends_with('\x1b') {
                        self.pending.push('\x1b');
                    }
                    return;
                };
                input = &input[end + length..];
                self.discarding_link = false;
            }
            let Some(start) = input.find(OPEN) else {
                // A chunk can end anywhere inside the short introducer.
                let suffix = (1..OPEN.len())
                    .rev()
                    .find(|&length| input.ends_with(&OPEN[..length]))
                    .unwrap_or(0);
                let split = input.len() - suffix;
                parser.advance(self, &input.as_bytes()[..split]);
                self.pending.push_str(&input[split..]);
                return;
            };
            parser.advance(self, &input.as_bytes()[..start]);
            // ESC starts a new control sequence even after incomplete CSI.
            parser.advance(self, b"\x18");
            let payload = &input[start + OPEN.len()..];
            let Some((end, length)) = osc_end(payload) else {
                if payload.len() <= super::hyperlink::MAX_DESTINATION_BYTES + 256 {
                    self.pending.push_str(&input[start..]);
                } else {
                    self.discarding_link = true;
                    self.hyperlink = None;
                    if input.ends_with('\x1b') {
                        self.pending.push('\x1b');
                    }
                }
                return;
            };
            self.hyperlink = payload[..end]
                .split_once(';')
                .and_then(|(_, target)| super::hyperlink::destination(target));
            input = &payload[end + length..];
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
        self.output.push_link(
            self.style,
            ch.encode_utf8(&mut [0; 4]),
            self.hyperlink.as_ref(),
        );
    }

    fn execute(&mut self, byte: u8) {
        let text = match byte {
            b'\n' => "\n",
            b'\r' => "\r",
            b'\t' => "\t",
            _ => return,
        };
        self.output
            .push_link(self.style, text, self.hyperlink.as_ref());
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if action == 'm' && intermediates.is_empty() && !ignore {
            self.apply_sgr(params);
        }
    }
}

fn osc_end(input: &str) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    bytes.iter().enumerate().find_map(|(index, byte)| {
        if *byte == 7 {
            Some((index, 1))
        } else if *byte == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
            Some((index, 2))
        } else {
            None
        }
    })
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
