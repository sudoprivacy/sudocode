//! Live PTY test: verify /model switch actually changes the model.
//!
//! Requires SCODE_TEST_BACKEND=live.

mod common;

use common::TestEnv;
use std::time::Duration;

#[test]
fn model_switch_changes_active_model() {
    let env = TestEnv::new("model-switch");

    if env.is_mock() {
        eprintln!("model_switch_changes_active_model: skipped in mock mode (requires SCODE_TEST_BACKEND=live)");
        return;
    }

    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(20));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("initial prompt: {e}\nPTY:\n{screen}");
    });

    // Switch to a specific model via /model <name>
    sess.send("/model claude-sonnet-4-6\r").expect("send /model");
    sess.expect("Model updated").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("model switch report: {e}\nPTY:\n{screen}");
    });

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after switch: {e}\nPTY:\n{screen}");
    });

    // Ask the model what it is
    sess.send("What model are you? Reply with only your model ID, nothing else.\r")
        .expect("send question");

    // The response should mention sonnet-4, not opus
    sess.expect("(?i)sonnet").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("model should identify as sonnet after switch: {e}\nPTY:\n{screen}");
    });

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt after response: {e}\nPTY:\n{screen}");
    });

    // Switch back to auto
    sess.send("/model auto\r").expect("send /model auto");
    sess.expect("Model updated").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("model switch back: {e}\nPTY:\n{screen}");
    });

    // Clean exit
    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("exit: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
