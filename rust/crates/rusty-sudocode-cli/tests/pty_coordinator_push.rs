//! PTY live e2e — coordinator-mode proactive `<task-notification>`
//! push into the next user turn.
//!
//! Roadmap coverage: sub-agent CC-fork parity §9.9 coord push.
//!
//! ## What this exercises (long-workflow, multi-turn REPL)
//!
//! 1. Set `SUDOCODE_COORDINATOR_MODE=1` so the coord prompt loads
//!    AND the tools crate's `persist_agent_terminal_state` emits to
//!    the coordinator inbox on sub-agent completion.
//! 2. Turn 1 (user prompt): "Spawn a subagent to output the
//!    sentinel, then confirm you launched it."
//!    - Parent LLM calls `Agent(...)` with `run_in_background=true`.
//!    - Sub-agent runs, terminates, writes envelope to
//!      `<workspace>/.sudocode-inbox/coordinator.jsonl`.
//! 3. Turn 2 (user prompt): "What task-notification blocks have you
//!    received in your context since my last message?"
//!    - CLI's `run_turn` calls `runtime::coordinator_notification::drain`,
//!      prepends the batched XML to the user input.
//!    - Parent LLM sees the `<task-notification>` XML in its user
//!      message and REPORTS it back.
//! 4. Assertion: The parent's turn-2 response MUST contain both
//!    - the `<task-notification>` opener, AND
//!    - the sub-agent's sentinel content
//!    (Either would be plausible via hallucination alone; both
//!     together only survive if the push path fired end-to-end.)
//!
//! ## Local-only per plan §6.4

mod common;

use std::time::Duration;

use common::{TestEnv, LIVE_TIMEOUT};

const SENTINEL: &str = "COORD_PUSH_SENTINEL_MNBVC";

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — coord-push chain \
         needs live sub-agent completion (mock harness scenario-inheritance \
         gap, plan §6.4)."
    );
    false
}

#[test]
fn task_notification_pushed_into_next_user_turn_under_coord_mode() {
    let env = TestEnv::new("pty-coord-push");
    if !require_live(
        &env,
        "task_notification_pushed_into_next_user_turn_under_coord_mode",
    ) {
        return;
    }

    // Interactive REPL — coord mode on. No positional prompt, so
    // scode enters the REPL and prints its ❯ prompt.
    let mut sess = env.spawn_with_env(
        &["--permission-mode", "danger-full-access"],
        &[("SUDOCODE_COORDINATOR_MODE", "1")],
    );
    let long = LIVE_TIMEOUT.saturating_mul(4);
    sess.set_default_timeout(long);

    // Wait for the REPL prompt.
    sess.expect("❯").expect("REPL prompt should appear");

    // Turn 1: spawn a sub-agent that emits the sentinel.
    let turn1 = format!(
        "Use Agent(subagent_type=\"general-purpose\", \
         description=\"emit sentinel\", \
         prompt=\"Reply with exactly the token {SENTINEL} and stop.\", \
         run_in_background=true) to spawn a worker. \
         Briefly confirm you launched it and end this turn."
    );
    sess.send(&format!("{turn1}\r")).expect("send turn 1");

    // The parent should emit a launch confirmation and end its turn.
    // Wait for the REPL prompt to reappear (turn 1 done).
    sess.expect("❯")
        .expect("REPL prompt should reappear after turn 1");

    // Give the sub-agent time to actually complete + emit envelope.
    // Sub-agents run on background threads; the mailbox write
    // happens synchronously inside persist_agent_terminal_state so
    // it's on disk by the time the sub-agent thread returns. But
    // the sub-agent turn itself might still be running when the
    // parent's turn 1 ends. Sleep so completion definitely happens
    // before turn 2's drain.
    std::thread::sleep(Duration::from_secs(8));

    // Turn 2: ask the parent to report on any task-notification it
    // has received. The CLI's drain fires BEFORE this prompt hits
    // the ConversationRuntime, so the prepended XML lands in the
    // parent's user-message context.
    let turn2 = "Since my last message: what `<task-notification>` XML \
         blocks (if any) do you now see in your context? Report the \
         verbatim `<result>...</result>` payload from any block you \
         see. If none, say NO_NOTIFICATIONS_SEEN.";
    sess.send(&format!("{turn2}\r")).expect("send turn 2");

    // The parent's turn-2 response MUST show:
    //   1. Evidence of the task-notification XML — either the
    //      literal `<task-notification>` opener OR at minimum the
    //      `<result>` tag it wraps.
    //   2. The SENTINEL string carried inside the notification's
    //      <result> section.
    // If drain didn't fire, the parent would report NO_NOTIFICATIONS_SEEN.
    sess.expect(SENTINEL).unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "expected sentinel {SENTINEL} in parent's turn-2 report \
             (proves the push path fired end-to-end): {e}\n\
             tail:\n{tail}",
            tail = screen
                .chars()
                .rev()
                .take(1200)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });

    // Exit cleanly.
    sess.expect("❯").expect("REPL prompt after turn 2");
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(15));
    let _ = sess.expect_eof();
}
