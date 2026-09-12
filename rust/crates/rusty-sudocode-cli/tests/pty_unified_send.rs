//! PTY test — unified `send` tool alias routing end-to-end.
//!
//! Verifies that the canonical `send` tool name (not the deprecated
//! `SendMessage`) dispatches through the real binary, writes the JSONL
//! envelope to `.sudocode-inbox/<recipient>.jsonl`, and returns a
//! success response that the mock server echoes back.
//!
//! Mock-safe: runs in CI without API keys.

mod common;

use std::time::Duration;

use common::TestEnv;

#[test]
fn send_tool_writes_envelope_and_roundtrips() {
    let env = TestEnv::new("unified-send");

    let prompt = env.prompt(
        "Send a message to test-peer saying hello from unified send.",
        "unified_send_roundtrip",
    );

    let mut sess = env.spawn(&[
        "--permission-mode",
        "workspace-write",
        "--allowedTools",
        "send",
        &prompt,
    ]);

    // The mock model calls `send` → scode dispatches through alias
    // routing → writes envelope → returns success JSON → mock echoes
    // the final text.
    sess.set_default_timeout(Duration::from_secs(15));
    sess.expect("(?i)(unified send roundtrip complete|Message sent to)")
        .unwrap_or_else(|e| {
            let screen = sess.render(|s| s.contents());
            panic!(
                "send roundtrip text not found: {e}\ntail:\n{tail}",
                tail = screen
                    .chars()
                    .rev()
                    .take(800)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>(),
            );
        });

    let exit = sess.expect_eof().unwrap_or_else(|e| {
        let screen = sess.render(|s| s.contents());
        panic!(
            "scode did not exit: {e}\ntail:\n{tail}",
            tail = screen
                .chars()
                .rev()
                .take(800)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
    });
    assert_eq!(exit, 0, "unified send test should exit 0");

    // Disk verification: the envelope must exist in the workspace's
    // `.sudocode-inbox/test-peer.jsonl`.
    let inbox = env
        .workspace_root()
        .join(".sudocode-inbox")
        .join("test-peer.jsonl");
    let content = common::read_file_with_retry(&inbox, 5, Duration::from_millis(200));
    assert!(
        content.is_some(),
        "envelope file should exist at {}",
        inbox.display()
    );
    let content = content.unwrap();
    assert!(
        content.contains("hello from unified send"),
        "envelope should contain the message body; got: {content}"
    );
}
