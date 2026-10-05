//! One activity clock for the inline and one-shot spinners.

use std::{
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::{Duration, Instant},
};

const STALL_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct SpinnerProgress {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    response_bytes: AtomicU32,
}

impl SpinnerProgress {
    pub(crate) fn new() -> Self {
        Self {
            epoch: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            response_bytes: AtomicU32::new(0),
        }
    }

    pub(crate) fn reset(&self) {
        self.response_bytes.store(0, Ordering::Relaxed);
        self.record_activity();
    }

    fn now_ms(&self) -> u64 {
        self.epoch
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    /// Actual content, tool progress or a phase transition starts a fresh wait.
    /// Atomics keep per-delta updates independent of the UI and input locks.
    pub(crate) fn record_activity(&self) {
        self.last_activity_ms
            .fetch_max(self.now_ms(), Ordering::Relaxed);
    }

    pub(crate) fn add_response_bytes(&self, count: u32) {
        if count != 0 {
            self.record_activity();
            self.response_bytes.fetch_add(count, Ordering::Relaxed);
        }
    }

    pub(crate) fn response_bytes(&self) -> u32 {
        self.response_bytes.load(Ordering::Relaxed)
    }

    /// Preserve the existing exemptions for first-token wait and reasoning.
    /// Turn duration and animation frames are never evidence of a stall.
    pub(crate) fn is_stalled(&self, is_reasoning: bool) -> bool {
        !is_reasoning
            && self.response_bytes() > 0
            && Duration::from_millis(
                self.now_ms()
                    .saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)),
            ) >= STALL_TIMEOUT
    }
}
