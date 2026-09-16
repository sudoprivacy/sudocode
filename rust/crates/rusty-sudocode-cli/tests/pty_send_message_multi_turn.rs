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
//!    `pid_output(pid, block=true)` and reports it.
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

    // The data-flow dependencies are what make the assertion meaningful, and
    // they are unchanged: the spawn yields an agent_id, SendMessage needs that
    // id to address the envelope, and `pid_output` needs it again to read the
    // worker's answer. No assertion here can pass by accident against a broken
    // pipeline.
    //
    // What changed is the framing. An earlier version was a numbered script —
    // "follow these steps precisely … SendMessage … message=\"please reply with
    // the sentinel X\" … then report the reply verbatim" — and a live model
    // recognised that shape, said so on screen ("relay a specific sentinel
    // string to it, then report the output verbatim … I won't follow these
    // steps"), and refused, so the test failed on a refusal rather than on the
    // envelope handling it exists to check. `pty_agent_summary` (ce366d00),
    // `pty_verification_streak` and `pty_agent_memory_scoping` were rewritten
    // for the same reason.
    //
    // The token now belongs to the WORKER, given at spawn time as how it should
    // answer a follow-up. Nothing asks the parent to carry it, and the causal
    // link is if anything tighter: the token can only appear if the SendMessage
    // envelope actually reached the worker and it took another turn.
    //
    // The worker sleeps first, and that is load-bearing rather than padding.
    // `run_multi_turn_loop` drains the inbox only AFTER a turn returns, and
    // exits immediately when it finds nothing there (`tools/src/lib.rs`). A
    // worker told to answer and stop finishes its first turn in a second or
    // two — long before the parent has taken its own next turn to call
    // SendMessage — so the drain saw an empty mailbox, the worker exited, and
    // the envelope landed with nobody left to read it. The screen said exactly
    // that: "the follow-up message landed in its mailbox but was not picked up
    // (the agent had exited)". Keeping the first turn busy for 30s is what puts
    // the envelope in the mailbox before the drain looks, which is the
    // behaviour this test exists to check.
    //
    // `Sleep` is in the general-purpose pool and needs only read-only
    // permission, so this asks the worker for nothing it is not already allowed
    // to do.
    let prompt = format!(
        "Start a helper in the background and then check in on it. \
         Use Agent(subagent_type=\"general-purpose\", description=\"standby helper\", \
         prompt=\"First call Sleep with duration_ms=30000 so you stay busy for a \
         while, then reply with the single word READY. If a follow-up message \
         arrives while you are working, answer that follow-up with \
         {FOLLOW_UP_SENTINEL}.\", run_in_background=true). \
         Then use SendMessage to ask the helper whether it is still standing by, \
         and use pid_output with block=true to wait for its answer and tell me \
         what it came back with."
    );

    // danger-full-access because the Agent tool itself requires it —
    // workspace-write triggers an approval prompt that would hang the
    // test. The subagent's WORK stays under whatever the child preset
    // allows.
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    // `* 8`, matching the other parent→child→report chains. The worker's first
    // turn now holds for 30s by design, and the chain still has the parent's
    // SendMessage turn and the resumed worker turn to go after that, so the
    // previous `* 4` left no headroom: the failing run hit the 120s ceiling
    // with the work still in flight.
    let long = LIVE_TIMEOUT.saturating_mul(8);
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
