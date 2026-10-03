//! Local, opt-in startup timings. No files or network events are emitted.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Option<Instant>> = OnceLock::new();

pub(crate) fn initialize() {
    let _ = START.set(
        (std::env::var_os("SCODE_TRACE_STARTUP").as_deref() == Some(std::ffi::OsStr::new("1")))
            .then(Instant::now),
    );
}

/// Record a named phase on stderr; cumulative time starts at entry to main.
pub(crate) fn measure<R>(phase: &str, operation: impl FnOnce() -> R) -> R {
    let Some(Some(start)) = START.get() else {
        return operation();
    };
    let phase_start = Instant::now();
    let result = operation();
    eprintln!(
        "startup phase={phase} elapsed_us={} cumulative_us={}",
        phase_start.elapsed().as_micros(),
        start.elapsed().as_micros(),
    );
    result
}
