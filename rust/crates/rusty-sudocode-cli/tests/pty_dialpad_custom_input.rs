//! PTY regression test: the `DialPad`'s built-in custom-input row accepts typed
//! text in-place (async/iocraft REPL, the default for a human).
//!
//! Regression: typing directly on the `[+]` row (without pressing Enter first)
//! used to switch input slots mid-keystroke-burst and drop every char but the
//! first — the reported "typing on `[+]` just submits one letter / exits". The
//! `DialPad` now owns an in-place text buffer (like `FuzzySelect`'s filter), so a
//! fast burst is captured whole.
//!
//! Runs in mock (deterministic `write_plan` dialog) and live.
//!
//! ```bash
//! cargo test --test pty_dialpad_custom_input                          # mock (CI)
//! SCODE_TEST_BACKEND=live cargo test --test pty_dialpad_custom_input  # real API
//! ```

mod common;

use std::time::Duration;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::Value;

#[inline]
fn expect_mock_feedback(env: &TestEnv, sess: &mut PtySession, expected: &str) {
    sess.expect("write_plan roundtrip complete:")
        .expect("the model must respond after receiving the submitted feedback");
    let has_feedback = env.captured_message_bodies().iter().any(|body| {
        let request: Value = serde_json::from_str(body).expect("parse captured model request");
        let Some(messages) = request["messages"].as_array() else {
            return false;
        };
        messages.iter().any(|message| {
            let Some(blocks) = message["content"].as_array() else {
                return false;
            };
            blocks.iter().any(|block| {
                if block["type"] != "tool_result" {
                    return false;
                }
                let content = &block["content"];
                let parts = content
                    .as_array()
                    .map_or(std::slice::from_ref(content), Vec::as_slice);
                parts.iter().any(|part| {
                    let result = if part["type"] == "json" {
                        Some(part["value"].clone())
                    } else {
                        part.as_str()
                            .or_else(|| part["text"].as_str())
                            .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    };
                    result.is_some_and(|value| value["feedback"].as_str() == Some(expected))
                })
            })
        })
    });
    assert!(
        has_feedback,
        "the actual API tool result must contain the entire feedback: {expected:?}"
    );
}

#[inline]
fn exit_after_feedback(env: &TestEnv, sess: &mut PtySession) {
    sess.send("/exit\r").expect("send /exit");
    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    let result = sess.expect_eof();
    if env.is_mock() {
        assert_eq!(result.expect("exit after delivered feedback"), 0);
    }
}

#[test]
fn dialpad_custom_input_row_accepts_typed_text_in_place() {
    let env = TestEnv::new("dialpad-custom");

    // Async/queue REPL (iocraft) — the path a human uses by default.
    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call write_plan with a short markdown plan as `content`. Just call the tool.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    if sess.expect("Choose an action").is_err() {
        if env.is_live() {
            eprintln!("SKIP: live model did not call write_plan");
            return;
        }
        panic!("dialog should appear");
    }

    // Arrow down past the 4 options onto the `[+]` custom-input row, then type
    // DIRECTLY (no Enter first) — the previously-broken path.
    for _ in 0..4 {
        sess.send("\x1b[B").expect("down");
        std::thread::sleep(Duration::from_millis(80));
    }
    let marker = "regressioncomment";
    sess.send(marker).expect("type comment");
    std::thread::sleep(Duration::from_millis(400));
    sess.send("\r").expect("submit comment");

    // The WHOLE comment must reach the model as feedback (not just the first
    // char). The mock echoes the tool result verbatim; live re-plans on it.
    sess.set_default_timeout(common::at_least(Duration::from_mins(1)));
    if env.is_mock() {
        expect_mock_feedback(&env, &mut sess, marker);
        sess.expect(marker)
            .expect("the full typed comment must reach the model, not a single char");
    } else {
        // Live: the model consumed the feedback and re-presented a plan, or
        // answered; either way the turn completes without error.
        let _ = sess.expect("Choose an action");
    }

    exit_after_feedback(&env, &mut sess);
}

/// Bracketed paste into the `DialPad` `[+]` row must work — including multi-line
/// content (collapsed to a placeholder, expanded to the full text on submit).
/// Regression: the `DialPad` path ignored `TerminalEvent::Paste` entirely.
#[test]
fn dialpad_custom_input_row_accepts_bracketed_paste() {
    let env = TestEnv::new("dialpad-paste");

    let mut sess = env.spawn_with_env(
        &[
            "--permission-mode",
            "workspace-write",
            "--allowedTools",
            "write_plan",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.expect("❯").expect("should see REPL prompt");

    let prompt = env.prompt(
        "Call write_plan with a short markdown plan as `content`. Just call the tool.",
        "write_plan_roundtrip",
    );
    sess.send(&format!("{prompt}\r")).expect("send prompt");

    sess.set_default_timeout(common::at_least(Duration::from_secs(30)));
    if sess.expect("Choose an action").is_err() {
        if env.is_live() {
            eprintln!("SKIP: live model did not call write_plan");
            return;
        }
        panic!("dialog should appear");
    }

    // Arrow onto the `[+]` row, then paste multi-line content via the bracketed
    // sequence a real terminal emits. >2 lines collapses to a placeholder; the
    // full text must be restored at submit time.
    for _ in 0..4 {
        sess.send("\x1b[B").expect("down");
        std::thread::sleep(Duration::from_millis(80));
    }
    let line_a = "pastedlinealpha";
    let line_b = "pastedlinebravo";
    let payload = format!("{line_a}\n{line_b}\nline3\nline4");
    sess.send(&format!("\x1b[200~{payload}\x1b[201~"))
        .expect("bracketed paste");
    std::thread::sleep(Duration::from_millis(400));
    sess.send("\r").expect("submit");

    sess.set_default_timeout(common::at_least(Duration::from_mins(1)));
    if env.is_mock() {
        expect_mock_feedback(&env, &mut sess, &payload);
        // The tool result echoes the feedback verbatim — the full pasted text
        // (both marker lines) must be present, proving paste reached the buffer
        // and expanded on submit (not lost, not left as a placeholder).
        sess.expect(line_a)
            .expect("pasted line A must reach the model");
        sess.expect(line_b)
            .expect("pasted line B must reach the model");
    }

    exit_after_feedback(&env, &mut sess);
}
