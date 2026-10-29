//! Bounded open-fence preview. The stream renderer retains canonical Markdown.
//! Complete lines share the finalized highlighter; partial lines never advance its state.

use super::{
    code_theme::{self, StreamingCodeHighlighter},
    fence_presentation,
    styled_text::StyledText,
    FencePresentation, TerminalRenderer,
};
use crossterm::style::ContentStyle;

#[derive(Default)]
pub(crate) struct CodePreview {
    opener: String,
    parser: Option<StreamingCodeHighlighter>,
    complete: StyledText,
    complete_bytes: usize,
    fallback: bool,
}

impl CodePreview {
    pub(crate) fn render(&mut self, source: &str, renderer: &TerminalRenderer) -> Option<String> {
        let (opener, body) = source.split_once('\n')?;
        let marker = super::parse_fence_opener(opener)?;
        // Nested/indented fences keep the canonical parser's normalization.
        // The preview can disappear; immutable scrollback is never rewritten.
        if !opener.starts_with(marker.character)
            || body
                .lines()
                .any(|line| super::parse_fence_opener(line).is_some())
        {
            return None;
        }
        let language = opener[marker.length..].trim();
        if fence_presentation(language) != FencePresentation::Source {
            return None;
        }
        if self.opener != opener || self.complete_bytes > body.len() {
            *self = Self::default();
            self.opener = opener.to_string();
            self.parser = Some(StreamingCodeHighlighter::new(
                language,
                renderer.color_theme.light_background,
                renderer.color_support,
            ));
        }
        if !code_theme::within_limits(body) {
            self.fallback = true;
            self.complete.clear_styles();
        }
        let added = &body[self.complete_bytes..];
        if let Some(end) = added.rfind('\n') {
            let added = &added[..=end];
            let highlighted = (!self.fallback)
                .then(|| self.parser.as_mut()?.append(&self.complete.text, added))
                .flatten();
            if let Some(highlighted) = highlighted {
                self.complete.append(&highlighted);
            } else {
                self.fallback = true;
                self.complete.clear_styles();
                self.complete.push(ContentStyle::default(), added);
            }
            self.complete_bytes += added.len();
        }
        let mut output = String::new();
        self.complete
            .write_ansi(0..self.complete.text.len(), &mut output);
        output.push_str(&body[self.complete_bytes..]);
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
        Some(output)
    }
}
