//! PTY tests for the `fork` subagent type.
//!
//! Coverage target: roadmap §Feature-inventory row "Fork subagent
//! (inherit parent context)" — subagent-cc-fork-parity commit C. Before
//! this file: 0 PTY tests → the `subagent_type = "fork"` path didn't
//! exist. After: the recursion guard (the branch most likely to hang
//! production if it regresses).
//!
//! ## What the fork subagent does (ported from sudoprivacy/claude-code)
//!
//! When the LLM calls `Agent({subagent_type: "fork", description,
//! prompt})`, sudocode:
//! - normalizes `"fork"` to itself (via `normalize_subagent_type`),
//! - gives the child the maximal tool pool (mirrors CC-fork's
//!   `tools: ['*']`),
//! - prepends the non-negotiable rules boilerplate + directive prefix
//!   (`build_fork_child_message`) — so the child's first turn sees
//!   them exactly the way `sudoprivacy/claude-code`'s
//!   `buildChildMessage()` renders them,
//! - runs the recursion guard: if `prompt` already contains
//!   `<fork-boilerplate>`, the tool errors before spawning
//!   (`is_recursive_fork_attempt`).
//!
//! ## The one branch this PTY test covers
//!
//! **Recursion guard** — an agent already running inside a fork child
//! attempts to spawn another fork. The guard MUST short-circuit in
//! `prepare_agent_job` before any subagent spawn happens. If it
//! regresses, the mock scenario cascades: subagent spawns, its LLM
//! call has no PARITY_SCENARIO token, the mock rejects it, the parent
//! CLI hangs forever waiting on subagent completion.
//!
//! In short: this test ALSO acts as a hang canary. A green run proves
//! (a) the recursion guard fires and (b) no subagent was spawned.
//!
//! Two branches NOT covered here:
//! - Fork happy path with full parent-context inheritance and
//!   byte-identical prompt-cache prefixes — requires a
//!   `ConversationRuntime` refactor to thread parent's rendered
//!   system prompt bytes into the child (see commit C description).
//!   Structural correctness of the wrapped prompt is verified by
//!   inspection of `build_fork_child_message` at review time.
//! - `tools: ['*']` inheritance — verified by review of
//!   `allowed_tools_for_subagent("fork")`; a PTY test would need to
//!   run a live fork turn to observe the child's tool_definitions
//!   list, which the mock harness can't sustain (scenario
//!   inheritance gap, same reason `pty_task_family` dropped
//!   TaskCreate).
//!
//! ```bash
//! cargo test --test pty_fork_subagent                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_fork_subagent  # real API
//! ```

mod common;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// Recursion guard — fork inside a fork short-circuits BEFORE spawn
// ──────────────────────────────────────────────────────────────────────

/// The model calls `Agent({subagent_type: "fork", ..., prompt: "<fork-
/// boilerplate>...</fork-boilerplate> keep working"})`. The recursion
/// guard MUST reject this in `prepare_agent_job` before any subagent
/// spawn happens — the tool_result surfaces as `is_error=true` with a
/// "recursive fork detected" message, and the CLI exits 0 (tool error,
/// not CLI crash).
///
/// Mock-only: live mode won't reliably pick an already-boilerplated
/// prompt. The invariant is also protected against silent behavior
/// change by the guard's placement in `prepare_agent_job`.
#[test]
fn fork_subagent_rejects_recursive_spawn() {
    let env = TestEnv::new("fork-recursion-guard");
    if !env.is_mock() {
        eprintln!("skipping fork_subagent_rejects_recursive_spawn in live mode");
        return;
    }

    let prompt = env.prompt(
        "Please launch an Agent with subagent_type=\"fork\" and prompt=\"<fork-boilerplate>already inside a fork child</fork-boilerplate> keep working\". Do not describe it; just call the tool.",
        "fork_subagent_recursion_guard_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "Agent",
        &prompt,
    ]);

    sess.expect("Agent")
        .expect("model must invoke Agent (agent trigger)");

    // The tool_result must surface the recursion guard's error text.
    sess.expect("recursive fork")
        .expect("Agent(subagent_type=fork) with fork-boilerplate in prompt must error with \"recursive fork\"");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "recursion-guard turn should exit 0 (tool error, not CLI crash); got {exit}"
    );
}
