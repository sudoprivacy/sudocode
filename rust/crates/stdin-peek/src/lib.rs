//! Ask whether stdin has data waiting, consuming nothing.
//!
//! # Why this crate exists
//!
//! `--print` reads piped stdin whenever stdin is not a terminal. When stdin is
//! a pipe a parent opened and then never writes to and never closes — what a
//! subprocess spawned without explicit stdin handling looks like, so any
//! harness, CI step or background job — a plain `read_to_string` never returns
//! and the CLI does nothing at all.
//!
//! The fix is to check readiness *before* starting a read, because a blocking
//! read on a pipe cannot be cancelled once parked. Unix has `poll`. Windows has
//! `PeekNamedPipe`, which is FFI and therefore `unsafe`, and the workspace root
//! sets `unsafe_code = "forbid"` — a lint that cannot be relaxed from inside a
//! crate that inherits it.
//!
//! So the exception is isolated here instead of weakened everywhere: this crate
//! deliberately does **not** opt into the workspace lints, and holds nothing but
//! the smallest safe wrapper around that one call. Reviewing the workspace's
//! entire `unsafe` surface means reviewing this file.
//!
//! Policy — how long to wait, and what to do when readiness cannot be
//! determined — stays in the caller, where the lint still applies.

use std::time::Duration;

/// What a readiness peek concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// A read will not block: either bytes are buffered, or the writer closed
    /// and the read will see EOF immediately.
    Ready,
    /// The deadline passed with the pipe still open and still empty. A read
    /// started now would park until the writer acts, which may be never.
    NotReady,
    /// Readiness could not be determined — stdin is not a pipe, or the peek
    /// failed for a reason that is not evidence either way. Callers should do
    /// what they did before this crate existed and simply read; an
    /// indeterminate answer must never be reported as `NotReady`, because that
    /// would silently drop piped input that was really there.
    Unknown,
}

/// Wait up to `timeout` for stdin to become readable, consuming nothing.
///
/// Only pipes can park a read the way described above, so only pipes are
/// peeked; a console or a file redirect (`< file`) reports [`Readiness::Unknown`]
/// and the caller reads as usual.
#[cfg(windows)]
#[must_use]
pub fn stdin_is_readable(timeout: Duration) -> Readiness {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::thread::sleep;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_PIPE};
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    /// `PeekNamedPipe` does not block, so readiness is polled. 25ms keeps the
    /// worst case under ~120 wake-ups for the 3s deadline the CLI uses, which
    /// is far below the cost of the API request that follows.
    const POLL_INTERVAL: Duration = Duration::from_millis(25);

    let stdin = io::stdin();
    let handle = stdin.as_raw_handle().cast::<core::ffi::c_void>() as HANDLE;

    // SAFETY: `handle` is borrowed from the live `io::stdin()` above and stays
    // valid for this call; `GetFileType` only inspects it.
    let file_type = unsafe { GetFileType(handle) };
    if file_type != FILE_TYPE_PIPE {
        return Readiness::Unknown;
    }

    let deadline = Instant::now() + timeout;
    loop {
        let mut available: u32 = 0;
        // SAFETY: `handle` is still the live stdin handle. Every out-pointer is
        // null except `lptotalbytesavail`, which points at a `u32` local we own;
        // a null read buffer with size 0 is the documented way to ask only how
        // many bytes are available, and it copies nothing out of the pipe.
        let ok = unsafe {
            PeekNamedPipe(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };

        if ok == 0 {
            let failed_because_writer_closed =
                io::Error::last_os_error().raw_os_error() == Some(ERROR_BROKEN_PIPE as i32);
            return if failed_because_writer_closed {
                // Every writer closed: a read returns EOF at once. That is
                // "will not block", and keeps `printf '' | scode -p …` fast.
                Readiness::Ready
            } else {
                Readiness::Unknown
            };
        }

        if available > 0 {
            return Readiness::Ready;
        }

        let now = Instant::now();
        if now >= deadline {
            return Readiness::NotReady;
        }
        sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }
}

/// Non-Windows builds never call this; Unix uses `poll` directly in the CLI,
/// where the workspace lints still apply.
#[cfg(not(windows))]
#[must_use]
pub fn stdin_is_readable(_timeout: Duration) -> Readiness {
    Readiness::Unknown
}
