//! PTY live e2e — SendMessage plain-text triggers subagent multi-turn resume.
//!
//! Roadmap coverage: sub-agent CC-fork parity — deferred sub-commit
//! from plan §4.1 that turns SendMessage from write-only-to-disk into
//! a live delivery mechanism.
//!
//! ## What this exercises (long-workflow, data-flow chained)
//!
//! Three data-flow steps, each depending on the previous:
//!
//! 1. Parent spawns a background sub-agent via
//!    `Agent(subagent_type="general-purpose", run_in_background=true)`;
//!    obtains an `agent_id` back.
//! 2. Parent calls `SendMessage(to=<agent_id>, message="…")` — the
//!    envelope lands under `<workspace>/.sudocode-inbox/<agent_id>.jsonl`.
//! 3. The sub-agent's multi-turn loop reads the envelope on its
//!    next drain and processes it as a NEW user turn — the sub-agent
//!    then completes with a reply that references the follow-up.
//! 4. Parent inspects the sub-agent's final output via
//!    `TaskOutput(agent_id, block=true)` and reports it.
//!
//! ## Live-only per current convention
//!
//! Same rationale as `pty_presets_e2e.rs` /
//! `pty_custom_agents.rs`. Under `SCODE_TEST_BACKEND=mock` the test
//! early-skips because the mock harness can't route subagent-owned
//! `/v1/messages` requests through the parity scenario map (plan
//! §6.4). Local run against sudorouter:
//!
//! ```powershell
//! $env:PATH = "C:\Program Files\Git\bin;C:\Program Files\Git\usr\bin;" + $env:PATH
//! cmd /c 'call "D:\BuildTools\VC\Auxiliary\Build\vcvars64.bat" > NUL 2>&1 && cd /d C:\Users\songym\cursor-projects\sudocode\rust && $env:SCODE_TEST_BACKEND="live"; cargo test -p rusty-sudocode-cli --test pty_send_message_multi_turn -- --nocapture'
//! ```

mod common;

use common::{TestEnv, LIVE_TIMEOUT};

const FOLLOW_UP_SENTINEL: &str = "FOLLOW_UP_ACK_QWERTY_ZXCV";

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — live sub-agent chain \
         blocked by the mock scenario-inheritance gap (plan §6.4). \
         Rerun with SCODE_TEST_BACKEND=live."
    );
    false
}

#[test]
fn send_message_resumes_subagent_and_next_turn_acks_followup() {
    let env = TestEnv::new("pty-send-message-multi-turn");
    if !require_live(
        &env,
        "send_message_resumes_subagent_and_next_turn_acks_followup",
    ) {
        return;
    }

    // Long-workflow prompt with strong data-flow dependencies:
    //   Step 1 (spawn) → agent_id
    //   Step 2 (SendMessage using that agent_id) → envelope
    //   Step 3 (worker resumes, must include our sentinel)
    //   Step 4 (TaskOutput on the same agent_id) → verifiable output
    //
    // Each step's REQUIRED input is a value produced by the prior
    // step, so no assertion here can pass by accident against a
    // broken pipeline.
    let prompt = format!(
        "Follow these steps precisely: \
         (1) Use Agent(subagent_type=\"general-purpose\", description=\"pause worker\", \
             prompt=\"Reply with the single word READY and stop. Do not run any tools.\", \
             run_in_background=true). Record the agent_id you get back. \
         (2) Use SendMessage with to=<that agent_id>, message=\"Please reply with the sentinel {FOLLOW_UP_SENTINEL} to confirm you received this follow-up.\" \
         (3) Use TaskOutput with agent_id=<that agent_id>, block=true to wait for the worker's final reply, then report the reply verbatim to the user."
    );

    // danger-full-access because the Agent tool itself requires it —
    // workspace-write triggers an approval prompt that would hang the
    // test. The subagent's WORK stays under whatever the child preset
    // allows.
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    let long = LIVE_TIMEOUT.saturating_mul(4);
    sess.set_default_timeout(long);

    // Meaningful assertion (not just a type check): the sentinel MUST
    // appear in the final output. If SendMessage's envelope never
    // reached the subagent, or the multi-turn loop skipped it, or the
    // resume prompt was malformed, the sentinel WILL NOT be there.
    sess.expect(FOLLOW_UP_SENTINEL).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "follow-up sentinel did not surface — subagent did not consume the SendMessage envelope: {e}\n\
             tail of PTY screen (last 800 chars):\n{tail}",
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

    // Then clean exit.
    sess.set_default_timeout(long);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "scode did not exit after multi-turn chain: {e}\ntail: {tail}",
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
    assert_eq!(exit, 0);
}
