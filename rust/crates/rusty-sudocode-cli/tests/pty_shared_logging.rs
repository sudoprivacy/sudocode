//! Independent CLI processes retain one coherent trace through log rotation.
mod common;

use common::TestEnv;
use pty_expect::PtySession;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;
use telemetry::{AnalyticsEvent, SudoclawLogSink, TelemetryEvent, TelemetrySink};

fn turn(env: &TestEnv, cli: &mut PtySession, marker: &str) {
    common::expect_input_line_cleared(cli, env.timeout(), "runtime accepts input");
    let prompt = env.prompt(
        &format!("{marker} Calculate 2+2 and reply with 'The answer is N'."),
        "single_turn_text",
    );
    cli.send(&prompt).unwrap();
    common::expect_input_line(cli, marker, env.timeout(), "prompt entered");
    cli.send("\r").unwrap();
    cli.expect("The answer is 4").expect("actual model answer");
    common::expect_input_line_cleared(cli, env.timeout(), "completed turn");
}

fn events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("shared trace exists")
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "trace {} line {} is not JSON: {error}",
                    path.display(),
                    index + 1
                )
            })
        })
        .collect()
}

fn has_request(events: &[Value], marker: &str) -> bool {
    events.iter().any(|event| {
        event["event"] == "request_debug"
            && event["attributes"]["body"].to_string().contains(marker)
    })
}

#[test]
fn independent_cli_keeps_writing_the_active_trace_after_peer_rotation() {
    let first = TestEnv::new_mock("shared-log-retained-writer");
    let second = TestEnv::new_mock("shared-log-rotator");
    let path = first.workspace_root().join("shared-trace.jsonl");
    let variables = [("SCODE_LOG_PATH", path.to_str().unwrap())];
    let mut retained = first.spawn_with_env(&["--permission-mode", "read-only"], &variables);
    retained.expect("❯").expect("first runtime ready");
    turn(&first, &mut retained, "TRACE_BEFORE_PEER_ROTATION");

    // Existing log volume, written through the public sink, makes the next
    // process rotate. No timing guess establishes the retained open writer.
    let padding = SudoclawLogSink::with_path(&path).unwrap();
    padding.record(TelemetryEvent::Analytics(
        AnalyticsEvent::new("logging-fixture", "existing-volume")
            .with_property("padding", "x".repeat(11 * 1024 * 1024).into()),
    ));
    drop(padding);

    let prompt = second.prompt(
        "TRACE_PEER_ROTATES Calculate 2+2 and reply with 'The answer is N'.",
        "single_turn_text",
    );
    let mut rotating = second.spawn_with_env(
        &["-p", &prompt, "--permission-mode", "read-only"],
        &variables,
    );
    rotating
        .expect("The answer is 4")
        .expect("peer model answer");
    assert_eq!(rotating.expect_eof().expect("peer finished"), 0);
    let rotated = path.with_file_name("shared-trace.jsonl.1");
    assert!(rotated.is_file(), "peer must have completed log rotation");

    turn(&first, &mut retained, "TRACE_AFTER_PEER_ROTATION");
    retained.send("/exit").unwrap();
    common::expect_input_line(&retained, "/exit", first.timeout(), "exit entered");
    retained.send("\r").unwrap();
    assert_eq!(retained.expect_eof().expect("retained runtime finished"), 0);
    assert_eq!(first.captured_message_count(), 2);
    assert_eq!(second.captured_message_count(), 1);

    let current = events(&path);
    let previous = events(&rotated);
    assert!(has_request(&previous, "TRACE_BEFORE_PEER_ROTATION"));
    assert!(has_request(&current, "TRACE_PEER_ROTATES"));
    assert!(
        has_request(&current, "TRACE_AFTER_PEER_ROTATION"),
        "a runtime opened before peer rotation must append its later request to the active trace"
    );
    assert!(
        !has_request(&previous, "TRACE_AFTER_PEER_ROTATION"),
        "later requests must not be appended behind the rotation boundary"
    );
    for event in current.iter().chain(&previous) {
        if event["event"] != "request_debug" {
            continue;
        }
        let request_id = &event["attributes"]["request_id"];
        for kind in ["response_debug", "response_usage"] {
            assert!(
                current.iter().chain(&previous).any(|candidate| {
                    candidate["event"] == kind
                        && candidate["attributes"]["request_id"] == *request_id
                }),
                "every completed model request must retain its {kind} record"
            );
        }
    }

    verify_ci_collection(first.workspace_root(), &path);
}

fn verify_ci_collection(workspace: &Path, path: &Path) {
    // Exercise the same CI entry point after the real writers have drained.
    // Evidence must survive a refusal, without copying malformed body bytes.
    let collector = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join(".github/scripts/summarize_live_model_trace.cjs");
    let evidence = workspace.join("evidence.json");
    let collected = Command::new("node")
        .args([
            collector.as_os_str(),
            path.as_os_str(),
            evidence.as_os_str(),
        ])
        .output()
        .expect("run the actual CI evidence collector");
    assert!(collected.status.success(), "healthy trace must be accepted");

    let corrupt = workspace.join("corrupt-trace.jsonl");
    let secret = "CREDENTIAL_REDACTION_PROBE_8cf065e2";
    let mut bytes = fs::read(path).unwrap();
    bytes.extend_from_slice(format!("{{\"private\":\"{secret}\"}}{{}}\n").as_bytes());
    fs::write(&corrupt, bytes).unwrap();
    let refused = Command::new("node")
        .args([
            collector.as_os_str(),
            corrupt.as_os_str(),
            evidence.as_os_str(),
        ])
        .env("PROXY_AUTH_TOKEN", secret)
        .output()
        .expect("collect a malformed trace");
    assert_eq!(refused.status.code(), Some(1), "corruption must fail CI");
    let serialized = fs::read_to_string(evidence).expect("failed evidence is retained");
    assert!(
        !serialized.contains(secret),
        "malformed bytes must stay private"
    );
    let rejected: Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(rejected["malformedLines"], 1);
    assert_eq!(rejected["malformedRecords"].as_array().unwrap().len(), 1);
    assert_eq!(
        rejected["malformedRecords"][0]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
}
