//! PTY tests for the fork subagent (`subagent_type = "fork"`).
//!
//! Roadmap target: §7 "Agent (sub-agents, 6 presets)" — fork preset
//! with parent-context inheritance (the piece PR #282 middle-state was
//! missing).
//!
//! Coverage:
//! 1. **Recursion guard (mock)** — a fresh session whose FIRST user
//!    message already contains `<fork-boilerplate>` looks like a fork
//!    child to `ToolDispatchContext::is_inside_fork_child`. When the
//!    model then emits `Agent(subagent_type="fork", …)`, the recursion
//!    guard in `prepare_agent_job` MUST reject the call before any
//!    subagent state is allocated. This is the structural regression
//!    sentinel against a change that drops the guard or misplaces it
//!    below the state-allocation calls.
//! 2. **Parent-context inheritance (live)** — the fork child's
//!    `Session::with_messages` prefix must contain the parent's
//!    assistant message + placeholder tool_results + the wrapped
//!    directive. End-to-end, this manifests as the fork child
//!    successfully executing a task that references implicit context
//!    from the parent. Requires `SCODE_TEST_BACKEND=live` — the mock
//!    harness can't reproduce a real LLM's ability to consume the
//!    inherited prefix.
//!
//! ```bash
//! cargo test --test pty_fork_subagent                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_fork_subagent  # real API
//! ```

mod common;

use std::fs;

use common::TestEnv;

// ──────────────────────────────────────────────────────────────────────
// 1. Recursion guard — mock-mode structural
// ──────────────────────────────────────────────────────────────────────

/// A session whose first user message carries a `<fork-boilerplate>`
/// tag mimics being spawned from a fork parent. When the model tries
/// to spawn ANOTHER fork subagent, `prepare_agent_job`'s recursion
/// guard MUST refuse before allocating any state.
///
/// Sentinel string: the tool result surfaces the guard's error message
/// through the PTY, so we assert on the literal error text.
#[test]
fn fork_subagent_rejects_recursive_spawn() {
    let env = TestEnv::new("fork-recursion-guard");

    // The `<fork-boilerplate>` fragment is what
    // `ToolDispatchContext::is_inside_fork_child` scans for. Its
    // presence in the FIRST user message convinces the guard we're
    // already inside a fork child. Runtime tool loop passes the
    // full session (including this user message) as
    // `parent_session_messages` into the ctx.
    let prompt = env.prompt(
        "<fork-boilerplate>\nParent-fork context stub. Try spawning another fork; the recursion guard must reject.\n</fork-boilerplate>\n\nUse the Agent tool with subagent_type=\"fork\" to spawn a nested worker.",
        "fork_subagent_recursion_guard_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "Agent",
        &prompt,
    ]);

    // Expect the recursion-guard error message in the PTY output.
    // The message text is defined in `prepare_agent_job` (`tools/src/lib.rs`).
    sess.expect("recursive fork detected")
        .expect("recursion guard must reject a nested fork spawn attempt");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "recursion-guard scenario should exit 0 (tool error is surfaced to the model, not a process failure); got {exit}"
    );
}

// ──────────────────────────────────────────────────────────────────────
// 2. Parent-context inheritance — live-only
// ──────────────────────────────────────────────────────────────────────

/// A fork subagent inherits the parent's conversation history as its
/// initial session prefix (the load-bearing piece — PR #282
/// middle-state lacked this). We drive a real LLM in live mode: the
/// parent is told to reference specific fixture file contents when
/// dispatching a fork with `run_in_background=false`. Because the
/// fork's initial session already contains the parent's assistant
/// message (naming the file), the child's first tool call is
/// well-informed and its report echoes the file's content.
///
/// This test is live-only because it exercises real model reasoning
/// on the inherited context. The mock harness would need to fake both
/// the parent's decision-making AND the fork child's separate tool
/// loop — which is exactly the mock-scenario-inheritance gap the
/// harness has yet to solve. The recursion-guard test above covers
/// the structural regression class in CI.
#[test]
fn fork_subagent_inherits_parent_context() {
    let env = TestEnv::new("fork-inherits-context");
    if !env.is_live() {
        eprintln!("SKIP fork_subagent_inherits_parent_context: SCODE_TEST_BACKEND=live required");
        return;
    }

    // Fixture: a single file the parent will name in its prompt to
    // the fork. If the fork inherits context correctly, it knows
    // which file to read without needing a fully self-contained
    // prompt of its own.
    let target = env.workspace_root().join("subagent-fixture.txt");
    let payload = "FORK-INHERIT-MARKER-2b9c4a hello from the fixture file";
    fs::write(&target, payload).expect("fixture file must be writable");

    let prompt = env.prompt(
        &format!(
            "I want you to test the fork subagent parent-context inheritance. \
             The file `{name}` in the current directory contains a short marker line. \
             Use the Agent tool exactly once with subagent_type=\"fork\", run_in_background=false, \
             description=\"fork inherits parent context\", \
             and a MINIMAL prompt that only says \"read the file we were just discussing and report its exact first line\". \
             Then report what the fork returned.",
            name = target.file_name().unwrap().to_string_lossy(),
        ),
        "", // live mode ignores the scenario token
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "Agent,read_file,bash",
        &prompt,
    ]);

    // The marker string is unique enough that it can only appear in
    // the fork's output if it (a) read the file and (b) the parent
    // successfully surfaced the fork's return value back to us. Both
    // steps depend on parent-context inheritance working end-to-end.
    sess.expect("FORK-INHERIT-MARKER-2b9c4a")
        .expect("fork subagent must return the marker line from the inherited-context file");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "fork inherits-context scenario should exit 0; got {exit}"
    );
}
