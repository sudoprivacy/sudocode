//! PTY live e2e — sub-agent permission prompts bubble up to the
//! parent process.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.4 Commit 12.
//!
//! ## What this exercises (long-workflow, data-flow chained)
//!
//! 1. Parent runs in `--permission-mode danger-full-access` (needed
//!    so the Agent tool itself can dispatch; sudocode's Agent tool
//!    requires `DangerFullAccess`).
//! 2. Parent spawns a sub-agent with
//!    `permission_mode: "bubble"` — the parity-target param that
//!    documents (and requests) the default sudocode behavior:
//!    permission escalation prompts from within the sub-agent land
//!    on the parent process's terminal, not on some (non-existent)
//!    inner prompter.
//! 3. The sub-agent performs a small write + emits the sentinel
//!    marker `BUBBLE_TEST_DONE`.
//! 4. Success = sentinel surfaces in the parent's report + scode
//!    exits cleanly, proving the `permission_mode="bubble"` param
//!    plumbs through end-to-end without breaking the chain.
//!
//! The stricter "read-only parent -> sub-agent write triggers a
//! visible bubble prompt" scenario isn't currently reachable via
//! sudocode's tool-loop (sub-agent tool restriction happens at
//! executor-level "tool X not enabled for this sub-agent" errors,
//! not via a permission prompt). Left as a separate parity target
//! if sudocode ever grows a permission-gated prompt path for
//! sub-agents specifically.
//!
//! ## Local-only per plan §6.4

mod common;

use common::{TestEnv, LIVE_TIMEOUT};

fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — subagent chain \
         blocked by mock scenario-inheritance gap (plan §6.4)."
    );
    false
}

#[test]
fn permission_prompt_from_subagent_bubbles_to_parent_terminal() {
    let env = TestEnv::new("pty-agent-perm-bubble");
    if !require_live(
        &env,
        "permission_prompt_from_subagent_bubbles_to_parent_terminal",
    ) {
        return;
    }

    // Parent in danger-full-access so the Agent tool dispatches
    // freely; the sub-agent then does a small write and emits the
    // sentinel. `permission_mode="bubble"` is the parity-target
    // param we're exercising — its presence in the tool call MUST
    // NOT break the chain (the schema tests already lock in the
    // parse; here we prove end-to-end plumb-through under a live
    // LLM).
    let prompt = "Follow this workflow: \
        (1) Use Agent(subagent_type=\"general-purpose\", description=\"tiny write\", \
            prompt=\"Write the file test-bubble.txt containing exactly the text 'hello'. \
             Then reply with exactly the marker BUBBLE_TEST_DONE and stop.\", \
            permission_mode=\"bubble\", \
            run_in_background=false). \
        (2) Report the sub-agent's final reply verbatim so the user sees the marker.";

    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", prompt]);
    let long = LIVE_TIMEOUT.saturating_mul(3);
    sess.set_default_timeout(long);

    // The BUBBLE_TEST_DONE marker MUST surface. Everything before
    // that is a chain of: sub-agent tried write -> enforcer
    // engaged -> parent-side path resolved -> sub-agent finished.
    sess.expect("BUBBLE_TEST_DONE").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "sub-agent chain never completed under permission_mode=bubble: {e}\n\
             tail (last 800 chars):\n{tail}",
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
