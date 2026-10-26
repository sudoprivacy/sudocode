//! PendingSlot message presentation. Queue ownership and row allocation stay
//! with the coordinator/layout; this is only a bounded, one-line projection.

use crate::render::styled_text::StyledText;
use crate::render::text_layout::{display_width, truncate_to_width};
use crate::render::{ColorSupport, ColorTheme, RESET};

const LABEL: &str = "↳ queued: ";
const COMPACT_LABEL: &str = "↳ ";

pub(super) fn preview(display: String, source: engine_events::InputSource) -> String {
    if source == engine_events::InputSource::Peer {
        crate::cli::received::queued_preview(&display)
    } else {
        display
    }
}

pub(super) fn render(
    display: &str,
    steering: bool,
    width: usize,
    support: ColorSupport,
    theme: &ColorTheme,
) -> String {
    let full_label = if steering { "↳ steering: " } else { LABEL };
    if width == 0 {
        return String::new();
    }
    // Keep at least half the row for the actual message on narrow terminals.
    let label = if width < 2 * display_width(full_label) {
        COMPACT_LABEL
    } else {
        full_label
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
