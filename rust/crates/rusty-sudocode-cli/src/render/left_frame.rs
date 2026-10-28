//! Shared scrollback frame layout for tool results and received messages.

use std::{borrow::Cow, fmt::Write as _};

use crossterm::style::Color;

use super::{
    ansi_fg,
    text_layout::{truncate_to_width, wrap_ansi_to_width},
    RESET,
};

pub(crate) fn header_width(width: usize) -> usize {
    width.saturating_sub(3)
}

pub(crate) fn body_width(width: usize) -> usize {
    width.saturating_sub(2).max(1)
}

/// Clip the title to one row and keep every body continuation inside the
/// gutter. There is no right border, so ANSI, tabs and wide text share the
/// same copyable layout. Keep every frame stroke at regular weight: some
/// terminal fonts shift bold rounded corners relative to the vertical bar.
pub(crate) fn render<'a>(
    header: &str,
    bodies: impl IntoIterator<Item = &'a str>,
    width: usize,
    color: Color,
) -> String {
    // Content may leave bold or dim active; the frame owns its font weight.
    let frame = format!("{RESET}{}", ansi_fg(color));
    let header = if header.contains(['\n', '\r', '\t']) {
        Cow::Owned(header.replace(['\n', '\r', '\t'], " "))
    } else {
        Cow::Borrowed(header)
    };
    let mut out = format!(
        "{frame}╭─{RESET} {}",
        truncate_to_width(&header, header_width(width))
    );
    for body in bodies {
        for line in body.split('\n') {
            for row in wrap_ansi_to_width(line, body_width(width)) {
                let _ = write!(out, "\n{frame}│{RESET} {row}");
            }
        }
    }
    let _ = write!(out, "\n{frame}╰─{RESET}");
    out
}
