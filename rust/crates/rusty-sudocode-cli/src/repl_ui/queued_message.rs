//! PendingSlot message presentation. Queue ownership and row allocation stay
//! with the coordinator/layout; this is only a bounded, one-line projection.

use crate::render::styled_text::StyledText;
use crate::render::text_layout::{display_width, truncate_to_width};
use crate::render::{ColorSupport, ColorTheme, RESET};

const LABEL: &str = "↳ queued: ";
const COMPACT_LABEL: &str = "↳ ";

pub(super) fn render(
    display: &str,
    width: usize,
    support: ColorSupport,
    theme: &ColorTheme,
) -> String {
    if width == 0 {
        return String::new();
    }
    // Keep at least half the row for the actual message on narrow terminals.
    let label = if width < 2 * display_width(LABEL) {
        COMPACT_LABEL
    } else {
        LABEL
    };
    let first = display
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    if support == ColorSupport::NoColor {
        let plain = StyledText::from_ansi(first).text;
        return truncate_to_width(&format!("{label}{plain}"), width);
    }
    let line = format!("{RESET}{}{label}{RESET}{first}{RESET}", theme.muted_fg());
    // A clipped preview must not leave an inherited style on the next item.
    format!("{}{RESET}", truncate_to_width(&line, width))
}
