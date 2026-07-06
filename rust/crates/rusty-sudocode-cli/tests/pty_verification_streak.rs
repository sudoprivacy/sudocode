//! PTY live e2e — auto-verification streak nudge fires after 3
//! TodoWrite completions and gets reset by a Verification spawn.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.4 Commit 10.
//!
//! ## Long-workflow (5-step chain, data-flow linked)
//!
//! 1. Parent LLM writes a TodoWrite plan with 3 todos, marks the
//!    first Completed.
//! 2. Marks the second Completed (streak = 2).
//! 3. Marks the third Completed — the tool's JSON return value now
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
fn three_todowrite_completions_nudge_verification_spawn() {
    let env = TestEnv::new("pty-verification-streak");
    if !require_live(&env, "three_todowrite_completions_nudge_verification_spawn") {
        return;
    }

    let prompt = format!(
        "Follow this multi-step workflow: \
         (1) Use TodoWrite to create three todos: 'implement A', 'implement B', 'implement C'. \
             Start with the first marked as in_progress and the others as pending. \
         (2) Use TodoWrite again to mark 'implement A' as completed and 'implement B' as in_progress. \
         (3) Use TodoWrite again to mark both A and B as completed and 'implement C' as in_progress. \
         (4) Use TodoWrite again to mark all three (A, B, C) as completed. \
             You should now see a system-reminder about running a Verification pass. \
         (5) In response to that reminder, spawn a Verification sub-agent: \
             Agent(subagent_type=\"Verification\", description=\"final verification\", \
                   prompt=\"Reply with exactly the string {VERIFIED_SENTINEL} to signal verification complete.\", \
                   run_in_background=false). \
         (6) After the Verification agent finishes, report its final reply verbatim to the user."
    );

    let mut sess = env.spawn(&["--permission-mode", "workspace-write", &prompt]);
    let long = LIVE_TIMEOUT.saturating_mul(4);
    sess.set_default_timeout(long);

    // Success = the sentinel surfaces. Only possible if:
    //   - The model executed TodoWrite 4 times ending in all-completed.
    //   - The verification_watcher fired a nudge after step 4.
    //   - The model interpreted the nudge and dispatched the Verification agent.
    //   - The Verification agent ran and emitted the sentinel.
    //   - The parent reported it back.
    // Any broken link in this chain fails the test.
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
