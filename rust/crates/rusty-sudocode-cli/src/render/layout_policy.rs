//! Transcript block spacing, shared by streaming, replay and input echoes.
//! Content owns its internal whitespace; this policy owns only block edges.

/// Rendered content plus its relationship to the preceding history.
pub(crate) enum HistoryPart {
    Stream { text: String, starts_block: bool },
    Block(String),
    Finish,
}

/// Incremental state for already-emitted output. Only the new chunk's trailing
/// newlines are inspected, never the transcript, Markdown or terminal styles.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LayoutPolicy {
    has_content: bool,
    trailing_newlines: usize,
    stream_open: bool,
}

impl LayoutPolicy {
    const BLOCK_BREAK: &'static str = "\n\n";

    /// Add one blank row between independent blocks, accounting for any
    /// line endings already emitted by the preceding block.
    #[inline]
    fn before_block(&self) -> &'static str {
        if self.has_content {
            &Self::BLOCK_BREAK[self.trailing_newlines.min(Self::BLOCK_BREAK.len())..]
        } else {
            ""
        }
    }

    pub(crate) fn before_stream(&self, starts_block: bool) -> &'static str {
        if starts_block || !self.stream_open {
            self.before_block()
        } else {
            ""
        }
    }

    #[inline]
    fn observe(&mut self, text: &str) {
        let content = text.trim_end_matches('\n');
        let trailing = text.len() - content.len();
        if content.is_empty() {
            self.trailing_newlines = self.trailing_newlines.saturating_add(trailing);
        } else {
            self.has_content = true;
            self.trailing_newlines = trailing;
        }
    }

    pub(crate) fn observe_external(&mut self, text: &str) {
        self.observe(text);
        if !text.trim_end_matches('\n').is_empty() {
            self.stream_open = false;
        }
    }

    /// Emit a logical block edge or a continuation through the same sink.
    /// A preview can query the next edge without committing it to history.
    pub(crate) fn write(&mut self, part: HistoryPart, mut emit: impl FnMut(&str)) {
        match part {
            HistoryPart::Stream { text, starts_block } if !text.is_empty() => {
                self.emit(self.before_stream(starts_block), &mut emit);
                self.emit(&text, &mut emit);
                self.stream_open = true;
            }
            HistoryPart::Block(text) => {
                let text = text.trim_matches('\n');
                if !text.is_empty() {
                    self.emit(self.before_block(), &mut emit);
                    self.emit(text, &mut emit);
                    self.emit("\n", &mut emit);
                    self.stream_open = false;
                }
            }
            HistoryPart::Finish => {
                self.emit(self.before_block(), &mut emit);
                self.stream_open = false;
            }
            HistoryPart::Stream { .. } => {}
        }
    }

    fn emit(&mut self, text: &str, emit: &mut impl FnMut(&str)) {
        if !text.is_empty() {
            emit(text);
            self.observe(text);
        }
    }

    /// Join complete blocks without accumulating their presentation newlines.
    /// No whitespace inside a block (including code/diff/log rows) is changed.
    #[inline]
    pub(crate) fn append_block(output: &mut String, block: &str) {
        let block = block.trim_matches('\n');
        if block.is_empty() {
            return;
        }
        output.truncate(output.trim_end_matches('\n').len());
        let mut layout = Self::default();
        layout.observe(output);
        output.push_str(layout.before_block());
        output.push_str(block);
    }
}
