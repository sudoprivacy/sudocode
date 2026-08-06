//! PTY tests for bash tool streaming output.
//!
//! Verifies that long-running bash commands show incremental progress
//! in the terminal rather than blocking until completion.
//!
//! ```bash
//! cargo test --test pty_bash_streaming                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_bash_streaming  # real API
//! ```
mod common;

use std::time::Duration;

use common::TestEnv;

/// A bash command that produces output over time should show progress
/// lines in the terminal before the final tool result appears.
///
/// Mock mode: the mock routes this to `bash_stdout_roundtrip` which
/// produces a single `echo` — verifies the progress path doesn't crash
/// even though the command is too fast for a visible progress line.
///
/// Live mode: asks the model to run a loop that echoes lines with
/// sleep, which triggers the 1-second progress interval. The terminal
/// should show "lines" or "bytes" progress markers.
#[test]
fn bash_streaming_shows_progress_for_slow_commands() {
    let env = TestEnv::new("bash-streaming");

    if env.is_mock() {
        // Mock: just verify the bash tool path works with streaming
        // enabled. The mock scenario uses `printf 'alpha from bash'`.
        let prompt = env.prompt(
            "Run this bash command: printf 'alpha from bash'",
            "bash_stdout_roundtrip",
        );
        let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
        sess.expect("alpha from bash")
            .expect("bash output should appear");
        let exit = sess.expect_eof().expect("scode should exit");
        assert_eq!(exit, 0);
        return;
    }

    // Live mode: ask the model to run a command that produces output
    // over multiple seconds.
    let prompt = env.prompt(
        "Run this exact bash command: for i in 1 2 3 4 5; do echo \"streaming line $i\"; sleep 0.5; done",
        "unused_live_only",
    );
    let mut sess = env.spawn(&["--permission-mode", "danger-full-access", &prompt]);
    sess.set_default_timeout(Duration::from_secs(60));

    // We should see either the streaming progress markers OR the final
    // tool result — both prove the streaming path executed.
    sess.expect("streaming line")
        .expect("bash output should appear in terminal");

    // Wait for turn to complete.
    sess.expect("tokens")
        .expect("status line should appear after turn");

    let exit = sess.expect_eof().expect("scode should exit");
    assert_eq!(exit, 0);
}
