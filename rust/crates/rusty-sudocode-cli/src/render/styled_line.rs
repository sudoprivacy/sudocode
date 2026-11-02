//! Compose chrome from independently styled spans, never nested ANSI strings.
//!
//! A child RESET cannot restore an enclosing style. Keep styles as data until
//! the final ANSI boundary instead; each span specifies its complete style.
//! Direct terminal output serializes ANSI; the REPL consumes structured spans.

use std::fmt;

use crossterm::style::{ContentStyle, Stylize};

use super::{styled_text::StyledText, theme, TextRole};

#[derive(Clone, Debug)]
pub(crate) struct StyledLine {
    base: ContentStyle,
    text: StyledText,
}

impl StyledLine {
    pub(crate) fn structured(&self) -> super::styled_text::StyledText {
        self.text.clone()
    }

    /// Low-emphasis chrome uses the theme's muted foreground, not faint on
    /// top of an already muted color. This is shared by summaries and status.
    pub(crate) fn muted() -> Self {
        Self {
            base: theme().style(TextRole::Secondary),
            text: StyledText::default(),
        }
    }

    #[inline]
    pub(crate) fn push(&mut self, text: impl AsRef<str>) {
        self.text.push(self.base, text.as_ref());
    }

    pub(crate) fn push_bold(&mut self, text: impl AsRef<str>) {
        self.text.push(self.base.bold(), text.as_ref());
    }

    /// Quantities use terminal-owned foreground emphasis, not a brand/status
    /// color or the surrounding muted label style. Zero has the same role.
    pub(crate) fn push_count(&mut self, count: usize) {
        self.text
            .push(theme().style(TextRole::Strong), &count.to_string());
    }

    #[inline]
    pub(crate) fn push_role(&mut self, text: impl AsRef<str>, role: TextRole) {
        self.text.push(theme().style(role), text.as_ref());
    }

    pub(crate) fn append(&mut self, other: Self) {
        if self.text.text.is_empty() {
            self.text = other.text;
        } else {
            self.text.append(&other.text);
        }
    }
}

impl fmt::Display for StyledLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.text.fmt_ansi(0..self.text.text.len(), f)
    }
}
