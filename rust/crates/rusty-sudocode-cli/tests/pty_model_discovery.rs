//! A model published by the selected endpoint reaches a running REPL picker.
mod common;
use common::TestEnv;
use std::time::{Duration, Instant};

#[test]
fn endpoint_catalog_reaches_the_running_model_picker() {
    let env = TestEnv::new("model-discovery");
    env.set_model_catalog(serde_json::json!({"data": [{
        "id": "claude-unseen-2040", "context_window": 1234567,
        "max_output_tokens": 98765, "vision_supported": true
    }]}));
    let mut session = env.spawn(&["--permission-mode", "read-only"]);
    session.expect("\u{276f}").expect("REPL prompt");
    // Wait for the real child to finish discovery, then exercise its in-memory
    // picker. Reading disk alone would miss the original immutable-cache bug.
    let deadline = Instant::now() + env.timeout();
    loop {
        let loaded = std::fs::read_dir(env.config_home().join("cache/model-catalogs"))
            .ok()
            .is_some_and(|files| {
                files.filter_map(Result::ok).any(|file| {
                    std::fs::read_to_string(file.path())
                        .ok()
                        .is_some_and(|body| !env.is_mock() || body.contains("claude-unseen-2040"))
                })
            });
        if loaded {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "child did not discover its endpoint catalog"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    session.send("/model").expect("type model command");
    common::expect_input_line(&session, "/model", env.timeout(), "model command");
    session.send("\r").expect("open picker");
    session.expect("Select model").expect("model picker");
    if env.is_mock() {
        session
            .expect("claude-unseen-2040")
            .expect("new upstream model in live picker");
    }
    session.send("\x1b").expect("dismiss picker");
    session.expect("\u{276f}").expect("return to REPL");
    session.send("/exit").expect("type exit");
    common::expect_input_line(&session, "/exit", env.timeout(), "exit command");
    session.send("\r").expect("submit exit");
    assert_eq!(session.expect_eof().expect("clean exit"), 0);
    if env.is_mock() {
        assert_eq!(env.captured_message_count(), 0);
    }
}
