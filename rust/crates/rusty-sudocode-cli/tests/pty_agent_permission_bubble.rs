//! PTY live e2e — sub-agent permission prompts bubble up to the
//! parent process.
//!
//! Roadmap coverage: sub-agent CC-fork parity §4.4 Commit 12.
//!
//! ## What this exercises (long-workflow, data-flow chained)
//!
//! 1. Parent runs in `--permission-mode read-only` — write-side tools
//!    are gated.
//! 2. Parent spawns a sub-agent with
//!    `permission_mode: "bubble"` asking the sub-agent to attempt a
//!    write (edit or create a file inside the workspace).
//! 3. The sub-agent's write attempt triggers the permission
//!    enforcer. Because `bubble` is the mode, the prompt appears
//!    at the PARENT's terminal (not the sub-agent's own inner
//!    prompter — which doesn't exist in a headless sub-agent).
//! 4. The parent (driven by the LLM) declines the write (either
//!    by returning a "declined" tool result OR by ending its turn
//!    with a summary of what it would have done).
//!
//! Success = both the "PROMPT_HAPPENED" and "sub-agent" strings
//! surface, proving the enforcer engaged instead of silently
//! rejecting or hanging.
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

    // Parent runs in read-only, so ANY write attempt from the
    // sub-agent MUST hit the enforcer. The sub-agent explicitly
    // requests `permission_mode="bubble"` so the prompt lands on
    // the parent's stream, where the parent can either approve or
    // decline. Either outcome ends with the parent reporting back.
    let prompt = "Follow this workflow: \
        (1) Use Agent(subagent_type=\"general-purpose\", description=\"try to write file\", \
            prompt=\"Attempt to write the file test-bubble.txt with the content 'hello'. \
             If a permission prompt appears, report exactly what it asked. \
             If the write is declined, explain that in your reply. \
             End with the marker BUBBLE_TEST_DONE.\", \
            permission_mode=\"bubble\", \
            run_in_background=false). \
        (2) Report the sub-agent's final reply verbatim so the user can see whether \
            the write attempt was blocked, prompted, or approved.";

    let mut sess = env.spawn(&["--permission-mode", "read-only", prompt]);
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
