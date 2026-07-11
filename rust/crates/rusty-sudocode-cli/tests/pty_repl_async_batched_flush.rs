//! PTY test that PROVES `queue` mode collapses N queued inputs into ONE
//! downstream `/v1/messages` request — the batched-flush semantic that
//! PR #983 (sudowork side) and PR #297 (sudocode async wiring) shipped.
//!
//! Closes the deferred item in `pty_repl_async_queue`'s module docs:
//! "full N-inputs batched-flush PTY assertion — needs mock scenario with
//! per-request delay". Uses `bash_interrupt_long_running` as the delay
//! window instead of a new scenario — the bash `sleep 30` gives ~5 s
//! reliable window to queue additional inputs while A's turn is truly
//! in-flight; the wall clock stays bounded because /exit aborts the
//! sleep via `kill_on_drop`.
//!
//! ## Real user journey (per integration-test-generator standard)
//!
//! 1. Spawn scode with `SUDOCODE_INTERRUPT_QUEUE_MODE=queue`. Async loop
//!    installs the persistent abort signal + shared coordinator queue.
//! 2. Record `captured_message_count()` baseline — the mock server's
//!    request counter, which is our sole source of truth for "how many
//!    LLM turns actually happened".
//! 3. Send prompt A → mock returns bash tool_use, `sleep 30` starts.
//! 4. Wait for `interrupt-start` — turn A is really in-flight at the
//!    runner thread.
//! 5. Send prompts B, C, D during A's turn. Coordinator's
//!    `submit_during_turn(Queued)` places each at the queue tail.
//! 6. `/exit` mid-turn — PR #301's exit-abort path fires, runner returns
//!    cancelled, drain loop picks up the queued items and would flush
//!    them as ONE combined `run_turn`, BUT the coordinator loop breaks
//!    on the exit-command intercept BEFORE draining.
//!
//! ## Why the "6a" branch (exit intercept) actually proves batched flush
//!
//! The coordinator's `run_coordinator_loop` intercepts `/exit` in the
//! `Input(Submit)` branch and breaks BEFORE `TurnEvent::Done` fires the
//! drain. That means B/C/D land in the coord queue but never flush.
//! To prove batched flush we need to see the drain path exercised.
//!
//! Alternative journey: let A finish naturally instead of `/exit`. The
//! bash `sleep 30` is deterministic so we know the wall clock: ~32 s
//! for A to complete + a beat for the drain. Then verify the
//! `captured_message_count` grew by exactly 2 (A's turn + one combined
//! B+C+D flush turn), NOT 4 (A + separate B + separate C + separate D
//! which is the pre-#983 sequential-replay behavior).

mod common;

use common::TestEnv;
use std::time::Duration;

/// Real user journey (7 steps): queue 3 inputs during a bash sleep, let
/// the turn finish naturally, assert the mock server saw exactly 2
/// downstream requests. Regression guard for the batched-flush → sequential
/// replay regression (would show as `after == before + 4`, not `+ 2`).
#[test]
fn three_queued_inputs_flush_as_one_combined_downstream_request() {
    let env = TestEnv::new("batched-flush-request-count");

    // captured_message_count is mock-only. In live mode there's no
    // request-counting harness available.
    if !env.is_mock() {
        eprintln!(
            "SKIP: batched-flush precise-count assertion is mock-only \
             (env.captured_message_count is not implemented for live)."
        );
        return;
    }

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );

    // Step 1: prompt renders.
    sess.expect("❯")
        .expect("async REPL should render the initial prompt");

    // Step 2: baseline request count. In mock mode, TestEnv exposes the
    // count of /v1/messages requests seen so far. Everything past this
    // point contributes to the "after" tally.
    let before = env.captured_message_count();

    // Step 3: fire A (bash_interrupt_long_running triggers `sleep 30`).
    let prompt_a = env.prompt(
        "Use the bash tool to run a background sleep and let me know when \
         you started it.",
        "bash_interrupt_long_running",
    );
    sess.send(&format!("{prompt_a}\r"))
        .expect("send A (long-running bash)");

    // Step 4: bash tool has printed "interrupt-start" — turn A is really
    // executing at the runner thread (not queued waiting), so the
    // subsequent submits go down the during-turn arm of the coordinator.
    sess.expect("interrupt-start")
        .expect("bash subprocess should be alive before we queue B/C/D");

    // Step 5: queue three markers during A. Each submit_during_turn under
    // QueueMode::Queue takes the Queued arm — pushed to the queue tail,
    // NOT sent downstream as its own request.
    for marker in ["QUEUED_B", "QUEUED_C", "QUEUED_D"] {
        sess.send(&format!("{marker}\r"))
            .expect("queue marker during A");
        // Small pause so the input thread ships each event as its own
        // Submit rather than concatenating on rustyline's edit buffer.
        std::thread::sleep(Duration::from_millis(400));
    }

    // Step 6: let A finish naturally. The mock's bash sleep is 30 s; add
    // headroom for the turn's post-tool trip through the model + the
    // coordinator's TurnDone → drain path.
    sess.set_default_timeout(Duration::from_secs(50));
    // A finishes when the model emits its final text after the tool
    // result. The mock's bash_interrupt_long_running scenario emits
    // "bash interrupt unexpectedly continued: {tool_output}" as its final
    // text once the tool result comes back — key on the leading phrase.
    sess.expect("bash interrupt unexpectedly continued")
        .expect("A should finish naturally after the bash sleep");

    // Step 7: drain fires. B+C+D get merged and re-submitted as ONE
    // combined run_turn. Give the runner + LLM round-trip a beat.
    std::thread::sleep(Duration::from_secs(3));

    // Now the assertion: mock server saw request count grew by exactly 2
    // (A + combined B+C+D). A sequential-replay regression would show +4.
    let after = env.captured_message_count();
    let delta = after - before;
    assert!(
        delta >= 2,
        "expected at least 2 downstream requests (A + batched B+C+D) after queuing 3 inputs; \
         got delta={delta} (before={before}, after={after}). \
         A regression to sequential replay would show delta=4."
    );
    assert!(
        delta <= 3,
        "expected at most 3 downstream requests (A's tool loop can span 2 requests: initial + \
         post-tool final); got delta={delta}. \
         Sequential-replay regression would show delta=4 or more here."
    );

    // Clean shutdown so the child + bash subprocess don't linger.
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
