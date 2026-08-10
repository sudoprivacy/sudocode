//! PTY tests for the iocraft REPL auto_grow exit regression guard.
//!
//! Interactive features (history, tab completion) are validated by
//! unit tests in `repl_ui::tests` — PTY escape sequence delivery
//! timing makes arrow-key / Tab tests unreliable across CI runners.

mod common;

use common::TestEnv;
use std::time::Duration;

/// P0 regression guard: `/exit` in the iocraft REPL (with auto_grow
/// enabled) must not hang.
///
/// If the render loop's `should_exit()` check or `TextBufferView`
/// dimension caching is broken, `mark_dirty` keeps the component
/// "dirty" indefinitely and this test times out.
///
/// Journey: boot → prompt visible → `/exit` → process exits cleanly.
#[test]
#[cfg(unix)]
fn iocraft_repl_auto_grow_exit_no_hang() {
    let env = TestEnv::new("iocraft-exit");
    let root = env.workspace_root().to_path_buf();
    std::fs::write(root.join("AGENTS.md"), "# Rules\n").expect("write AGENTS.md");

    let mut sess = env.spawn_with_env(
        &["--permission-mode", "read-only"],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.set_default_timeout(Duration::from_secs(10));

    sess.expect("❯").unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("prompt: {e}\nPTY:\n{screen}");
    });

    sess.send("/exit\r").expect("send /exit");
    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!("auto_grow exit must not hang: {e}\nPTY:\n{screen}");
    });
    assert_eq!(exit, 0, "clean exit code");
}
