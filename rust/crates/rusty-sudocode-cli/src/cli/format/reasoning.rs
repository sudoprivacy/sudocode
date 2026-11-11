//! Reasoning: prose the model wrote for itself, shown dim and never parsed as
//! Markdown. The renderer draws it for live turns and transcript replay alike;
//! the margin owner (`ResponseGlyphState`) gives each block its ✻ lead.

use crate::render::{styled_text::StyledText, theme, TextRole};

/// Reasoning text in its theme role.
pub(crate) fn styled(text: &str) -> StyledText {
    StyledText::from_ansi_with_style(text, theme().style(TextRole::Reasoning))
}

/// One complete streamed line. Styled once per line, not per delta, so the
/// scrollback holds one dim run per line instead of a run per chunk; a blank
/// line has nothing to tint and is emitted raw.
pub(crate) fn line(line: &str) -> StyledText {
    if line.trim().is_empty() {
        StyledText::from_ansi(line)
    } else {
        styled(line)
    }
}
