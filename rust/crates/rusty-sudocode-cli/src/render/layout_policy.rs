//! Transcript block spacing, shared by streaming, replay and input echoes.
//! Content owns its internal whitespace; this policy owns only block edges.

/// Incremental state for already-emitted output. Only the new chunk's trailing
/// newlines are inspected, never the transcript, Markdown or terminal styles.
#[derive(Default)]
pub(crate) struct LayoutPolicy {
    has_content: bool,
    trailing_newlines: usize,
}

impl LayoutPolicy {
    const BLOCK_BREAK: &'static str = "\n\n";

    /// Add one blank row between independent blocks, accounting for any
    /// line endings already emitted by the preceding block.
    #[inline]
    pub(crate) fn before_block(&self) -> &'static str {
        if self.has_content {
            &Self::BLOCK_BREAK[self.trailing_newlines.min(Self::BLOCK_BREAK.len())..]
        } else {
            ""
        }
    }

    #[inline]
    pub(crate) fn observe(&mut self, text: &str) {
        let content = text.trim_end_matches('\n');
        let trailing = text.len() - content.len();
        if content.is_empty() {
            self.trailing_newlines = self.trailing_newlines.saturating_add(trailing);
        } else {
            self.has_content = true;
            self.trailing_newlines = trailing;
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

    /// Complete an echo/turn so the next block starts with the same gap as
    /// replay. Kept separate from internal content and from the live chrome.
    #[inline]
    pub(crate) fn finish_block(output: &mut String) {
        output.truncate(output.trim_end_matches('\n').len());
        if !output.is_empty() {
            output.push_str(Self::BLOCK_BREAK);
        }
    }
}
