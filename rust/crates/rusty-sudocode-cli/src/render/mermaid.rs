//! One terminal adapter for the pinned Codex engine. The transcript keeps the
//! original Markdown; only completed fences can replace source on screen.

use std::borrow::Cow;
use std::ops::Range;

use codex_mermaid::{RenderError, Role};
use crossterm::style::Stylize;

use super::{CodeBlockKind, ColorTheme, Event, Options, Parser, Tag, TagEnd};

pub(super) fn is_mermaid(info: &str) -> bool {
    info.split([',', ' ', '\t']).next() == Some("mermaid")
}

// Same source-offset check as Codex's markdown_render/mermaid.rs: CommonMark
// emits End(CodeBlock) at EOF even when no actual closing fence was received.
pub(super) fn has_closing_fence(input: &str, range: Range<usize>, content_end: usize) -> bool {
    let Some(block) = input.get(range.clone()) else {
        return false;
    };
    let Some(marker @ (b'`' | b'~')) = block.as_bytes().first().copied() else {
        return false;
    };
    let opening_len = block.bytes().take_while(|byte| *byte == marker).count();
    let Some(suffix) = input.get(content_end..range.end) else {
        return false;
    };
    suffix
        .trim_end_matches([' ', '\t', '\r', '\n'])
        .bytes()
        .rev()
        .take_while(|byte| *byte == marker)
        .count()
        >= opening_len
}

/// A blank line inside a nested fence is not a scrollback boundary. Keep its
/// enclosing CommonMark block intact so final rendering retains list/quote context.
pub(super) fn stream_boundary(source: &str, boundary: Option<usize>) -> Option<usize> {
    let boundary = boundary?;
    if !source.contains("mermaid") {
        return Some(boundary);
    }
    let mut depth = 0usize;
    let mut block_start = 0;
    let mut fence = None;
    let mut content_end = 0;
    for (event, range) in Parser::new_ext(source, Options::all()).into_offset_iter() {
        match &event {
            Event::Start(tag) => {
                if depth == 0 {
                    block_start = range.start;
                }
                if let Tag::CodeBlock(CodeBlockKind::Fenced(info)) = tag {
                    if is_mermaid(info) {
                        content_end = range.start;
                        fence = Some(range.clone());
                    }
                }
                depth += 1;
            }
            Event::Text(_) if fence.is_some() => content_end = range.end,
            Event::End(TagEnd::CodeBlock) if fence.is_some() => {
                let fence = fence.take().expect("Mermaid fence");
                if boundary > block_start
                    && (boundary < fence.end
                        || (boundary == fence.end && !source[..boundary].ends_with('\n'))
                        || !has_closing_fence(source, fence, content_end))
                {
                    return (block_start > 0).then_some(block_start);
                }
                depth = depth.saturating_sub(1);
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Some(boundary)
}

pub(super) fn render(
    source: &str,
    width: usize,
    theme: &ColorTheme,
) -> Result<String, RenderError> {
    let mut output = String::new();
    for line in codex_mermaid::render_spans(source, width)? {
        for span in line {
            match span.role {
                Role::Node => output.push_str(&span.text.with(theme.primary).to_string()),
                Role::Edge => output.push_str(&span.text.with(theme.border).to_string()),
                Role::Text => output.push_str(&span.text),
            }
        }
        output.push('\n');
    }
    Ok(output)
}

/// Rejected or unfinished source is readable text, never an escape-sequence
/// fallback around the engine's terminal-safety check. Stored source is intact.
pub(super) fn safe_source(source: &str) -> Cow<'_, str> {
    if !source
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return Cow::Borrowed(source);
    }
    Cow::Owned(
        source
            .chars()
            .flat_map(|ch| {
                if ch.is_control() && !matches!(ch, '\n' | '\t') {
                    ch.escape_default().collect::<Vec<_>>()
                } else {
                    vec![ch]
                }
            })
            .collect(),
    )
}
