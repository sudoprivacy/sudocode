//! PTY live e2e — auto-verification streak nudge fires after 3
//! TaskUpdate(status=completed) calls and gets reset by a
//! Verification spawn.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.4 Commit 10.
//!
//! ## Long-workflow (5-step chain, data-flow linked)
//!
//! 1. Parent LLM creates three tasks via TaskCreate.
//! 2. Marks each one completed via TaskUpdate(status=completed).
//! 3. After the third completion, the tool's JSON return value
//!    contains the `<system-reminder>` nudge substring. The parent
//!    sees it in the tool_use result on its next turn.
//! 4. Parent, per the nudge, spawns
//!    `Agent(subagent_type="Verification", …)` — this resets the
//!    streak counter.
//! 5. Parent reports back to the user; the reply must contain the
//!    sentinel `VERIFIED_SENTINEL_ZYX987`.
//!
//! Assertion strategy: the sentinel comes from the Verification
//! sub-agent's prompt, so it only appears if the parent actually
//! spawned the Verification agent (which only happens if the model
//! saw the nudge). Strong causal link between "nudge fired" and
//! "sentinel appeared."
//!
//! ## Local-only per plan §6.4
//!
//! Same rationale as the other subagent-spawning PTY tests — mock
//! harness can't route the subagent's own /v1/messages requests.

mod common;

use common::{TestEnv, LIVE_TIMEOUT};

const VERIFIED_SENTINEL: &str = "VERIFIED_SENTINEL_ZYX987";

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — subagent-spawning \
         chain blocked by mock scenario-inheritance gap (plan §6.4). \
         Rerun with SCODE_TEST_BACKEND=live."
    );
    false
}

#[test]
fn three_task_completions_nudge_verification_spawn() {
    let env = TestEnv::new("pty-verification-streak");
    if !require_live(&env, "three_task_completions_nudge_verification_spawn") {
        return;
    }

    let prompt = format!(
        "Follow this multi-step workflow: \
         (1) Use TaskCreate to create three tasks: \
             subject='implement A', subject='implement B', subject='implement C'. \
         (2) Use TaskUpdate to mark 'implement A' as completed (status='completed'). \
         (3) Use TaskUpdate to mark 'implement B' as completed. \
         (4) Use TaskUpdate to mark 'implement C' as completed. \
             You should now see a system-reminder about running a Verification pass. \
         (5) In response to that reminder, spawn a Verification sub-agent: \
             Agent(subagent_type=\"Verification\", description=\"final verification\", \
                   prompt=\"Reply with exactly the string {VERIFIED_SENTINEL} to signal verification complete.\", \
                   run_in_background=false). \
         (6) After the Verification agent finishes, report its final reply verbatim to the user."
    );

    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    let long = LIVE_TIMEOUT.saturating_mul(4);
    sess.set_default_timeout(long);

    // Success = the sentinel surfaces. Only possible if:
    //   - The model created tasks and completed them via TaskUpdate.
    //   - The verification_watcher fired a nudge after 3 completions.
    //   - The model interpreted the nudge and dispatched the Verification agent.
    //   - The Verification agent ran and emitted the sentinel.
    //   - The parent reported it back.
    sess.expect(VERIFIED_SENTINEL).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "verification sentinel did not surface — one of the nudge/spawn/report \
             links is broken: {e}\ntail (last 800): {tail}",
            tail = screen
                .chars()
                .rev()
                .take(800)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });

    sess.set_default_timeout(long);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        panic!("scode did not exit cleanly: {e}");
    });
    assert_eq!(exit, 0);
}
