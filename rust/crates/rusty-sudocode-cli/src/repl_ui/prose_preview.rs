//! Disposable prose in the live frame. Final text remains owned by the stream renderer.
//! Source and layout are bounded; only a new source or width reparses Markdown.

use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::{
    styled_text::StyledText, ResponseGlyphState, ResponsePosition, TerminalRenderer,
};

pub(crate) const MAX_SOURCE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewPosition {
    pub(crate) response: ResponsePosition,
    pub(crate) gap: &'static str,
}

pub(crate) enum PreviewUpdate {
    Append {
        delta: String,
        position: PreviewPosition,
    },
    Reset {
        source: String,
        position: PreviewPosition,
    },
    Clear,
}

#[derive(Default)]
pub(crate) struct ProsePreview {
    renderer: TerminalRenderer,
    source: String,
    position: Option<PreviewPosition>,
    is_dirty: bool,
    is_frozen: bool,
    width: usize,
    rendered: StyledText,
    row_budget: usize,
    visible: Option<Arc<StyledText>>,
}

impl ProsePreview {
    pub(crate) fn update(&mut self, update: PreviewUpdate) {
        match update {
            PreviewUpdate::Append { delta, position } => {
                // Hold the last complete bounded preview once this prototype's
                // limit is reached. Never split a delta's grapheme sequence or
                // discard canonical source: MarkdownStreamState retains it.
                if self.is_frozen {
                    return;
                }
                if self.source.len().saturating_add(delta.len()) <= MAX_SOURCE_BYTES {
                    let previous_len = self.source.len();
                    self.source.push_str(&delta);
                    if !delta.is_ascii() && has_ambiguous_width(&self.source) {
                        // Keep the safe prefix visible instead of removing it
                        // when an arriving emoji changes width semantics.
                        self.source.truncate(previous_len);
                        self.is_frozen = true;
                        return;
                    }
                    self.position = Some(position);
                    self.is_dirty = true;
                } else {
                    self.is_frozen = true;
                }
            }
            PreviewUpdate::Reset { source, position } => {
                self.clear();
                if source.len() <= MAX_SOURCE_BYTES {
                    self.source = source;
                    self.position = Some(position);
                    self.is_dirty = true;
                }
            }
            PreviewUpdate::Clear => self.clear(),
        }
    }

    fn clear(&mut self) {
        self.source.clear();
        self.position = None;
        self.rendered = StyledText::default();
        self.visible = None;
        self.is_dirty = false;
        self.is_frozen = false;
    }

    pub(crate) fn is_active(&self) -> bool {
        !self.source.is_empty()
    }

    pub(crate) fn layout(&mut self, width: usize, row_budget: usize) -> Option<Arc<StyledText>> {
        let is_changed = self.is_dirty || self.width != width;
        if is_changed {
            self.width = width;
            self.is_dirty = false;
            self.rendered = StyledText::default();
            if let Some(position) = self.position.filter(|_| is_prose(&self.source)) {
                let markdown = self
                    .renderer
                    .render_markdown_with_width(&self.source, width.saturating_sub(2));
                let mut glyph = ResponseGlyphState::from_position(width, position.response);
                let mut text = position.gap.to_string();
                text.push_str(&glyph.apply(&markdown));
                // MixedText's measure uses str::lines: the last terminator
                // advances the cursor but owns no extra row. Keep both
                // paragraph terminators so history and the canvas each own
                // the same blank separator row.
                self.rendered = StyledText::from_ansi(&text);
            }
        }
        if is_changed || self.row_budget != row_budget {
            self.row_budget = row_budget;
            self.visible = None;
            if row_budget > 0 && !self.rendered.text.is_empty() {
                let rows = self.rendered.text.lines().count();
                let start = self
                    .rendered
                    .text
                    .split_inclusive('\n')
                    .take(rows.saturating_sub(row_budget))
                    .map(str::len)
                    .sum();
                let mut visible = StyledText::default();
                for (style, part) in self.rendered.spans(start..self.rendered.text.len()) {
                    visible.push(style, part);
                }
                self.visible = Some(Arc::new(visible));
            }
        }
        self.visible.clone()
    }
}

fn is_prose(source: &str) -> bool {
    if source.trim().is_empty() || source.contains('|') {
        return false;
    }
    // Some terminals count emoji sequences as individual scalar widths.
    // A history write can then wrap where the live canvas clips. Until that
    // capability is negotiated, use the complete-block path for such text.
    if has_ambiguous_width(source) {
        return false;
    }
    has_prose_lines(source)
}

/// Only prose gets an extra boundary before asynchronous progress output.
/// Fences and other structured blocks keep the original stream parser state.
pub(crate) fn has_prose_structure(source: &str) -> bool {
    !source.trim().is_empty() && !source.contains('|') && has_prose_lines(source)
}

fn has_prose_lines(source: &str) -> bool {
    source.lines().all(|line| {
        if line.is_empty() {
            return true;
        }
        if line.starts_with([' ', '\t', '>', '#', '`', '~', '<'])
            || line.starts_with(['-', '*', '+'])
        {
            return false;
        }
        !line.split_once(". ").is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn has_ambiguous_width(source: &str) -> bool {
    !source.is_ascii()
        && source.graphemes(true).any(|grapheme| {
            grapheme.width()
                != grapheme
                    .chars()
                    .map(|ch| ch.width().unwrap_or(0))
                    .sum::<usize>()
        })
}
