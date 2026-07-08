//! PTY live e2e — 6 preset sub-agents each independently complete a task.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.2 Commit 7. Each of
//! sudocode's 6 built-in sub-agent presets (`general-purpose`,
//! `Explore`, `Plan`, `Verification`, `scode-guide`,
//! `statusline-setup`) gets one test that drives the FULL Agent-tool
//! round-trip: parent LLM calls `Agent(subagent_type='X', ...)`, the
//! child preset runs its allowed-tool subset to completion, and the
//! parent surfaces a task-notification or agent_id sentinel.
//!
//! ## Local-only per current convention
//!
//! These tests require `SCODE_TEST_BACKEND=live` because the mock
//! harness has a known scenario-inheritance gap (see plan §6.4):
//! subagents make their own `/v1/messages` requests without carrying
//! the parent's `PARITY_SCENARIO:` token, so the mock rejects them
//! and the parent CLI hangs. Under `SCODE_TEST_BACKEND=mock` (CI's
//! default) each test early-skips with a stderr note instead of
//! hanging.
//!
//! Local run against sudorouter:
//!
//! ```powershell
//! $env:PATH = "C:\Program Files\Git\bin;C:\Program Files\Git\usr\bin;" + $env:PATH
//! cmd /c 'call "D:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" > NUL 2>&1 && cd /d C:\Users\songym\cursor-projects\sudocode\rust && $env:SCODE_TEST_BACKEND="live"; cargo test -p rusty-sudocode-cli --test pty_presets_e2e -- --nocapture'
//! ```
//!
//! `~/.nexus/sudocode/sudocode.json` must exist and point at sudorouter
//! with a valid API key — same config the interactive CLI uses.
//!
//! ## Design notes
//!
//! - **One prompt per test** — the parent LLM is asked ONCE to spawn a
//!   worker of the target preset and report back. No multi-turn
//!   choreography (which would compound flake).
//! - **Loose assertions** — real LLM output is inherently variable.
//!   Each test looks for a small set of sentinels that would be
//!   nearly-impossible to miss if the preset actually ran (e.g.
//!   `<task-notification` opener when coord mode is on, `agent-` id
//!   prefix, a preset-specific marker word). The tests are structural
//!   guards, not response-string oracles.
//! - **Timeouts sized for real LLM latency** — each test allows up to
//!   `LIVE_TIMEOUT * 3` for the full parent→child→completion chain.
//!   Sudorouter's response times can spike on cold cache.

mod common;

use std::time::Duration;

use common::{TestEnv, LIVE_TIMEOUT};

/// Small helper: skip the test with a stderr note when the harness is
/// in mock mode. Returns `true` when the test should proceed (live
/// mode), `false` when it should return early (mock mode).
///
/// Kept as a plain function rather than a macro so a future
/// contributor can add e.g. `#[cfg_attr(feature = "ci", ignore)]`
/// without touching every call site.
fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — this test needs live \
         because subagent-spawning tests hit the mock scenario-inheritance \
         gap (plan §6.4). Rerun with SCODE_TEST_BACKEND=live."
    );
    false
}

/// The full parent→child→completion chain can take longer than a
/// single LLM call — allocate ample slack so a slow cold-cache turn
/// doesn't false-fail us.
fn preset_test_timeout() -> Duration {
    LIVE_TIMEOUT.saturating_mul(3)
}

/// Watch the PTY session for one of the "an agent completed" sentinels
/// that a coordinator/parent surfaces after Agent has dispatched.
/// Any single hit is enough — real LLM responses vary in exact
/// phrasing. Fails the test if none appear before the deadline.
fn expect_completion_sentinel(sess: &mut pty_expect::PtySession, preset: &str) {
    // Sentinels in order of specificity:
    //   1. Structural: task-notification XML opener (coord mode on)
    //      or the launched agent_id manifest opener (default).
    //   2. Natural language: "completed", "finished", "done".
    // The regex OR pattern matches any of them, without regex-special
    // characters that pty-expect's matcher would trip on.
    let pattern = r"task-notification|agent-|completed|finished";
    sess.expect(pattern).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "preset {preset} run did not surface a completion sentinel: {e}\n\
             tail of PTY screen (last 600 chars):\n{tail}",
            tail = screen
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });
}

/// Drive a single preset test: spawn scode with a prompt that asks the
/// parent LLM to `Agent(subagent_type=<preset>, ...)`, then wait for a
/// completion sentinel.
fn run_preset(preset: &str, description: &str, worker_task: &str) {
    let env = TestEnv::new(&format!("presets-e2e-{preset}"));
    if !require_live(&env, &format!("preset_{preset}_e2e")) {
        return;
    }

    let prompt = format!(
        "Use Agent(subagent_type=\"{preset}\", description=\"{description}\", prompt=\"{worker_task}\") \
         to run a small task, then report back briefly."
    );

    // danger-full-access because the Agent tool requires it to dispatch.
    // read-only / workspace-write would hang on the initial permission
    // prompt. The CHILD sub-agent's tool pool is restricted by its
    // preset regardless of the PARENT's permission mode.
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    sess.set_default_timeout(preset_test_timeout());

    expect_completion_sentinel(&mut sess, preset);

    // Drain to natural EOF — a real one-shot run exits after the
    // final assistant turn. Bounded so a hung child can't wedge the
    // test harness.
    sess.set_default_timeout(preset_test_timeout());
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "preset {preset}: scode did not exit cleanly after the run: {e}\n\
             tail: {tail}",
            tail = screen
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });
    assert_eq!(
        exit, 0,
        "preset {preset}: scode should exit 0 after the Agent chain completes; got {exit}"
    );
}

// ── the 6 presets ──────────────────────────────────────────────────

#[test]
fn preset_general_purpose_e2e() {
    run_preset(
        "general-purpose",
        "quick arithmetic",
        "What is 6 * 7? Reply with the numeric answer only.",
    );
}

#[test]
fn preset_explore_e2e() {
    // Explore is read-only — pick a task that only requires file reads
    // (glob/grep) so the preset's restricted tool pool is sufficient.
    run_preset(
        "Explore",
        "count Rust files",
        "How many `.rs` files are at the top level of the current directory? \
         Reply with just the number.",
    );
}

#[test]
fn preset_plan_e2e() {
    run_preset(
        "Plan",
        "one-step plan",
        "Draft a two-item TodoWrite plan for adding a README section \
         titled 'Quickstart'. Report the todo list only.",
    );
}

#[test]
fn preset_verification_e2e() {
    // Verification typically uses bash to check things.
    run_preset(
        "Verification",
        "echo sanity check",
        "Run `echo verified` via bash and report whether it printed \
         `verified`. Reply with yes or no.",
    );
}

#[test]
fn preset_scode_guide_e2e() {
    run_preset(
        "scode-guide",
        "config path question",
        "In one sentence: where does scode read its main config file from?",
    );
}

#[test]
fn preset_statusline_setup_e2e() {
    run_preset(
        "statusline-setup",
        "statusline hint",
        "In one sentence: what setting controls whether scode's statusline \
         is visible?",
    );
}
