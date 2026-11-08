//! Live PTY test for the interactive tool-permission approval prompt.
//!
//! In `--permission-mode workspace-write`, a tool that requires
//! `danger-full-access` (e.g. `bash`) asks for approval. The REPL presents the
//! request in its choice panel, the same one the `write_plan` review uses, so
//! the answer never spawns a second terminal owner mid-turn. This test drives
//! that path end-to-end: the request appears, "1" (Allow once) is accepted and
//! the command runs.
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
fn permission_prompt_allow_runs_the_tool() {
    let env = TestEnv::new("perm-prompt");
    if env.is_mock() {
        eprintln!("SKIP: mock backend does not emit a permission-gated tool call");
        return;
    }

    // workspace-write + a bash command → bash requires danger-full-access →
    // the REPL asks for approval.
    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "bash",
    ]);
    common::expect_input_line_cleared(&sess, LIVE_TIMEOUT, "REPL input ready");
    let marker = common::turn_status_marker(&sess);
    sess.send("Run the bash command `echo perm_prompt_ok` and report its output.\r")
        .expect("send prompt");

    // If the model declines to use bash, skip gracefully: the choice panel is
    // covered deterministically by the write_plan review tests.
    let long = LIVE_TIMEOUT.saturating_mul(3);
    let deadline = std::time::Instant::now() + long;
    loop {
        let screen = sess.render(common::screen_contents);
        if screen.contains("Allow this tool call?") && screen.contains("Allow once") {
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("SKIP: live model did not attempt bash (no permission request shown)");
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Quick-select "1" (Allow once); the command then runs and its output surfaces.
    sess.send("1").expect("allow once");
    common::expect_turn_complete_after(&sess, &marker, long, "allowed command completes");
    common::expect_screen(
        &sess,
        |screen| screen.contains("perm_prompt_ok"),
        common::DEFAULT_TIMEOUT,
        "allowed bash command output should surface",
    );

    // Best-effort teardown (the suite's tolerant pattern for post-turn /exit).
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    let _ = sess.expect_eof();
}
