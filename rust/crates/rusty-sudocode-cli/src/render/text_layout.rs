//! Terminal columns, grapheme boundaries and style-preserving line layout.

use std::{borrow::Cow, fmt::Write as _, ops::Range};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::styled_text::StyledText;

fn strip_ansi(input: &str) -> Cow<'_, str> {
    if input.contains('\x1b') {
        Cow::Owned(StyledText::from_ansi(input).text)
    } else {
        Cow::Borrowed(input)
    }
}

/// Keep transient status descriptions on one row and discard terminal controls.
#[inline]
pub(crate) fn single_line(input: &str) -> Cow<'_, str> {
    // Reuse the decoder's fast-path predicate: C0 controls also need removal
    // when there is no ESC byte. Ordinary progress text stays borrowed.
    let plain = StyledText::plain_source(input);
    if plain.contains(['\n', '\r', '\t']) {
        Cow::Owned(plain.replace(['\n', '\r', '\t'], " "))
    } else {
        plain
    }
}

pub(crate) fn display_width(input: &str) -> usize {
    let plain = strip_ansi(input);
    if !plain.contains('\t') {
        return terminal_width(&plain);
    }
    plain
        .graphemes(true)
        .fold(0, |col, grapheme| col + grapheme_width(grapheme, col))
}

/// Codex's terminal-cell correction for visible halfwidth sound marks.
/// Source: openai/codex 970111f6cb tui/src/width.rs (Apache-2.0).
#[inline]
fn terminal_width(text: &str) -> usize {
    text.width() + text.matches(['\u{FF9E}', '\u{FF9F}']).count()
}

/// Columns a grapheme may take when deciding where a row breaks. Terminals
/// disagree on multi-code-point clusters: `👩🏽‍💻` is one 2-cell cluster to
/// iTerm2 and `unicode-width`, but 6 cells to xterm.js with Unicode 11 (VS Code)
/// and to vt100, which lay out each code point. A row measured short
/// overflows, and the terminal wraps its tail into column 0; a row measured
/// long only ends a few cells early. Breaks therefore take the larger count.
fn row_fitting_width(grapheme: &str, col: usize) -> usize {
    let clustered = grapheme_width(grapheme, col);
    if grapheme.chars().nth(1).is_none() {
        return clustered;
    }
    let per_code_point: usize = grapheme
        .chars()
        .map(|c| terminal_width(c.encode_utf8(&mut [0; 4])))
        .sum();
    clustered.max(per_code_point)
}

fn grapheme_width(grapheme: &str, col: usize) -> usize {
    if grapheme == "\t" {
        8 - col % 8
    } else {
        terminal_width(grapheme)
    }
}

/// `text` word-wrapped right of the gutter, each row starting at
/// [`GUTTER_COLS`](crate::render::GUTTER_COLS). A row that already fits keeps
/// its own indentation, so pre-laid-out panels only move into the gutter.
pub(crate) fn wrap_in_gutter(text: &StyledText, width: usize) -> StyledText {
    let mut out = StyledText::default();
    if text.text.is_empty() {
        return out;
    }
    let rows = wrap_words(
        text,
        width.saturating_sub(crate::render::GUTTER_COLS).max(1),
    );
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            out.plain("\n");
        }
        if !row.text.is_empty() {
            out.plain(crate::render::GUTTER);
            out.append(row);
        }
    }
    out
}

/// Wrap a single logical line. Rows retain complete styles so a frame or
/// response prefix can reset the terminal without losing continuation colors.
/// An indivisible grapheme wider than the entire viewport is kept intact.
pub(crate) fn wrap_line(
    source: &StyledText,
    range: Range<usize>,
    width: usize,
    initial_col: usize,
) -> Vec<(StyledText, usize)> {
    wrap_line_with_indent(source, range, width, initial_col, 0)
}

pub(crate) fn wrap_line_with_indent(
    source: &StyledText,
    range: Range<usize>,
    width: usize,
    initial_col: usize,
    continuation_indent: usize,
) -> Vec<(StyledText, usize)> {
    let width = width.max(1);
    let indent = continuation_indent.min(width.saturating_sub(2));
    let padding = " ".repeat(indent);
    let mut rows = Vec::new();
    let mut row = StyledText::default();
    let mut col = initial_col;
    for (offset, grapheme) in source.text[range.clone()].grapheme_indices(true) {
        let start = range.start + offset;
        let cells = grapheme_width(grapheme, col);
        if grapheme == "\t" {
            // Expand even tabs wider than the viewport without overflowing it.
            let (style, _, link) = source.rich_spans(start..start + 1).next().unwrap();
            for _ in 0..cells {
                if col >= width {
                    rows.push((std::mem::take(&mut row), col));
                    row.push(crossterm::style::ContentStyle::default(), &padding);
                    col = indent;
                }
                row.push_link(style, " ", link);
                col += 1;
            }
        } else {
            if col > 0 && col + cells > width {
                rows.push((std::mem::take(&mut row), col));
                row.push(crossterm::style::ContentStyle::default(), &padding);
                col = indent;
            }
            for (style, part, link) in source.rich_spans(start..start + grapheme.len()) {
                row.push_link(style, part, link);
            }
            col += cells;
        }
    }
    rows.push((row, col));
    rows
}

pub(crate) fn wrap_ansi_to_width(input: &str, width: usize) -> Vec<String> {
    wrap_ansi_with_indent(input, width, 0)
}

pub(crate) use super::word_wrap::wrap_words;

/// Wrap styled list content with hanging indentation on continuation rows.
pub(crate) fn wrap_ansi_with_indent(input: &str, width: usize, indent: usize) -> Vec<String> {
    if width == 0 {
        return vec![input.to_string()];
    }
    let source = StyledText::from_ansi(input);
    wrap_line_with_indent(&source, 0..source.text.len(), width, 0, indent)
        .into_iter()
        .map(|(mut row, columns)| {
            let fill = row
                .spans(0..row.text.len())
                .last()
                .and_then(|(style, _)| style.background_color);
            if let Some(background) = fill {
                if columns < width {
                    row.push(
                        crossterm::style::ContentStyle {
                            background_color: Some(background),
                            ..Default::default()
                        },
                        &" ".repeat(width - columns),
                    );
                }
            }
            let mut output = String::new();
            row.write_ansi(0..row.text.len(), &mut output);
            output
        })
        .collect()
}

/// A history line with its background padding erased instead of written.
/// `wrap_ansi_with_indent` pads a background-coloured row to the full width
/// with spaces in that colour. The terminal holds those spaces as text, so
/// when it narrows each such row wraps into blank shaded rows. When a line
/// reaches the terminal's edge (`width`) and ends in spaces that carry one
/// background, the spaces become that background and EL. The terminal
/// paints the same colour to the edge, and erased cells never wrap. This is
/// the rule iocraft applies to a live frame's shaded tail (`erased_tail`),
/// applied here where history leaves scode. Any other line comes back
/// unchanged: one narrower than `width` (EL would colour past it), one laid
/// out for another width, or one with controls other than SGR and links.
pub(crate) fn erase_filled_tail(line: &str, width: usize) -> Cow<'_, str> {
    if !line.contains('\x1b') || !only_styles_and_links(line) {
        return Cow::Borrowed(line);
    }
    let styled = StyledText::from_ansi(line);
    let end = styled.text.len();
    let start = styled.text.trim_end_matches(' ').len();
    if start == end || display_width(&styled.text) != width {
        return Cow::Borrowed(line);
    }
    let mut background = None;
    for (style, _, link) in styled.rich_spans(start..end) {
        let Some(color) = style.background_color else {
            return Cow::Borrowed(line);
        };
        if link.is_some() || background.is_some_and(|other| other != color) {
            return Cow::Borrowed(line);
        }
        background = Some(color);
    }
    let Some(background) = background else {
        return Cow::Borrowed(line);
    };
    let mut erased = String::with_capacity(line.len());
    styled.write_ansi(0..start, &mut erased);
    let _ = write!(
        erased,
        "{}\x1b[K{}",
        crossterm::style::SetBackgroundColor(background),
        super::RESET
    );
    Cow::Owned(erased)
}

/// Whether every control in `line` is SGR or an OSC 8 hyperlink, the only ones
/// a styled row carries. Rewriting any other line could drop its controls.
fn only_styles_and_links(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            0x1b => match bytes.get(index + 1) {
                Some(b'[') => {
                    let Some(length) = bytes[index + 2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                    else {
                        return false;
                    };
                    if bytes[index + 2 + length] != b'm' {
                        return false;
                    }
                    index += length + 3;
                }
                Some(b']') if bytes[index + 2..].starts_with(b"8;") => {
                    let rest = &bytes[index + 2..];
                    let Some(end) = rest.iter().position(|b| *b == 0x07 || *b == 0x1b) else {
                        return false;
                    };
                    index += 2 + end + if rest[end] == 0x07 { 1 } else { 2 };
                }
                _ => return false,
            },
            b'\r' => return false,
            byte if byte < 0x20 && byte != b'\t' => return false,
            _ => index += 1,
        }
    }
    true
}

/// [`wrap_line_with_indent`] for prose: rows end between words. A row ends
/// at its last space that fits, the spaces are dropped, and the next row
/// starts at `continuation_indent` with the following word. A word wider than
/// a whole row is still split between graphemes. Leading indentation is never
/// a break, and a row continuing at `initial_col` cannot move its first word.
pub(crate) fn wrap_words_with_indent(
    source: &StyledText,
    range: Range<usize>,
    width: usize,
    initial_col: usize,
    continuation_indent: usize,
) -> Vec<(StyledText, usize)> {
    let width = width.max(1);
    let indent = continuation_indent.min(width.saturating_sub(2));
    let padding = " ".repeat(indent);
    let graphemes: Vec<(usize, &str)> = source.text[range.clone()]
        .grapheme_indices(true)
        .map(|(offset, grapheme)| (range.start + offset, grapheme))
        .collect();
    let skip_spaces = |mut index: usize| {
        while graphemes
            .get(index)
            .is_some_and(|(_, grapheme)| *grapheme == " ")
        {
            index += 1;
        }
        index
    };
    let mut rows = Vec::new();
    let mut row = StyledText::default();
    let mut col = initial_col;
    // Where this row may end: its length and column before the latest run of
    // spaces that follows a word on it, and the index of that run.
    let mut space: Option<(usize, usize, usize)> = None;
    let mut has_word = false;
    // A continuation row holding only its indentation.
    let mut fresh = false;
    let mut index = 0;
    while index < graphemes.len() {
        let (start, grapheme) = graphemes[index];
        let cells = row_fitting_width(grapheme, col);
        if col > 0 && col + cells > width && !fresh {
            let next = if let Some((len, end_col, run)) = space.take() {
                row.truncate(len);
                rows.push((std::mem::take(&mut row), end_col));
                Some(skip_spaces(run))
            } else {
                rows.push((std::mem::take(&mut row), col));
                (grapheme == " ").then(|| skip_spaces(index))
            };
            row.push(crossterm::style::ContentStyle::default(), &padding);
            col = indent;
            has_word = false;
            fresh = true;
            if let Some(next) = next {
                index = next;
                continue;
            }
        }
        if grapheme == " " {
            let follows_word = has_word && index > 0 && graphemes[index - 1].1 != " ";
            if follows_word {
                space = Some((row.text.len(), col, index));
            }
        } else {
            has_word = true;
        }
        let (style, _, link) = source
            .rich_spans(start..start + grapheme.len())
            .next()
            .unwrap();
        if grapheme == "\t" {
            // Tabs become the spaces they occupy, as in `wrap_line_with_indent`.
            row.push_link(style, &" ".repeat(cells), link);
        } else {
            for (style, part, link) in source.rich_spans(start..start + grapheme.len()) {
                row.push_link(style, part, link);
            }
        }
        col += cells;
        fresh = false;
        index += 1;
    }
    rows.push((row, col));
    rows
}

/// Terminal strings use grapheme widths. The pinned iocraft canvas clips by
/// scalar widths, so its adapter must reserve those columns without splitting
/// a grapheme. Keep that compatibility decision separate from scrollback.
#[derive(Clone, Copy)]
pub(crate) enum ColumnMetric {
    Terminal,
    Canvas,
}

impl ColumnMetric {
    #[inline]
    pub(crate) fn width(self, input: &str) -> usize {
        match self {
            Self::Terminal => display_width(input),
            Self::Canvas => strip_ansi(input)
                .chars()
                .map(|ch| {
                    if ch == '\t' {
                        1
                    } else {
                        ch.width().unwrap_or(0)
                    }
                })
                .sum(),
        }
    }

    #[inline]
    pub(crate) fn truncate(self, input: &str, max_width: usize) -> String {
        truncate(input, max_width, self)
    }
}

#[inline]
pub(crate) fn truncate_to_width(input: &str, max_width: usize) -> String {
    ColumnMetric::Terminal.truncate(input, max_width)
}

fn truncate(input: &str, max_width: usize, metric: ColumnMetric) -> String {
    if metric.width(input) <= max_width {
        return input.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let source = StyledText::from_ansi(input);
    let mut col = 0;
    let mut end = 0;
    for (offset, grapheme) in source.text.grapheme_indices(true) {
        let cells = match metric {
            ColumnMetric::Terminal => grapheme_width(grapheme, col),
            ColumnMetric::Canvas => metric.width(grapheme),
        };
        if col + cells >= max_width {
            break;
        }
        col += cells;
        end = offset + grapheme.len();
    }
    let mut output = String::new();
    source.write_ansi(0..end, &mut output);
    output.push('…');
    output
}
