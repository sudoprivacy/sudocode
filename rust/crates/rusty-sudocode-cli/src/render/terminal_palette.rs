//! One startup palette probe, outside the render/input hot paths.

use std::io::{self, IsTerminal};
use std::sync::OnceLock;
use std::time::Duration;

type Rgb = (u8, u8, u8);
static COLORS: OnceLock<Option<(Rgb, Rgb)>> = OnceLock::new();

/// Called before the default REPL starts its input reader. The probe and the
/// REPL use iocraft's same crossterm event queue, preserving early keys/pastes.
pub(crate) fn initialize() {
    COLORS.get_or_init(|| {
        if !io::stdin().is_terminal()
            || !io::stdout().is_terminal()
            || super::ColorSupport::detect() == super::ColorSupport::NoColor
        {
            return None;
        }
        iocraft::query_terminal_colors(Duration::from_millis(250))
            .ok()
            .flatten()
            .map(|colors| (colors.foreground, colors.background))
    });
}

#[inline]
pub(super) fn background() -> Option<Rgb> {
    COLORS.get().copied().flatten().map(|(_, bg)| bg)
}

/// Match Codex's adaptive theme selection threshold.
#[inline]
pub(super) fn is_light((r, g, b): Rgb) -> bool {
    0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b) > 128.0
}
