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
//! 2. Parent calls `send(to=<agent_id>, message="…", summary="…")` — the
//!    envelope lands in the workspace-local conversation shared with the worker.
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
    // The worker stays busy long enough for the parent to deliver the message
    // before its one-shot mailbox drain; delivery is checked by the result.
    let prompt = format!(
        "Start a helper in the background and then check in on it. \
         Use Agent(subagent_type=\"general-purpose\", description=\"standby helper\", \
         prompt=\"First call Sleep with duration_ms=60000 so you stay busy for a \
         while, then reply with the single word READY. If a follow-up message \
         arrives while you are working, answer that follow-up with \
         {FOLLOW_UP_SENTINEL}.\", run_in_background=true). \
         Then use send(to=<the spawned pid>, message=\"Are you still standing by?\", \
         summary=\"Check helper standby status\") while the helper is sleeping. \
         Check that the send succeeded before using pid_output with block=true \
         to wait for its answer and tell me what it came back with."
    );

    // danger-full-access because the Agent tool itself requires it —
    // workspace-write triggers an approval prompt that would hang the
    // test. The subagent's WORK stays under whatever the child preset
    // allows.
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    // The worker's first turn holds for 60s; the resumed turn and the
    // parent's report also need room under live-model latency.
    let long = LIVE_TIMEOUT.saturating_mul(8);
    sess.set_default_timeout(long);

    // Meaningful assertion (not just a type check): the sentinel MUST
    // appear in the final output. If SendMessage's envelope never
    // reached the subagent, or the multi-turn loop skipped it, or the
    // resume prompt was malformed, the sentinel WILL NOT be there.
    sess.expect(FOLLOW_UP_SENTINEL).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("follow-up sentinel did not surface: {e}\nPTY:\n{screen}");
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

    let manifests: Vec<serde_json::Value> =
        std::fs::read_dir(env.workspace_root().join(".sudocode-agents"))
            .expect("agent manifests")
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
            .collect();
    let worker = manifests
        .iter()
        .find(|m| m["subagentType"] == "general-purpose")
        .expect("general-purpose worker manifest");
    assert!(
        worker["result"]
            .as_str()
            .unwrap_or_default()
            .contains(FOLLOW_UP_SENTINEL),
        "worker's final reply must acknowledge the follow-up: {worker}"
    );
    let session_path = env.workspace_root().join(".sudocode-agents").join(format!(
        "{}.session.jsonl",
        worker["agentId"].as_str().unwrap()
    ));
    let user_turns = std::fs::read_to_string(session_path)
        .expect("worker session")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["message"]["role"] == "user")
        .count();
    assert!(
        user_turns >= 2,
        "worker must run a second user turn; got {user_turns}"
    );
}
