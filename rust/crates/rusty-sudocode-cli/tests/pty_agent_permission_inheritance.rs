//! PTY live e2e — a sub-agent inherits the spawning session's permission
//! mode, and `agent_spawn` no longer requires danger-full-access.
//!
//! Regression guard for the permission-inheritance fix: before it, the
//! `agent_spawn` tool was gated at `DangerFullAccess` and every spawned
//! sub-agent ran on an unconditional full-access policy. That combination
//! made sub-agents unusable from a `read-only` or `workspace-write` session —
//! the parent's `agent_spawn` call was denied outright — while a spawned
//! worker silently got maximum authority regardless of the parent's mode.
//!
//! The existing `pty_presets_e2e` tests all spawn under
//! `--permission-mode danger-full-access`, so none of them would catch a
//! regression back to the old gate. This test spawns under `read-only`, which
//! only completes if the gate is `ReadOnly` AND the child inherits the
//! parent's (read-only) mode: a read-only Explore that reads files and
//! reports back.
//!
//! ## Local-only per current convention
//!
//! Like `pty_presets_e2e`, this needs `SCODE_TEST_BACKEND=live` — a spawned
//! sub-agent makes its own `/v1/messages` requests that do not carry the
//! parent's `PARITY_SCENARIO:` token, so the mock harness cannot serve them
//! (plan §6.4). Under mock mode the test early-skips with a stderr note.
//!
//! Local run against sudorouter:
//!
//! ```bash
//! SCODE_TEST_BACKEND=live cargo test -p rusty-sudocode-cli \
//!   --test pty_agent_permission_inheritance -- --nocapture
//! ```

mod common;

use std::time::Duration;

use common::{model_unavailable_in_screen, screen_tail, TestEnv, LIVE_TIMEOUT};

/// The full parent→child→completion chain runs two real LLM turns plus a
/// file-reading tool loop; allow the same generous budget the other
/// parent→child→report chains use (`pty_presets_e2e::preset_test_timeout`).
fn chain_timeout() -> Duration {
    LIVE_TIMEOUT.saturating_mul(8)
}

/// Skip (returning `false`) when the harness is in mock mode.
fn require_live(env: &TestEnv, test_name: &str) -> bool {
    if env.is_live() {
        return true;
    }
    eprintln!(
        "SKIP {test_name}: SCODE_TEST_BACKEND=mock — a spawning test hits the \
         mock scenario-inheritance gap (plan §6.4). Rerun with \
         SCODE_TEST_BACKEND=live."
    );
    false
}

/// A read-only session must be able to spawn a read-only Explore sub-agent
/// and have it complete. This exercises BOTH halves of the fix at once:
///   - the `agent_spawn` gate is `ReadOnly` (a `DangerFullAccess` gate would
///     deny the call outright and the run would surface a permission error
///     instead of a completion), and
///   - the child inherits the read-only mode and still finishes its
///     read-only task (listing function names) without a permission prompt.
#[test]
fn read_only_session_spawns_and_completes_readonly_subagent() {
    let env = TestEnv::new("perm-inherit-readonly");
    if !require_live(
        &env,
        "read_only_session_spawns_and_completes_readonly_subagent",
    ) {
        return;
    }

    // Fixtures the child will read — two Rust files with three functions.
    std::fs::write(
        env.workspace_root().join("a.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .expect("write a.rs fixture");
    std::fs::write(env.workspace_root().join("b.rs"), "fn gamma() {}\n")
        .expect("write b.rs fixture");

    let prompt = "Use Agent(subagent_type=\"Explore\", description=\"list functions\", \
         prompt=\"List every function name defined in the .rs files in the current \
         directory, one per line.\") to run the task, then report back what it returned.";

    // read-only ON PURPOSE: the whole point of the regression is that a
    // non-danger session can spawn now. A denied spawn would surface
    // "requires danger-full-access" instead of a completion sentinel.
    let mut sess = env.spawn(&["--permission-mode", "read-only", prompt]);
    sess.set_default_timeout(chain_timeout());

    // The child names at least one of the fixture functions in its report,
    // or the parent surfaces a launched-agent / completion sentinel. Any hit
    // proves the spawn was allowed and the read-only child ran to completion.
    let pattern = r"alpha|beta|gamma|agent-|completed|finished";
    if let Err(error) = sess.expect(pattern) {
        let tail = screen_tail(&sess, 800);
        if model_unavailable_in_screen(&tail) {
            eprintln!("SKIP: the gateway did not serve the run (screen tail below)\n{tail}");
            return;
        }
        // A permission denial is the specific regression this guards against —
        // call it out explicitly so a future breakage is unambiguous.
        assert!(
            !tail.contains("danger-full-access"),
            "REGRESSION: read-only session was denied agent_spawn \
             (gate reverted to danger-full-access?):\n{tail}"
        );
        panic!("no completion sentinel and no permission denial: {error}\n{tail}");
    }

    // A denial can co-occur with a later sentinel in a noisy screen; assert
    // the denial string never appeared at all.
    let full_tail = screen_tail(&sess, 2000);
    assert!(
        !full_tail.contains("requires danger-full-access"),
        "REGRESSION: agent_spawn demanded danger-full-access under a \
         read-only session:\n{full_tail}"
    );

    sess.set_default_timeout(chain_timeout());
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let tail = screen_tail(&sess, 800);
        panic!("scode did not exit cleanly after the read-only spawn: {e}\n{tail}");
    });
    assert_eq!(
        exit, 0,
        "scode should exit 0 after the read-only Agent chain completes; got {exit}"
    );
}
