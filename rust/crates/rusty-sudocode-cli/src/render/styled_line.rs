//! Compose chrome from independently styled spans, never nested ANSI strings.
//!
//! A child RESET cannot restore an enclosing style. Keep styles as data until
//! the final ANSI boundary instead; each span specifies its complete style.
//! Direct terminal output serializes ANSI; the REPL consumes structured spans.

use std::fmt;

use crossterm::style::{Color, ContentStyle, Stylize};

use super::{theme, RESET};

#[derive(Clone, Debug)]
pub(crate) struct StyledLine {
    base: ContentStyle,
    spans: Vec<(ContentStyle, String)>,
}

impl StyledLine {
    pub(crate) fn structured(&self) -> super::styled_text::StyledText {
        let mut text = super::styled_text::StyledText::default();
        for (style, content) in &self.spans {
            text.push(*style, content);
        }
        text
    }

    /// Low-emphasis chrome uses the theme's muted foreground, not faint on
    /// top of an already muted color. This is shared by summaries and status.
    pub(crate) fn muted() -> Self {
        Self {
            base: ContentStyle::new().with(theme().muted),
            spans: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, text: impl Into<String>) {
        self.push_style(text.into(), self.base);
    }

    pub(crate) fn push_bold(&mut self, text: impl Into<String>) {
        self.push_style(text.into(), self.base.bold());
    }

    pub(crate) fn push_colored(&mut self, text: impl Into<String>, color: Color) {
        self.push_style(text.into(), self.base.with(color));
    }

    pub(crate) fn append(&mut self, other: Self) {
        for (style, text) in other.spans {
            self.push_style(text, style);
        }
    }

    // Text arguments are unstyled content. Compose rich fragments with append,
    // not by embedding a serialized StyledLine in another span's text.
    fn push_style(&mut self, text: String, style: ContentStyle) {
        if text.is_empty() {
            return;
        }
        if let Some((previous_style, previous_text)) = self.spans.last_mut() {
            if *previous_style == style {
                previous_text.push_str(&text);
                return;
            }
        }
        self.spans.push((style, text));
    }
}

impl fmt::Display for StyledLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (style, text) in &self.spans {
            // Establish a complete style even if the preceding output was dim
            // or bold. Crossterm owns attribute/color encoding and cleanup.
            write!(f, "{RESET}{}", style.apply(text))?;
        }
        Ok(())
    }
}
