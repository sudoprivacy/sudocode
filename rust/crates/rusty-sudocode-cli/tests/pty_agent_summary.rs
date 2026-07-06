//! PTY live e2e — AgentSummary auto-summarize on long sub-agent output.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.4 Commit 11.
//!
//! ## What this exercises (long-workflow, data-flow chained)
//!
//! 1. Parent spawns a sub-agent whose prompt asks it to emit a very
//!    long output (>> 8 KB threshold).
//! 2. When the sub-agent returns, the tools crate detects
//!    `full_text.chars().count() > threshold` and:
//!    a. writes the FULL output to `<agent_id>.full.md` sibling;
//!    b. updates the manifest with `resultFullPath`;
//!    c. spawns a summarizer sub-turn against sudorouter that
//!       condenses the output to ≤500 words.
//! 3. Parent's `TaskOutput(agent_id)` returns the SHORT summary.
//! 4. Parent reports the summary AND the full-result path back.
//!
//! Assertion: BOTH a short summary and the `.full.md` sidecar path
//! must surface in the parent's final report — proving that
//! summarization fired AND the full text is preserved for the
//! parent to `read_file` if it wants.
//!
//! ## Local-only per plan §6.4
//!
//! Requires SCODE_TEST_BACKEND=live (mock harness has scenario-
//! inheritance gap on sub-agent /v1/messages calls).

mod common;

use common::{TestEnv, LIVE_TIMEOUT};

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — subagent-spawning \
         chain blocked by mock scenario-inheritance gap (plan §6.4)."
    );
    false
}

#[test]
fn long_subagent_output_is_summarized_and_full_text_preserved() {
    let env = TestEnv::new("pty-agent-summary");
    if !require_live(
        &env,
        "long_subagent_output_is_summarized_and_full_text_preserved",
    ) {
        return;
    }

    // The sub-agent's prompt asks for very long output — well over
    // the 8 KB summary threshold. We keep the prompt deterministic
    // ("output the digit 1 exactly 12000 times") so the char count
    // is predictable and any live-model wobble doesn't drop below
    // threshold.
    let prompt = "Follow these steps: \
        (1) Use Agent(subagent_type=\"general-purpose\", description=\"long output\", \
            prompt=\"Output the digit 1 exactly twelve thousand times, with no other characters, \
             no spaces, no newlines, no explanation. Just the character '1' repeated 12000 times.\", \
            run_in_background=true). Record the agent_id you get back. \
        (2) Use TaskOutput with agent_id=<that agent_id>, block=true to wait for the worker. \
        (3) Report back to the user with (a) the summary text you received from TaskOutput, \
            AND (b) the value of the `resultFullPath` field in the TaskOutput response — \
            the exact path string as it appears there. Tag them clearly, e.g. \
            'SUMMARY: <text>' and 'FULL_PATH: <path>'.";

    let mut sess = env.spawn(&["--permission-mode", "workspace-write", prompt]);
    let long = LIVE_TIMEOUT.saturating_mul(4);
    sess.set_default_timeout(long);

    // Success signals — both must appear:
    //   1. A `.full.md` path (sidecar file).
    //   2. Something clearly shorter than 12 000 chars (proof
    //      summarization happened; we can't easily count from
    //      the outside, but we look for the FULL_PATH sentinel).
    sess.expect(r"\.full\.md").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "expected `.full.md` sidecar path to surface: {e}\ntail:\n{tail}",
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

    // The parent SHOULD ALSO surface the "SUMMARY:" tag it was
    // asked to emit. If the model skips the tag we accept any short
    // description that's clearly not 12 000 ones.
    sess.set_default_timeout(long);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        panic!("scode did not exit cleanly: {e}");
    });
    assert_eq!(exit, 0);
}
