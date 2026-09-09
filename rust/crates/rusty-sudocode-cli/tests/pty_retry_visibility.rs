//! PTY test: a provider retry is visible while it is happening.
//!
//! When the provider answers 429 or 5xx the transport backs off and tries
//! again. From the outside that is indistinguishable from a slow model —
//! several seconds of nothing, possibly repeatedly — so the retry has to say
//! so. `⟳ retry 1/8 — 429: slow down` is the whole feature.
//!
//! **Why this test exists.** This regressed once already and nothing caught
//! it. Before the engine/renderer split, the CLI owned its own `ApiClient` and
//! implemented the transport's `RetryNotifier` directly, driving the spinner.
//! The split deleted that client — correctly, the renderer must not name `api`
//! — but the replacement was never wired: `RetryNotifier` had no implementor
//! anywhere and `SpinnerRef::set_retry` had no callers. The feature had been
//! documented as "cannot be reliably triggered in automated tests — requires a
//! specific proxy error", so nothing was watching. The mock can produce that
//! specific proxy error, which is what makes this testable at all.
//!
//! ```bash
//! cargo test --test pty_retry_visibility
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;

/// The transport retries a 429 and says so on the terminal, then the turn
/// completes normally.
///
/// The mock answers the first request 429 and the second normally, so this
/// exercises the real retry loop in `HttpTransport` — backoff included — not a
/// synthesised event.
#[test]
fn provider_retry_is_reported_then_the_turn_completes() {
    let env = TestEnv::new("retry-visibility");
    if !env.is_mock() {
        eprintln!(
            "skipping provider_retry_is_reported_then_the_turn_completes: \
             needs the mock's scripted 429 (a live provider will not \
             rate-limit on cue)"
        );
        return;
    }

    let prompt = env.prompt("Say hello", "retry_then_succeed");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    // The first backoff is a second plus jitter, and the turn still has to
    // finish after it.
    sess.set_default_timeout(Duration::from_secs(60));

    // Named as the user sees it: which attempt, out of how many, and why.
    // Matched as one line so a report that lost the reason — the only part
    // that says whether to wait or to go fix something — fails here.
    sess.expect(r"retry 1/8 .* 429").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("the retry should be reported: {e}\nPTY screen:\n{screen}");
    });

    // And the retry actually retried: the turn produces its answer.
    sess.expect("(?i)hello").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("the turn should complete after the retry: {e}\nPTY screen:\n{screen}");
    });

    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0, "a retried turn should still exit 0; got {exit}");
}

/// A turn that never hits a retry says nothing about retries.
///
/// Without this the test above would pass against a build that printed the
/// line unconditionally, which would be its own bug — every turn in every
/// session gaining a line about a retry that did not happen.
#[test]
fn a_turn_without_retries_reports_none() {
    let env = TestEnv::new("retry-visibility-absent");
    if !env.is_mock() {
        eprintln!("skipping a_turn_without_retries_reports_none: mock-only");
        return;
    }

    let prompt = env.prompt("Say hello", "streaming_text");
    let mut sess = env.spawn(&["--permission-mode", "read-only", &prompt]);
    sess.set_default_timeout(Duration::from_secs(60));

    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY screen:\n{screen}");
    });
    assert_eq!(exit, 0);

    let screen = sess.render(|s| s.contents());
    assert!(
        !screen.contains("retry"),
        "nothing was retried, so nothing should mention a retry; PTY screen:\n{screen}"
    );
}
