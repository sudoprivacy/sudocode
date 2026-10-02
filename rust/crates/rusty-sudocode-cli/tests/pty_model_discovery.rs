//! A model published by the selected endpoint reaches a running REPL picker.
mod common;
use common::TestEnv;
use std::time::{Duration, Instant};

#[test]
// Screen borrows the parser; the method item cannot satisfy render's HRTB.
#[allow(clippy::redundant_closure_for_method_calls)]
fn text_only_endpoint_rejects_agent_work_before_inference() {
    let env = TestEnv::new("text-only-model");
    let model = if env.is_mock() {
        "claude-sonnet-4-6".to_string()
    } else {
        std::env::var("SCODE_LIVE_MODEL")
            .expect("set SCODE_LIVE_MODEL to a documented text-only deployment")
    };
    env.set_model_catalog(serde_json::json!({"data": [{
        "id": model, "tool_calling_supported": false
    }]}));
    // Do not prime the cache: the first prompt must wait for initial discovery.
    let prompt = env.prompt(
        "What is 2+2? Answer with just the number.",
        "single_turn_text",
    );
    let trace_path = env.workspace_root().join("capability-events.jsonl");
    let mut session = env.spawn_with_env(
        &["--compact", "--permission-mode", "read-only", &prompt],
        &[("SCODE_LOG_PATH", trace_path.to_str().unwrap())],
    );
    // Observe the rendered error before EOF: ConPTY can report process exit
    // while its final output is still being drained by the reader thread.
    session.expect("tool calling").unwrap_or_else(|error| {
        panic!(
            "capability error missing: {error}; {}",
            session.render(|screen| screen.contents())
        )
    });
    assert_ne!(session.expect_eof().expect("failed turn exits"), 0);
    let screen = session.render(|screen| screen.contents());
    assert!(
        screen.contains("does not support") && screen.contains("tool calling"),
        "missing capability error in terminal: {screen}"
    );
    let events: Vec<serde_json::Value> = std::fs::read_to_string(trace_path)
        .expect("capability trace")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert!(events
        .iter()
        .any(|event| event["event"] == "model_capability_rejected"
            && event["attributes"]["model"] == model
            && event["attributes"]["capability"] == "tool_calling"));
    assert!(
        !events.iter().any(|event| matches!(
            event["event"].as_str(),
            Some("request_debug" | "request_succeeded" | "request_failed")
        )),
        "unsupported request attempted inference"
    );
    if env.is_mock() {
        assert_eq!(
            env.captured_message_count(),
            0,
            "unsupported request reached inference"
        );
        // The same ID on another connection with unknown support stays usable.
        let other = TestEnv::new("unknown-tool-capability");
        other.set_model_catalog(serde_json::json!({"data": [{"id": model}]}));
        let prompt = other.prompt(
            "What is 2+2? Answer with just the number.",
            "single_turn_text",
        );
        let mut session = other.spawn(&["--compact", "--permission-mode", "read-only", &prompt]);
        assert_eq!(
            session
                .expect_eof()
                .expect("unknown capability still works"),
            0
        );
        assert!(other.captured_message_count() > 0);
    }
}

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
