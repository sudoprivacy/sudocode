//! Reasoning: prose the model wrote for itself, shown dim and never parsed as
//! Markdown. Live streaming and replay both render through here; the margin
//! owner (`ResponseGlyphState`) gives each block its ✻ lead.

use crate::render::{styled_text::StyledText, theme, ResponseGlyphState, TextRole};

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

/// A whole reasoning block as replay shows it: laid out with the same ✻ lead
/// and hanging indent the live stream gets.
pub(crate) fn block(text: &str, width: usize) -> String {
    let mut margin = ResponseGlyphState::new(width);
    margin.begin_reasoning();
    let mut output = String::new();
    margin
        .apply_styled(&styled(text))
        .write_ansi_lines(&mut output);
    output
}
