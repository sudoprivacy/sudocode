//! PTY tests for Coordinator Mode.
//!
//! Coverage target: roadmap §Feature-inventory row "Coordinator Mode
//! (multi-worker orchestration prompt)" — subagent-cc-fork-parity
//! commit D. Before this file: 0 PTY tests → the coordinator prompt
//! didn't exist. After: env-var toggle drives the prompt swap and the
//! ported role phrasing lands in `scode print-system-prompt`.
//!
//! ## What Coordinator Mode does (ported from sudoprivacy/claude-code)
//!
//! When `SUDOCODE_COORDINATOR_MODE=1` (mirrors CC-fork's
//! `CLAUDE_CODE_COORDINATOR_MODE`), the runtime prepends the ported
//! `coordinator_system_prompt()` to the system prompt's dynamic
//! sections. This gives the model a coordinator role directive
//! (Research → Synthesis → Implementation → Verification, delegate via
//! Agent + SendMessage, stop via TaskStop) that takes primacy over the
//! default identity.
//!
//! ## Two branches this PTY test covers
//!
//! 1. **Env off / default** — `scode print-system-prompt` MUST NOT
//!    contain the coordinator role phrase. Regression sentinel
//!    against "coordinator prompt leaks into the default path" (which
//!    would double the token cost of every non-coordinator turn).
//!
//! 2. **Env on** — `SUDOCODE_COORDINATOR_MODE=1 scode
//!    print-system-prompt` MUST contain the coordinator role phrase.
//!    Regression sentinel against a rename of the env var, a broken
//!    `is_coordinator_mode()` parser, or a wiring gap in
//!    `build_system_prompt_for` / `print_system_prompt`.
//!
//! Both branches are backend-agnostic — the mock/live split for other
//! PTY families doesn't apply: `print-system-prompt` doesn't hit an
//! LLM backend at all.
//!
//! ```bash
//! cargo test --test pty_coordinator_mode
//! ```

mod common;

use common::TestEnv;

// Marker phrase from the ported coordinator prompt. Chosen to be
// specific enough that it can't match any other section of the
// default sudocode identity prompt.
const COORDINATOR_MARKER: &str = "You are a **coordinator**";

// ──────────────────────────────────────────────────────────────────────
// 1. Env off — coordinator prompt MUST NOT appear in the default path
// ──────────────────────────────────────────────────────────────────────

/// Fresh session with no coordinator env var. The `print-system-prompt`
/// output must not contain the coordinator marker. This test is the
/// baseline sanity — a regression that makes the coordinator prompt
/// always-on would nearly double the size of every prompt.
#[test]
fn coordinator_prompt_absent_without_env_var() {
    let env = TestEnv::new("coordinator-off");

    let mut sess = env.spawn(&["print-system-prompt"]);

    // Wait for the child to exit and observe rendered screen.
    let exit = sess.expect_eof().expect("scode print-system-prompt should exit");
    assert_eq!(exit, 0, "print-system-prompt turn should exit 0; got {exit}");

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
/// `print-system-prompt` output.
#[test]
fn coordinator_prompt_present_with_env_var() {
    let env = TestEnv::new("coordinator-on");

    let mut sess = env.spawn_with_env(
        &["print-system-prompt"],
        &[("SUDOCODE_COORDINATOR_MODE", "1")],
    );

    // The marker string is the most-distinctive short phrase; wait for
    // it explicitly before the CLI exits so we can catch a truncation
    // bug (screen render might drop later content).
    sess.expect("coordinator")
        .expect("print-system-prompt output must include the coordinator role");

    let exit = sess.expect_eof().expect("scode print-system-prompt should exit");
    assert_eq!(exit, 0, "print-system-prompt turn should exit 0; got {exit}");

    let rendered = sess.render(|screen| screen.contents());
    assert!(
        rendered.contains(COORDINATOR_MARKER),
        "SUDOCODE_COORDINATOR_MODE=1 must inject the coordinator role prompt containing \
         \"{COORDINATOR_MARKER}\"; got output NOT containing it. First 400 chars: {snip}",
        snip = &rendered.chars().take(400).collect::<String>(),
    );
}
