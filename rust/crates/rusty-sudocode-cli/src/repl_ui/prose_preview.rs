//! Disposable prose in the live frame. Final text remains owned by the stream renderer.
//! Source and layout are bounded; unchanged source, width and position reuse cached rows.

use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

use crate::render::{
    code_preview::CodePreview, styled_text::StyledText, ResponseGlyphState, ResponsePosition,
    TerminalRenderer,
};

pub(crate) const MAX_SOURCE_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreviewPosition {
    pub(crate) response: ResponsePosition,
    pub(crate) starts_block: bool,
}

/// Oversized provider packets never allocate an unbounded preview copy.
pub(crate) enum PreviewDelta {
    Text(String),
    Exceeded,
}

impl PreviewDelta {
    pub(crate) fn from_source(source: &str) -> Self {
        if source.len() <= MAX_SOURCE_BYTES {
            Self::Text(source.to_string())
        } else {
            Self::Exceeded
        }
    }
}

pub(crate) enum PreviewUpdate {
    Append {
        delta: PreviewDelta,
        position: PreviewPosition,
    },
    Reset {
        source: String,
        position: PreviewPosition,
        is_frozen: bool,
    },
    Reposition(PreviewPosition),
    Clear,
}

#[derive(Default)]
pub(crate) struct ProsePreview {
    renderer: TerminalRenderer,
    code: CodePreview,
    source: String,
    position: Option<PreviewPosition>,
    gap: &'static str,
    is_dirty: bool,
    is_frozen: bool,
    width: usize,
    rendered: StyledText,
    row_budget: usize,
    visible: Option<Arc<StyledText>>,
}

impl ProsePreview {
    pub(crate) fn position_after_history(
        &mut self,
        history: &crate::render::layout_policy::LayoutPolicy,
    ) -> bool {
        let gap = self
            .position
            .map_or("", |position| history.before_stream(position.starts_block));
        let changed = gap != self.gap;
        self.gap = gap;
        self.is_dirty |= changed;
        changed
    }

    pub(crate) fn update(&mut self, update: PreviewUpdate) {
        match update {
            PreviewUpdate::Append { delta, position } => {
                // Hold the last complete bounded preview once the preview's
                // limit is reached. Never split a delta's grapheme sequence or
                // discard canonical source: MarkdownStreamState retains it.
                if self.is_frozen {
                    return;
                }
                let PreviewDelta::Text(delta) = delta else {
                    self.is_frozen = true;
                    return;
                };
                if self.source.len().saturating_add(delta.len()) <= MAX_SOURCE_BYTES {
                    let previous_len = self.source.len();
                    self.source.push_str(&delta);
                    if !delta.is_ascii() && has_ambiguous_width(&self.source) {
                        // Keep the safe prefix visible instead of removing it
                        // when an arriving grapheme changes width semantics.
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
            PreviewUpdate::Reset {
                source,
                position,
                is_frozen,
            } => {
                self.clear();
                self.is_frozen = is_frozen;
                if source.len() <= MAX_SOURCE_BYTES {
                    self.source = source;
                    self.position = Some(position);
                    self.is_dirty = true;
                }
            }
            PreviewUpdate::Reposition(position) => {
                // A tool/notice changes where prose is displayed, not the
                // provider's source or its frozen preview prefix.
                if self.position != Some(position) {
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
        self.code = CodePreview::default();
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
            let markdown = if has_ambiguous_width(&self.source) {
                None
            } else {
                self.code.render(&self.source, &self.renderer).or_else(|| {
                    is_prose(&self.source).then(|| {
                        self.renderer.render_markdown_styled_with_width(
                            &self.source,
                            width.saturating_sub(2),
                        )
                    })
                })
            };
            if let (Some(position), Some(markdown)) = (self.position, markdown) {
                let mut glyph = ResponseGlyphState::from_position(width, position.response);
                self.rendered.plain(self.gap);
                self.rendered.append(&glyph.apply_styled(&markdown));
                // MixedText's measure uses str::lines: the last terminator
                // advances the cursor but owns no extra row. Keep both
                // paragraph terminators so history and the canvas each own
                // the same blank separator row.
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
                visible.append_range(&self.rendered, start..self.rendered.text.len());
                self.visible = Some(Arc::new(visible));
            }
        }
        self.visible.clone()
    }
}

fn is_prose(source: &str) -> bool {
    has_prose_structure(source) && !has_ambiguous_width(source)
}

/// Conservative shape gate for live prose; the stream parser owns final blocks.
#[inline]
fn has_prose_structure(source: &str) -> bool {
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

/// The live canvas and history measure clusters by one rule, the one
/// measured at startup (`render::grapheme_probe`). Legacy Windows console
/// hosts can still miscount combining sequences against both, so their text
/// keeps the complete-block path.
fn has_ambiguous_width(source: &str) -> bool {
    cfg!(windows)
        && !source.is_ascii()
        && source.graphemes(true).any(|grapheme| {
            grapheme.chars().nth(1).is_some()
                && grapheme != "\r\n"
                && iocraft::width::grapheme_columns(grapheme) > 0
        })
}
