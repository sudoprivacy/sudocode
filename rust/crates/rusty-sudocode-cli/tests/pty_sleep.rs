//! PTY tests for the `Sleep` tool.
//!
//! Coverage target: roadmap §Feature-inventory row "Sleep" (must-have,
//! CC parity, group 6 Planning & Structured). Before this file: 0 PTY
//! tests → Gap. After: the two branches that catch real regressions.
//!
//! ## What Sleep actually does (source: tools/src/lib.rs)
//!
//! An LLM-callable tool `{"name": "Sleep", "input": {"duration_ms": N}}`.
//! Blocks the tool call for `N` ms without holding a shell process,
//! polling a 50ms abort tick so a user-driven interrupt can cancel
//! mid-sleep. Enforces `MAX_SLEEP_DURATION_MS = 300_000` (5 min) as a
//! hard cap — over-max returns `Err` before sleeping.
//!
//! Two branches that matter in production:
//!
//! 1. **Happy path** — agent invokes Sleep with a small duration.
//!    The tool actually waits, doesn't spawn a shell, doesn't error.
//!    Wall-clock assertion catches a "silently no-op" regression (a
//!    refactor that drops the wait loop or clamps to 0).
//!
//! 2. **Over-max rejection** — duration_ms > 300_000 must return
//!    `Err(...)` immediately. Regression pattern: a change that
//!    forgets the guard or moves it below the sleep loop would burn
//!    hours in production (or eat a whole CI minute).
//!
//! What's NOT covered here:
//! - Abort-during-sleep — requires driving stdin/signal from the PTY
//!   partway through a sleep, which is a bigger harness change. The
//!   abort branch is exercised by `execute_sleep` unit tests in the
//!   tools crate.
//!
//! ```bash
//! cargo test --test pty_sleep                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_sleep  # real API
//! ```

mod common;

use std::time::Instant;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. Happy path — Sleep actually waits, exits cleanly
// ──────────────────────────────────────────────────────────────────────

/// Agent invokes Sleep with `duration_ms = 600`. The tool must
/// actually wait (wall-clock delta ≥ 600ms) and the CLI must exit 0.
///
/// Wall-clock assertion is mock-only: live mode can't guarantee the
/// exact duration the model picks — it might pick anything from 100ms
/// upward. In mock the scenario pins the duration to 600ms so we can
/// bound the assertion tightly enough to catch a "silently no-op"
/// regression while giving generous slack for PTY / process overhead.
#[test]
fn sleep_actually_waits_and_exits_zero() {
    let env = TestEnv::new("sleep-happy");
    let is_mock = env.is_mock();

    let prompt = env.prompt(
        "Please call the Sleep tool with duration_ms = 600. Do not describe it; just call the tool.",
        "sleep_short_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "Sleep",
        &prompt,
    ]);

    let started = Instant::now();

    sess.expect("Sleep")
        .expect("model must invoke Sleep (agent trigger)");

    let exit = sess.expect_eof().expect("scode should exit");
    let elapsed = started.elapsed();

    assert_eq!(exit, 0, "sleep turn should exit 0; got {exit}");

    if is_mock {
        // Mock's scenario returns duration_ms=600. Real wait must
        // actually happen. Allow 500ms as a lower bound (ConPTY /
        // ANSI reader can eat some milliseconds before the mock
        // reply lands) — a silent-no-op regression would complete
        // in <50ms.
        assert!(
            elapsed.as_millis() >= 500,
            "Sleep(600ms) must actually block; elapsed only {}ms — looks like a silent no-op regression",
            elapsed.as_millis()
        );
        // Upper bound: 20s is very loose but catches a "sleep loops
        // forever" regression that would otherwise time-out somewhere
        // else and be misdiagnosed.
        assert!(
            elapsed.as_secs() < 20,
            "Sleep(600ms) must not take 20+ s wall-clock; got {}ms — likely wait-loop regression",
            elapsed.as_millis()
        );
    }
}

// ──────────────────────────────────────────────────────────────────────
// 2. Over-max rejection — the hard cap holds
// ──────────────────────────────────────────────────────────────────────

/// Agent invokes Sleep with `duration_ms = 400_000` (>
/// `MAX_SLEEP_DURATION_MS = 300_000`). The tool must reject with `Err`
/// BEFORE beginning to sleep — otherwise a rogue / broken model could
/// hang the CLI for 5+ minutes.
///
/// Structural invariant: the whole turn (including the follow-up mock
/// response after the error tool_result) must complete in seconds, not
/// minutes. If elapsed > 60s the guard has regressed.
///
/// Mock-only: live mode won't reliably pick an over-max value.
#[test]
fn sleep_rejects_over_max_duration_without_blocking() {
    let env = TestEnv::new("sleep-over-max");
    if !env.is_mock() {
        // Live mode: skip — we can't force the model to pick
        // duration_ms > 300_000. The invariant is still checked in
        // mock and in `execute_sleep` unit tests.
        eprintln!("skipping sleep_rejects_over_max_duration_without_blocking in live mode");
        return;
    }

    let prompt = env.prompt(
        "Please call the Sleep tool with duration_ms = 400000. Do not describe it; just call the tool.",
        "sleep_over_max_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "Sleep",
        &prompt,
    ]);

    let started = Instant::now();

    sess.expect("Sleep")
        .expect("model must invoke Sleep (agent trigger)");

    let exit = sess.expect_eof().expect("scode should exit");
    let elapsed = started.elapsed();

    assert_eq!(
        exit, 0,
        "over-max turn should exit 0 (tool error, not CLI crash); got {exit}"
    );

    // The guard is the invariant. A 400s wall-clock would mean the
    // guard failed and Sleep began waiting. Give 60s of budget for
    // PTY overhead + mock reply + follow-up turn.
    assert!(
        elapsed.as_secs() < 60,
        "Sleep(400_000ms) with MAX = 300_000ms must reject early; wall-clock was {}s — guard regressed?",
        elapsed.as_secs()
    );
}
