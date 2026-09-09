//! PTY tests for the deferred tools mechanism (CC parity).
//!
//! Verifies the `ExecuteExtraTool` → deferred tool dispatch roundtrip:
//! the mock LLM emits an `ExecuteExtraTool` call with `tool_name: "CronList"`,
//! scode dispatches it, and the model sees the CronList result.
//!
//! ```bash
//! cargo test --test pty_deferred_tools                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_deferred_tools  # real API
//! ```
mod common;

use common::TestEnv;

#[test]
fn execute_extra_tool_roundtrip() {
    let env = TestEnv::new("execute-extra-tool");
    let prompt = env.prompt(
        "List all scheduled cron tasks using ExecuteExtraTool.",
        "execute_extra_tool_roundtrip",
    );

    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);

    sess.expect("roundtrip complete")
        .expect("should see roundtrip completion message");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(
        exit, 0,
        "execute_extra_tool roundtrip should exit 0; got {exit}"
    );
}
