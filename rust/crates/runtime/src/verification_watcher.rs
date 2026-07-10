//! Process-wide streak counter that nudges the assistant to spawn a
//! Verification sub-agent when it has closed several tasks/todos in a
//! row without proving the changes work.
//!
//! Ported from CC-fork's task-closure telemetry: after 3+ consecutive
//! task closures without a Verification spawn, the coordinator/model
//! is nudged with a `<system-reminder>` hint. Once nudged, the
//! counter resets so the next nudge only fires after another streak.
//!
//! ## Wiring points (in the `tools` crate)
//!
//! - **Increment**: [`record_completions`] called from
//!   `run_todo_write` for each newly-completed todo transition.
//!   `TaskUpdate` in sudocode is a message-append tool that does not
//!   close a task, so it does NOT increment (unlike CC's TaskUpdate
//!   which can transition status). If sudocode later adds a
//!   status-transition tool, wire it here too.
//! - **Reset**: [`reset_streak`] called from `prepare_agent_job`
//!   when the spawned sub-agent is `subagent_type = "Verification"`.
//!   Assumes the model DID follow the nudge and started a real
//!   verifier — resetting keeps the counter honest.
//! - **Read + consume**: [`should_nudge_and_consume`] called at the
//!   end of `run_todo_write` (and by any other completion emitter);
//!   returns `Some(&str)` exactly once per streak-hitting-threshold,
//!   then resets the counter atomically so the nudge doesn't repeat.
//!
//! ## Threshold
//!
//! Default `3` (mirrors CC's heuristic). Overridable via
//! `SUDOCODE_VERIFICATION_STREAK_THRESHOLD`. `0` disables (returns
//! `None` forever).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Environment variable used to override the streak threshold. `0`
/// disables the nudge.
pub const VERIFICATION_STREAK_ENV: &str = "SUDOCODE_VERIFICATION_STREAK_THRESHOLD";

/// Default streak size that triggers the nudge. Matches CC-fork's
/// heuristic.
pub const DEFAULT_VERIFICATION_STREAK_THRESHOLD: usize = 3;

/// The `<system-reminder>` payload injected into the tool result
/// when the streak threshold is crossed. Wrapped in the reminder
/// tag so the model treats it as a systemic hint rather than a user
/// message — mirrors the CC-fork convention.
pub const VERIFICATION_NUDGE_TEXT: &str = "<system-reminder>\n\
3 or more tasks have been closed without a Verification pass. Spawn `Agent(subagent_type=\"Verification\", …)` to prove the changes actually work — running the code, exercising edge cases, checking failures — before continuing. Rubber-stamping is worse than nothing.\n\
</system-reminder>";

/// Process-global counter. Starts at 0. Simple atomic — no locking
/// needed because the operations are add / set / compare-exchange.
fn counter() -> &'static AtomicUsize {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    &COUNT
}

/// Read the currently-configured threshold. Returns `None` when the
/// feature is disabled (env is set to `0`).
#[must_use]
pub fn streak_threshold() -> Option<usize> {
    match std::env::var(VERIFICATION_STREAK_ENV) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(0) => None,
            Ok(n) => Some(n),
            Err(_) => Some(DEFAULT_VERIFICATION_STREAK_THRESHOLD),
        },
        Err(_) => Some(DEFAULT_VERIFICATION_STREAK_THRESHOLD),
    }
}

/// Add `n` to the streak counter. Preserved for internal use; most
/// callers should use [`record_completion_by_id`] which also
/// dedupes so re-listing the same todo doesn't re-increment.
pub fn record_completions(n: usize) {
    counter().fetch_add(n, Ordering::SeqCst);
}

/// Process-global dedupe set: content strings whose completion has
/// already been counted for the CURRENT streak. Cleared on
/// [`reset_streak`]. This exists because sudocode's TodoWrite tool
/// clears its on-disk store when all todos are Completed — after
/// that, a re-listed "already done" batch would look brand new to a
/// naive delta computation. Persisting the counted set in-memory
/// makes the counter stable across those clears.
fn counted_ids() -> &'static Mutex<BTreeSet<String>> {
    static SEEN: std::sync::OnceLock<Mutex<BTreeSet<String>>> = std::sync::OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Record a completion for the given content identifier. Only
/// increments the counter if `id` hasn't already been counted since
/// the last reset. Returns `true` when the counter was actually
/// bumped, `false` when the id was a duplicate.
///
/// Content strings are the natural key — TodoItem doesn't have a
/// stable ID field and re-persistence uses the same content.
pub fn record_completion_by_id(id: &str) -> bool {
    let mut set = counted_ids().lock().unwrap_or_else(|e| e.into_inner());
    if set.contains(id) {
        return false;
    }
    set.insert(id.to_string());
    counter().fetch_add(1, Ordering::SeqCst);
    true
}

/// Reset the counter to `0`. Called when the model dispatches a
/// Verification sub-agent — the nudge worked, we start counting
/// again from zero.
///
/// The dedupe set is intentionally NOT cleared: after a
/// Verification pass, the model typically re-lists the SAME
/// already-completed todos (sudocode's TodoWrite clears the on-disk
/// store when all are done, so the re-list looks brand-new to the
/// old_todos delta). Clearing the dedupe set would treat those
/// re-lists as fresh completions and re-fire the nudge immediately,
/// which is worse than annoying — it teaches the model to ignore
/// the reminder.
pub fn reset_streak() {
    counter().store(0, Ordering::SeqCst);
}

/// Test-only: clear both the counter AND the dedupe set. Used to
/// isolate one integration test's state from the next.
#[doc(hidden)]
pub fn reset_all_for_test() {
    counter().store(0, Ordering::SeqCst);
    if let Ok(mut set) = counted_ids().lock() {
        set.clear();
    }
}

/// Return the current streak value. Diagnostics + tests only —
/// production code should use [`should_nudge_and_consume`] which
/// atomically checks + resets.
#[must_use]
pub fn current_streak() -> usize {
    counter().load(Ordering::SeqCst)
}

/// Check-and-consume: if the streak has reached the threshold,
/// return `Some(VERIFICATION_NUDGE_TEXT)` AND reset the counter to
/// `0` atomically. Returns `None` when disabled or below threshold.
///
/// Atomic compare-exchange guarantees the nudge fires exactly once
/// per streak even under concurrent tool calls — a second caller
/// racing us sees the reset value and gets `None`.
#[must_use]
pub fn should_nudge_and_consume() -> Option<&'static str> {
    let threshold = streak_threshold()?;
    let counter = counter();
    loop {
        let current = counter.load(Ordering::SeqCst);
        if current < threshold {
            return None;
        }
        // Try to swap `current -> 0`. If the CAS fails (another
        // thread beat us), loop and re-check — if the new value is
        // still ≥ threshold we still fire; otherwise `None`.
        if counter
            .compare_exchange(current, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(VERIFICATION_NUDGE_TEXT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialises tests — the counter + env are process-global.
    static LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset_env() {
        std::env::remove_var(VERIFICATION_STREAK_ENV);
        reset_all_for_test();
    }

    #[test]
    fn streak_defaults_to_3() {
        let _g = guard();
        reset_env();
        assert_eq!(streak_threshold(), Some(3));
    }

    #[test]
    fn env_override_reads_threshold() {
        let _g = guard();
        reset_env();
        std::env::set_var(VERIFICATION_STREAK_ENV, "5");
        assert_eq!(streak_threshold(), Some(5));
        reset_env();
    }

    #[test]
    fn env_zero_disables_feature() {
        let _g = guard();
        reset_env();
        std::env::set_var(VERIFICATION_STREAK_ENV, "0");
        assert!(streak_threshold().is_none());
        // even after many completions, still no nudge
        record_completions(10);
        assert!(should_nudge_and_consume().is_none());
        reset_env();
    }

    #[test]
    fn under_threshold_returns_none() {
        let _g = guard();
        reset_env();
        record_completions(2);
        assert!(should_nudge_and_consume().is_none());
        assert_eq!(current_streak(), 2, "counter must NOT reset on None");
        reset_env();
    }

    #[test]
    fn at_threshold_returns_nudge_and_resets_counter() {
        let _g = guard();
        reset_env();
        record_completions(3);
        let nudge = should_nudge_and_consume().expect("threshold hit");
        assert!(nudge.contains("Verification"));
        assert!(nudge.contains("<system-reminder>"));
        assert_eq!(current_streak(), 0, "counter must reset atomically");
        // Second call fires again only after another streak.
        assert!(should_nudge_and_consume().is_none());
        reset_env();
    }

    #[test]
    fn over_threshold_still_returns_nudge_once() {
        let _g = guard();
        reset_env();
        record_completions(10);
        assert!(should_nudge_and_consume().is_some());
        assert!(should_nudge_and_consume().is_none(), "one-shot per streak");
        reset_env();
    }

    #[test]
    fn reset_streak_zeros_the_counter() {
        let _g = guard();
        reset_env();
        record_completions(2);
        assert_eq!(current_streak(), 2);
        reset_streak();
        assert_eq!(current_streak(), 0);
        // Now getting to threshold requires 3 more, not 1
        record_completions(2);
        assert!(should_nudge_and_consume().is_none());
        reset_env();
    }

    #[test]
    fn nudge_text_names_verification_agent_and_wraps_in_system_reminder() {
        // The message the model sees MUST be actionable — name the
        // exact tool call, and tag it as a system-reminder so the
        // model distinguishes it from user input.
        assert!(VERIFICATION_NUDGE_TEXT.contains("Agent(subagent_type=\"Verification\""));
        assert!(VERIFICATION_NUDGE_TEXT.starts_with("<system-reminder>"));
        assert!(VERIFICATION_NUDGE_TEXT.ends_with("</system-reminder>"));
    }
}
