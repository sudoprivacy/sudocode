//! Windows console virtual-terminal state: enable it once, then answer "is it
//! on" from one place.
//!
//! Windows consoles need `ENABLE_VIRTUAL_TERMINAL_PROCESSING` before they
//! interpret ANSI escapes, and they do not set `TERM` — that is a POSIX
//! convention. Code that infers "can this terminal do color?" from `TERM`
//! therefore concludes *no* on every Windows machine. The console's own VT
//! state is the honest signal, so it lives here and both the startup enable
//! and the color-tier detection read it from this module rather than
//! re-deriving it.
//!
//! On non-Windows platforms escapes need no enabling, so the query is a
//! compile-time `true`.

/// Enable virtual-terminal processing on the current console, once per process.
///
/// Must run before any raw escape is written — the banner and other early
/// output are emitted long before the first spinner would have triggered
/// crossterm's own lazy enable.
#[inline]
pub fn enable_vt_processing() {
    #[cfg(windows)]
    {
        let _ = vt_processing_enabled();
    }
}

/// Whether the console interprets ANSI escapes.
///
/// On Windows the first call also performs the enable (crossterm's
/// `supports_ansi` sets `ENABLE_VIRTUAL_TERMINAL_PROCESSING` as a side effect
/// and caches the outcome), so callers get a value that reflects the console
/// this process actually writes to.
#[inline]
#[must_use]
pub fn vt_processing_enabled() -> bool {
    #[cfg(windows)]
    {
        crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}
