//! Live PTY test for the interactive tool-permission approval prompt.
//!
//! In `--permission-mode workspace-write`, a tool that requires
//! `danger-full-access` (e.g. `bash`) triggers an escalation prompt that the
//! sync REPL answers through `CliPermissionPrompter`. That prompter reads the
//! choice through the SHARED rustyline editor (not a throwaway `dialoguer::Select`
//! that grabbed its own crossterm raw session mid-turn) — the same single-owner
//! fix as the write_plan dialog. This test drives that path end-to-end: the box
//! appears, "1" (Allow) is accepted through the shared editor, and the turn
//! proceeds.
//!
//! Live-only: only a real model reliably decides to run a bash command, and the
//! mock backend doesn't emit a permission-gated tool call.
//!
//! ```bash
//! SCODE_TEST_BACKEND=live cargo test --test pty_permission_prompt  # real API
//! ```

mod common;

use std::time::Duration;

use common::{TestEnv, LIVE_TIMEOUT};

#[test]
fn permission_prompt_allow_is_read_through_shared_editor() {
    let env = TestEnv::new("perm-prompt");
    if env.is_mock() {
        eprintln!("SKIP: mock backend does not emit a permission-gated tool call");
        return;
    }

    // workspace-write + a bash command → bash requires danger-full-access →
    // the REPL shows the escalation prompt.
    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "bash",
    ]);
    sess.expect("❯").expect("should see REPL prompt");

    sess.send("Run the bash command `echo perm_prompt_ok` and report its output.\r")
        .expect("send prompt");

    let long = LIVE_TIMEOUT.saturating_mul(3);
    sess.set_default_timeout(long);

    // The escalation box must appear. If the model declines to use bash, skip
    // gracefully (the shared-editor read path is unit-covered by the write_plan
    // dialog; this test adds the permission-specific live coverage when reachable).
    if sess.expect("(?i)Permission required").is_err() {
        eprintln!("SKIP: live model did not attempt bash (no permission prompt shown)");
        return;
    }
    sess.expect("(?i)Approve this tool call")
        .expect("should show the approve prompt read via the shared editor");

    // Answer "1" (Allow) through the shared editor — the previously-throwaway path.
    sess.send("1\r").expect("send Allow");

    // With the tool allowed, the command runs and its output surfaces.
    sess.expect("perm_prompt_ok")
        .expect("allowed bash command output should surface");

    // Best-effort teardown (the suite's tolerant pattern for post-turn /exit).
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(Duration::from_secs(30));
    let _ = sess.expect_eof();
}
