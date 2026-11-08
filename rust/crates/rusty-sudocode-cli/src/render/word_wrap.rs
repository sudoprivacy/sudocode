//! Source-preserving word wrapping adapted from openai/codex 970111f6cb wrapping.rs.
//! Copyright 2026 OpenAI. Apache-2.0; see third_party/codex.
//! Unicode/hyphen break decisions remain upstream's; styles and links stay in StyledText.

use super::{styled_text::StyledText, text_layout::display_width};
use std::ops::Range;
use textwrap::{core::Word, word_splitters::split_words, Options};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) fn wrap_words(source: &StyledText, width: usize) -> Vec<StyledText> {
    let mut rows = Vec::new();
    let mut base = 0;
    for raw in source.text.split('\n') {
        let logical = raw.trim_end_matches('\r');
        let options = Options::new(width.max(1)).wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
        rows.extend(
            wrap_ranges_trim(logical, options)
                .into_iter()
                .map(|range| source.slice(base + range.start..base + range.end)),
        );
        base += raw.len() + 1;
    }
    if rows.is_empty() {
        rows.push(StyledText::default());
    }
    rows
}

struct ProjectedText {
    text: String,
    source_boundaries: Vec<(usize, usize)>,
    grapheme_boundaries: Vec<usize>,
}

fn project_complex_graphemes(text: &str) -> Option<ProjectedText> {
    if !text.contains(['\u{FF9E}', '\u{FF9F}'])
        && !text
            .graphemes(/*is_extended*/ true)
            .any(|grapheme| grapheme.chars().count() > 1)
    {
        return None;
    }

    let mut projected = String::with_capacity(text.len());
    let mut source_boundaries = vec![(0, 0)];
    let mut grapheme_boundaries = vec![0];
    for (source_start, grapheme) in text.grapheme_indices(/*is_extended*/ true) {
        if grapheme.chars().count() > 1 || grapheme.contains(['\u{FF9E}', '\u{FF9F}']) {
            let source_end = source_start + grapheme.len();
            let content_start = grapheme
                .find(|ch: char| !ch.is_whitespace())
                .unwrap_or(grapheme.len());
            let (whitespace, content) = grapheme.split_at(content_start);
            for (offset, ch) in whitespace.char_indices() {
                projected.push(ch);
                source_boundaries.push((projected.len(), source_start + offset + ch.len_utf8()));
                grapheme_boundaries.push(projected.len());
            }

            let width = display_width(content);
            if width == 0 && !content.is_empty() {
                projected.push_str(content);
                source_boundaries.push((projected.len(), source_end));
            }
            let projected_start = projected.len();
            for _ in 0..width / 2 {
                if projected.len() > projected_start {
                    projected.push('\u{2060}');
                }
                projected.push('界');
                source_boundaries.push((projected.len(), source_end));
            }
            if width % 2 == 1 {
                if projected.len() > projected_start {
                    projected.push('\u{2060}');
                }
                projected.push('a');
                source_boundaries.push((projected.len(), source_end));
            }
        } else {
            for (offset, ch) in grapheme.char_indices() {
                projected.push(ch);
                source_boundaries.push((projected.len(), source_start + offset + ch.len_utf8()));
            }
        }
        grapheme_boundaries.push(projected.len());
    }

    Some(ProjectedText {
        text: projected,
        source_boundaries,
        grapheme_boundaries,
    })
}

fn source_offset(boundaries: &[(usize, usize)], projected_offset: usize) -> usize {
    boundaries
        .binary_search_by_key(&projected_offset, |(offset, _)| *offset)
        .map_or(projected_offset, |index| boundaries[index].1)
}

fn break_projected_words<'a>(
    words: impl Iterator<Item = Word<'a>>,
    projected: &'a ProjectedText,
    line_width: usize,
) -> Vec<Word<'a>> {
    let projected_start = projected.text.as_ptr() as usize;
    let mut pieces = Vec::new();

    for word in words {
        if display_width(word.word) <= line_width {
            pieces.push(word);
            continue;
        }

        let word_start = word.word.as_ptr() as usize - projected_start;
        let word_end = word_start + word.word.len();
        let mut piece_start = word_start;
        let mut piece_width = 0;
        let mut atom_start = word_start;
        let boundary_start = projected
            .grapheme_boundaries
            .partition_point(|atom_end| *atom_end <= word_start);

        for atom_end in projected
            .grapheme_boundaries
            .iter()
            .copied()
            .skip(boundary_start)
        {
            if atom_end > word_end {
                break;
            }

            let atom_width = display_width(&projected.text[atom_start..atom_end]);
            if piece_width > 0 && piece_width + atom_width > line_width {
                pieces.push(Word::from(&projected.text[piece_start..atom_start]));
                piece_start = atom_start;
                piece_width = 0;
            }
            piece_width += atom_width;
            atom_start = atom_end;
        }

        let mut last = Word::from(&projected.text[piece_start..word_end]);
        last.whitespace = word.whitespace;
        last.penalty = word.penalty;
        pieces.push(last);
    }

    pieces
}

fn wrap_projected_ranges(
    projected: &ProjectedText,
    opts: &Options<'_>,
    include_trailing_spaces: bool,
) -> Vec<Range<usize>> {
    let line_widths = [
        opts.width
            .saturating_sub(display_width(opts.initial_indent)),
        opts.width
            .saturating_sub(display_width(opts.subsequent_indent)),
    ];
    let line_ending = opts.line_ending.as_str();
    let mut ranges = Vec::new();
    let mut line_start = 0;

    for line in projected.text.split(line_ending) {
        let words = opts.word_separator.find_words(line);
        let split_words = split_words(words, &opts.word_splitter);
        let mut broken_words = if opts.break_words {
            break_projected_words(split_words, projected, line_widths[1])
        } else {
            split_words.collect()
        };
        if opts.break_words && !opts.initial_indent.is_empty() {
            broken_words.insert(0, Word::from(""));
        }

        let wrapped_words = opts.wrap_algorithm.wrap(&broken_words, &line_widths);
        let mut cursor = line_start;
        for words in wrapped_words {
            let Some(last_word) = words.last() else {
                let source = source_offset(&projected.source_boundaries, cursor);
                ranges.push(source..source + usize::from(include_trailing_spaces));
                continue;
            };
            let len = words
                .iter()
                .map(|word| word.word.len() + word.whitespace.len())
                .sum::<usize>()
                - last_word.whitespace.len();
            let end = cursor + len;
            let trailing_spaces = if include_trailing_spaces {
                projected.text[end..]
                    .chars()
                    .take_while(|ch| *ch == ' ')
                    .count()
            } else {
                0
            };
            let source_start = source_offset(&projected.source_boundaries, cursor);
            let source_end = source_offset(&projected.source_boundaries, end + trailing_spaces);
            ranges.push(source_start..source_end + usize::from(include_trailing_spaces));
            cursor = end + last_word.whitespace.len();
        }
        line_start += line.len() + line_ending.len();
    }

    ranges
}

fn wrap_ranges_trim<'a, O>(text: &str, width_or_options: O) -> Vec<Range<usize>>
where
    O: Into<Options<'a>>,
{
    let opts = width_or_options.into();
    if let Some(projected) = project_complex_graphemes(text) {
        return wrap_projected_ranges(&projected, &opts, /*include_trailing_spaces*/ false);
    }
    let mut lines: Vec<Range<usize>> = Vec::new();
    let mut cursor = 0usize;
    for (line_index, line) in textwrap::wrap(text, &opts).iter().enumerate() {
        match line {
            std::borrow::Cow::Borrowed(slice) => {
                let range = borrowed_slice_range(text, slice).unwrap_or_else(|| {
                    let synthetic_prefix = if line_index == 0 {
                        opts.initial_indent
                    } else {
                        opts.subsequent_indent
                    };
                    map_owned_wrapped_line_to_range(text, cursor, slice, synthetic_prefix)
                });
                cursor = range.end;
                lines.push(range);
            }
            std::borrow::Cow::Owned(slice) => {
                let synthetic_prefix = if line_index == 0 {
                    opts.initial_indent
                } else {
                    opts.subsequent_indent
                };
                let mapped = map_owned_wrapped_line_to_range(text, cursor, slice, synthetic_prefix);
                lines.push(mapped.clone());
                cursor = mapped.end;
            }
        }
    }
    lines
}

fn borrowed_slice_range(text: &str, slice: &str) -> Option<Range<usize>> {
    let text_start = text.as_ptr() as usize;
    let text_end = text_start.checked_add(text.len())?;
    let slice_start = slice.as_ptr() as usize;
    let slice_end = slice_start.checked_add(slice.len())?;

    if slice_start < text_start || slice_end > text_end {
        return None;
    }

    Some((slice_start - text_start)..(slice_end - text_start))
}

fn map_owned_wrapped_line_to_range(
    text: &str,
    cursor: usize,
    wrapped: &str,
    synthetic_prefix: &str,
) -> Range<usize> {
    let wrapped = if synthetic_prefix.is_empty() {
        wrapped
    } else {
        wrapped.strip_prefix(synthetic_prefix).unwrap_or(wrapped)
    };

    let mut start = cursor;
    while start < text.len() && !wrapped.starts_with(' ') {
        let Some(ch) = text[start..].chars().next() else {
            break;
        };
        if ch != ' ' {
            break;
        }
        start += ch.len_utf8();
    }

    let mut end = start;
    let mut saw_source_char = false;
    let mut chars = wrapped.chars().peekable();
    while let Some(ch) = chars.next() {
        if end < text.len() {
            let Some(src) = text[end..].chars().next() else {
                unreachable!("checked end < text.len()");
            };
            if ch == src {
                end += src.len_utf8();
                saw_source_char = true;
                continue;
            }
        }

        // textwrap can materialize owned lines when penalties are inserted.
        // The default penalty is a trailing '-'; it does not correspond to
        // source bytes, so we skip it while keeping byte ranges in source text.
        if ch == '-' && chars.peek().is_none() {
            continue;
        }

        // Non-source chars can be synthesized by textwrap in owned output
        // (e.g. non-space indent prefixes). Keep going and map the source bytes
        // we can confidently match instead of crashing the app.
        if !saw_source_char {
            continue;
        }

        // Preserve the confidently matched source range. Rendering stays pure
        // and never sends source contents to a diagnostic sink.
        break;
    }

    start..end
}
