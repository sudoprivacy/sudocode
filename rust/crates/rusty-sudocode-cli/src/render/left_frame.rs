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

/// How a frame's last row ends. Something still in progress stays open with a
/// dashed `╎` ("continues"); a finished block closes with `╰─`. Both are one
/// row, so a block keeps its height when it closes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tail {
    Closed,
    Open,
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
    tail: Tail,
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
    let tail = match tail {
        Tail::Closed => "╰─",
        Tail::Open => "╎",
    };
    let _ = write!(out, "\n{frame}{tail}{RESET}");
    out
}
