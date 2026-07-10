//! PTY tests for Coordinator Mode.
//!
//! Coverage target: roadmap §Feature-inventory row "Coordinator Mode
//! (multi-worker orchestration prompt)" — subagent-cc-fork-parity
//! commit D. Before this file: 0 PTY tests → the coordinator prompt
//! didn't exist. After: env-var toggle drives the prompt swap and the
//! ported role phrasing lands in `scode system-prompt`.
//!
//! ## What Coordinator Mode does (ported from sudoprivacy/claude-code)
//!
//! When `SUDOCODE_COORDINATOR_MODE=1` (mirrors CC-fork's
//! `CLAUDE_CODE_COORDINATOR_MODE`), the runtime prepends the ported
//! `coordinator_system_prompt()` to the system prompt's dynamic
//! sections. This gives the model a coordinator role directive
//! (Research → Synthesis → Implementation → Verification, delegate via
//! Agent, stop via TaskStop) that takes primacy over the default
//! identity.
//!
//! ## Two branches this PTY test covers
//!
//! 1. **Env off / default** — `scode system-prompt` MUST NOT
//!    contain the coordinator role phrase. Regression sentinel
//!    against "coordinator prompt leaks into the default path" (which
//!    would double the token cost of every non-coordinator turn).
//!
//! 2. **Env on** — `SUDOCODE_COORDINATOR_MODE=1 scode
//!    system-prompt` MUST contain the coordinator role phrase.
//!    Regression sentinel against a rename of the env var, a broken
//!    `is_coordinator_mode()` parser, or a wiring gap in
//!    `build_system_prompt_for` / `print_system_prompt`.
//!
//! Both branches are backend-agnostic — the mock/live split for other
//! PTY families doesn't apply: `system-prompt` doesn't hit an
//! LLM backend at all.
//!
//! ```bash
//! cargo test --test pty_coordinator_mode
//! ```

mod common;

use common::TestEnv;

// Marker phrase from the ported coordinator prompt. Kept short enough
// (< 40 chars) to survive terminal line-wrapping at 80 cols without
// getting broken across two rows, and free of regex-special
// characters (`*` in particular) so pty-expect's regex-style pattern
// matcher accepts it as a literal substring.
const COORDINATOR_MARKER: &str = "orchestrates software engineering";

// ──────────────────────────────────────────────────────────────────────
// 1. Env off — coordinator prompt MUST NOT appear in the default path
// ──────────────────────────────────────────────────────────────────────

/// Fresh session with no coordinator env var. The `system-prompt`
/// output must not contain the coordinator marker. This test is the
/// baseline sanity — a regression that makes the coordinator prompt
/// always-on would nearly double the size of every prompt.
#[test]
fn coordinator_prompt_absent_without_env_var() {
    let env = TestEnv::new("coordinator-off");

    let mut sess = env.spawn(&["system-prompt"]);

    // Wait for the child to exit and observe rendered screen.
    let exit = sess.expect_eof().expect("scode system-prompt should exit");
    assert_eq!(exit, 0, "system-prompt turn should exit 0; got {exit}");

    let rendered = sess.render(|screen| screen.contents());
    assert!(
        !rendered.contains(COORDINATOR_MARKER),
        "Default system prompt must NOT contain the coordinator marker \"{COORDINATOR_MARKER}\" \
         — got output containing it. First 400 chars: {snip}",
        snip = &rendered.chars().take(400).collect::<String>(),
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. Env on — coordinator prompt MUST appear
// ──────────────────────────────────────────────────────────────────────

/// With `SUDOCODE_COORDINATOR_MODE=1`, the coordinator role prompt is
/// prepended to dynamic sections and MUST appear verbatim in the
/// `system-prompt` output.
#[test]
fn coordinator_prompt_present_with_env_var() {
    let env = TestEnv::new("coordinator-on");

    let mut sess = env.spawn_with_env(&["system-prompt"], &[("SUDOCODE_COORDINATOR_MODE", "1")]);

    // The marker string is the most-distinctive short phrase; wait for
    // it explicitly against the raw output STREAM (not the terminal
    // screen buffer — long system prompts get scrolled off the fixed
    // terminal render window, so render-based assertions are
    // unreliable). Stream-based expect is what regression-guards the
    // coordinator prompt injection end-to-end.
    sess.expect(COORDINATOR_MARKER)
        .expect("system-prompt output must contain the coordinator role marker verbatim");

    let exit = sess.expect_eof().expect("scode system-prompt should exit");
    assert_eq!(exit, 0, "system-prompt turn should exit 0; got {exit}");
}
