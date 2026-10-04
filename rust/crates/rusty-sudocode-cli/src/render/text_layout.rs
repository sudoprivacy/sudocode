//! Terminal columns, grapheme boundaries and style-preserving line layout.

use std::{borrow::Cow, ops::Range};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::styled_text::StyledText;

fn strip_ansi(input: &str) -> Cow<'_, str> {
    if input.contains('\x1b') {
        Cow::Owned(StyledText::from_ansi(input).text)
    } else {
        Cow::Borrowed(input)
    }
}

pub(crate) fn display_width(input: &str) -> usize {
    let plain = strip_ansi(input);
    if !plain.contains('\t') {
        return plain.width();
    }
    plain
        .graphemes(true)
        .fold(0, |col, grapheme| col + grapheme_width(grapheme, col))
}

fn grapheme_width(grapheme: &str, col: usize) -> usize {
    if grapheme == "\t" {
        8 - col % 8
    } else {
        grapheme.width()
    }
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

fn wrap_line_with_indent(
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
            let style = source.spans(start..start + 1).next().unwrap().0;
            for _ in 0..cells {
                if col >= width {
                    rows.push((std::mem::take(&mut row), col));
                    row.push(crossterm::style::ContentStyle::default(), &padding);
                    col = indent;
                }
                row.push(style, " ");
                col += 1;
            }
        } else {
            if col > 0 && col + cells > width {
                rows.push((std::mem::take(&mut row), col));
                row.push(crossterm::style::ContentStyle::default(), &padding);
                col = indent;
            }
            for (style, part) in source.spans(start..start + grapheme.len()) {
                row.push(style, part);
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

pub(crate) fn truncate_to_width(input: &str, max_width: usize) -> String {
    if display_width(input) <= max_width {
        return input.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let source = StyledText::from_ansi(input);
    let mut col = 0;
    let mut end = 0;
    for (offset, grapheme) in source.text.grapheme_indices(true) {
        let cells = grapheme_width(grapheme, col);
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
