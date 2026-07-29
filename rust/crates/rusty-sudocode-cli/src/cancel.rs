//! Single source of truth for the double Ctrl-C exit mechanism.
//!
//! Both the sync REPL (via `HookAbortMonitor` in `main.rs`) and the async REPL
//! (via `LineEditor` in `input.rs`) need to track "was the last Ctrl-C recent
//! enough that a second one means exit?". This module owns the shared constant,
//! the cross-turn timestamp, and the helpers that read/write it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// CC-parity timeout for double Ctrl-C force exit.
pub const DOUBLE_CTRLC_WINDOW: Duration = Duration::from_millis(800);

/// Shared timestamp (millis since process start) of the last Ctrl-C that
/// cancelled a turn. Persists across `HookAbortMonitor` lifetimes so a
/// fast second Ctrl-C on the *next* turn's monitor (or between turns)
/// triggers process exit. `0` means no pending exit.
static LAST_CTRLC_CANCEL_MS: AtomicU64 = AtomicU64::new(0);

/// Monotonic milliseconds since process start. Used as the time-base for
/// `LAST_CTRLC_CANCEL_MS` so the timestamp survives across monitor lifetimes
/// without wall-clock drift concerns.
///
/// Returns at least 1 — zero is reserved as the "no previous Ctrl-C"
/// sentinel in [`LAST_CTRLC_CANCEL_MS`].
pub fn process_uptime_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let elapsed = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    elapsed.max(1)
}

/// Record a Ctrl-C cancel. Call this every time a Ctrl-C fires (whether it
/// actually cancels a turn or just resets the prompt).
pub fn record_ctrlc() {
    LAST_CTRLC_CANCEL_MS.store(process_uptime_ms(), Ordering::Relaxed);
}

/// Returns `true` when the previous `record_ctrlc()` was within
/// [`DOUBLE_CTRLC_WINDOW`] of now — meaning the user double-pressed Ctrl-C
/// and wants to exit.
pub fn is_double_ctrlc() -> bool {
    let prev = LAST_CTRLC_CANCEL_MS.load(Ordering::Relaxed);
    if prev == 0 {
        return false;
    }
    let now = process_uptime_ms();
    now.saturating_sub(prev) <= DOUBLE_CTRLC_WINDOW.as_millis() as u64
}
