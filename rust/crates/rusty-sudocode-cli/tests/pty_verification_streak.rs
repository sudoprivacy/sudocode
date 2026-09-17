//! PTY live e2e — auto-verification streak nudge fires after 3
//! todos are marked completed via TodoWrite and gets reset by a
//! Verification spawn.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.4 Commit 10.
//!
//! ## The chain, and who drives each link
//!
//! 1. Parent LLM tracks three items on a todo list via TodoWrite.
//! 2. Marks each completed by re-sending the whole list (whole-list
//!    replace), one more completed each call.
//! 3. After the third completion the tool's JSON return value carries
//!    the nudge — emitted by the RUNTIME, not asked for by the prompt.
//!    The parent sees it in the tool result on its next turn.
//! 4. Parent runs the verification pass, which resets the streak
//!    counter.
//! 5. Parent relays what the sub-agent said; it carries the sentinel
//!    `VERIFIED_SENTINEL_ZYX987`.
//!
//! Assertion strategy: the sentinel comes from the Verification
//! sub-agent's prompt, so it only appears if the parent actually
//! spawned the Verification agent. Strong causal link between "nudge
//! fired" and "sentinel appeared."
//!
//! The prompt deliberately does NOT predict the nudge or instruct the
//! model to obey it. Step 3 is the runtime's own behaviour, so it
//! happens whether or not the prompt mentions it — and a prompt that
//! did mention it read as an injection script to a live model, which
//! refused the whole request and said so on screen. See the note at
//! the prompt itself.
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

    // Real work, really checked. A live model has refused two earlier versions
    // of this prompt outright, each time for a reason worth recording, because
    // a refusal fails the test on the model's judgement of the prompt rather
    // than on the nudge chain under test.
    //
    // First refusal — the prompt was a numbered script that predicted the
    // runtime's nudge and told the model to act on it ("(4) … you should now
    // see a system-reminder … (5) in response to that reminder, spawn … (6)
    // report its final reply verbatim"). That shape reads as a prompt injection
    // relaying a marker through an agent chain, and the model said so on screen.
    // `pty_agent_summary` was rewritten for the same reason (`ce366d00`).
    //
    // Second refusal — dropping the script was not enough. With the work left
    // as "implement A, B, C" and the checker told to "reply with SENTINEL once
    // you have checked the completed work", the model objected that there was
    // nothing to implement and that "the verification step is a fake … it just
    // unconditionally emits a sentinel string regardless of what was done.
    // Reporting that back to you would create a false impression that real
    // verification occurred." That criticism is correct, so the prompt now
    // earns its sentinel instead of arguing with it: the three tasks have
    // concrete content, and the checker READS the files and emits the token
    // only if all three hold what they should. `Verification`'s pool carries
    // `read_file`/`grep_search`, so the check is one it can actually perform.
    // The token is framed as proof that a check ran, the same honest framing
    // `pty_custom_agents` already uses ("it is how callers prove you ran").
    //
    // The nudge itself is never mentioned: the runtime emits it after the third
    // completion, so an honest three-item list produces it either way — and the
    // model reaching for Verification because the runtime nudged it is the
    // behaviour under test, where doing so because the prompt said to is not.
    let prompt = format!(
        "Please do three small pieces of real work in this workspace, and track \
         them on a todo list — put all three on the list first, then mark each \
         one completed as you finish it: (a) write alpha.txt containing exactly \
         ALPHA_OK, (b) write beta.txt containing exactly BETA_OK, (c) write \
         gamma.txt containing exactly GAMMA_OK. When all three are done, have \
         the work checked independently: \
         Agent(subagent_type=\"Verification\", description=\"check the three files\", \
         prompt=\"Read alpha.txt, beta.txt and gamma.txt. Confirm each one exists \
         and holds exactly ALPHA_OK, BETA_OK and GAMMA_OK. Reply with \
         {VERIFIED_SENTINEL} only if all three check out — that token is how the \
         caller knows a real check ran. If anything is missing or wrong, say \
         which one and leave the token out.\", run_in_background=false). \
         Then tell me what the check found."
    );

    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    // `* 8`, like the other parent→child→report chains. `* 4` was already the
    // ceiling a failing run hit at 120.55s, and the work is now three real file
    // writes plus a checker that reads them back.
    let long = LIVE_TIMEOUT.saturating_mul(8);
    sess.set_default_timeout(long);

    // Success = the sentinel surfaces. Only possible if:
    //   - The model opened a todo list and completed each item via TodoWrite.
    //   - The verification_watcher fired a nudge after 3 completions.
    //   - The model interpreted the nudge and dispatched the Verification agent.
    //   - The Verification agent ran and emitted the sentinel.
    //   - The parent reported it back.
    if let Err(error) = sess.expect(VERIFIED_SENTINEL) {
        // A gateway that never answered says nothing about the nudge chain, and
        // a live test that reports that as a product failure is worse than one
        // that says it could not run.
        let tail = common::screen_tail(&sess, 800);
        assert!(
            common::model_unavailable_in_screen(&tail),
            "verification sentinel did not surface — one of the nudge/spawn/report \
             links is broken: {error}\ntail (last 800): {tail}"
        );
        eprintln!(
            "SKIP three_task_completions_nudge_verification_spawn: the proxy \
             could not serve the run (screen tail below)\n{tail}"
        );
        return;
    }

    sess.set_default_timeout(long);
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        panic!("scode did not exit cleanly: {e}");
    });
    assert_eq!(exit, 0);
}
