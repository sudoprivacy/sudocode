use std::path::PathBuf;
use telemetry::{SudoclawLogSink, TelemetryEvent, TelemetrySink};

fn temp_log_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "sudoclaw-integration-{}-{}.log",
        name,
        std::process::id()
    ))
}

#[test]
fn sink_creates_log_file_and_directory() {
    let path = temp_log_path("create-dir");
    let nested = path.join("nested").join("dir").join("test.log");

    let _sink = SudoclawLogSink::with_path(&nested).expect("should create nested path");

    assert!(nested.exists());
    let _ = std::fs::remove_file(nested);
    let _ = std::fs::remove_dir(path.join("nested").join("dir"));
    let _ = std::fs::remove_dir(path.join("nested"));
}

#[test]
fn sink_appends_to_existing_file() {
    let path = temp_log_path("append");

    // First write
    let sink1 = SudoclawLogSink::with_path(&path).expect("sink1 should work");
    sink1.record(TelemetryEvent::SessionStarted {
        session_id: "session-1".to_string(),
        timestamp_ms: 1000,
        version: "0.1.0".to_string(),
        cwd: "/test".to_string(),
        mode: "standalone".to_string(),
        model: "claude-sonnet".to_string(),
    });

    // Second write (append)
    let sink2 = SudoclawLogSink::with_path(&path).expect("sink2 should work");
    sink2.record(TelemetryEvent::SessionStarted {
        session_id: "session-2".to_string(),
        timestamp_ms: 2000,
        version: "0.1.0".to_string(),
        cwd: "/test".to_string(),
        mode: "standalone".to_string(),
        model: "claude-sonnet".to_string(),
    });

    let contents = std::fs::read_to_string(&path).expect("should read log");
    assert!(contents.contains("session-1"));
    assert!(contents.contains("session-2"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn multiple_events_in_sequence() {
    let path = temp_log_path("sequence");

    let sink = SudoclawLogSink::with_path(&path).expect("sink should work");

    // Simulate a complete session
    sink.record(TelemetryEvent::SessionStarted {
        session_id: "trace-test".to_string(),
        timestamp_ms: 1000,
        version: "0.1.5".to_string(),
        cwd: "/workspace".to_string(),
        mode: "standalone".to_string(),
        model: "claude-sonnet-4-6".to_string(),
    });

    sink.record(TelemetryEvent::HttpRequestStarted {
        session_id: "trace-test".to_string(),
        request_id: "req_test-001".to_string(),
        attempt: 1,
        method: "POST".to_string(),
        path: "/v1/messages".to_string(),
        timestamp_ms: 1500,
        attributes: serde_json::Map::new(),
    });

    sink.record(TelemetryEvent::HttpRequestSucceeded {
        session_id: "trace-test".to_string(),
        request_id: "req_test-001".to_string(),
        attempt: 1,
        method: "POST".to_string(),
        path: "/v1/messages".to_string(),
        status: 200,
        start_timestamp_ms: 1500,
        end_timestamp_ms: 2000,
        duration_ms: 500,
        provider_request_id: Some("req-123".to_string()),
        attributes: serde_json::Map::new(),
    });

    sink.record(TelemetryEvent::HttpResponseUsage {
        session_id: "trace-test".to_string(),
        request_id: "req_test-001".to_string(),
        timestamp_ms: 2000,
        input_tokens: 500,
        output_tokens: 200,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 100,
        cost_units: Some(43_700),
        cost_currency: Some("sudo_point".to_string()),
    });

    sink.record(TelemetryEvent::SessionEnded {
        session_id: "trace-test".to_string(),
        timestamp_ms: 3000,
        total_turns: 1,
        total_input_tokens: 500,
        total_output_tokens: 200,
        duration_ms: 2000,
    });

    let contents = std::fs::read_to_string(&path).expect("should read log");
    let lines: Vec<&str> = contents.lines().collect();

    assert_eq!(lines.len(), 5);

    // Verify each line is valid JSON with correct session_id
    for line in &lines {
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("each line should be JSON");
        assert_eq!(parsed["session_id"], "trace-test");
        assert_eq!(parsed["component"], "scode");
    }

    // Verify event order
    let events: Vec<String> = lines
        .iter()
        .map(|l| {
            let parsed: serde_json::Value = serde_json::from_str(l).unwrap();
            parsed["event"].as_str().unwrap().to_string()
        })
        .collect();

    assert_eq!(
        events,
        vec![
            "session_started",
            "request_started",
            "request_succeeded",
            "response_usage",
            "session_ended"
        ]
    );
    let response_usage: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
    assert_eq!(
        response_usage["attributes"]["cost_units"],
        serde_json::json!(43_700)
    );
    assert_eq!(
        response_usage["attributes"]["cost_currency"],
        serde_json::json!("sudo_point")
    );

    let _ = std::fs::remove_file(path);
}
