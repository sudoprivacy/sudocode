//! Exercise real resumed status chrome and local reports, with no model call.
mod common;
use common::TestEnv;
use runtime::{ContentBlock, ConversationMessage, Session, TokenUsage};
use serde_json::{json, Value};
use std::time::Duration;

fn screen_contents(screen: &pty_expect::Screen<'_>) -> String {
    screen.contents()
}

#[test]
fn resumed_status_distinguishes_turn_and_session_cache_share() {
    let env = TestEnv::new_mock("cache-share-resume");
    let mut saved = Session::new().with_workspace_root(env.workspace_root());
    for (read, write, input) in [(6000, 3000, 1000), (9000, 1000, 1000)] {
        saved.push_user_text("Saved question".to_owned()).unwrap();
        let mut message = ConversationMessage::assistant(vec![ContentBlock::Text {
            text: "Saved answer".to_owned(),
        }]);
        message.usage = Some(TokenUsage {
            cache_read_input_tokens: read,
            cache_creation_input_tokens: write,
            input_tokens: input,
            output_tokens: 20,
            ..TokenUsage::default()
        });
        saved.push_message(message).unwrap();
    }
    let path = env.workspace_root().join("saved.jsonl");
    saved.save_to_path(&path).unwrap();
    let mut sess = env.spawn_with_env(
        &[
            "--resume",
            path.to_str().unwrap(),
            "--permission-mode",
            "read-only",
        ],
        &[("SUDOCODE_INTERRUPT_QUEUE_MODE", "queue")],
    );
    sess.resize(40, 180).unwrap();
    common::expect_screen(
        &sess,
        |screen| screen.contains("⚡82%") && screen.contains("✎9%") && screen.contains("Σ⚡71%"),
        common::at_least(Duration::from_secs(15)),
        "turn and session use the shared prompt denominator",
    );
    sess.send("/exit").unwrap();
    common::expect_input_line(&sess, "/exit", common::DEFAULT_TIMEOUT, "type exit");
    sess.send("\r").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    assert_eq!(env.captured_message_count(), 0);
}

#[test]
fn cache_stats_json_keeps_session_intervals_and_legacy_unknowns() {
    let env = TestEnv::new_mock("cache-session-report");
    let root = env.config_home().join("cache/prompt-cache");
    let complete = root.join("session-complete");
    let legacy = root.join("session-legacy");
    std::fs::create_dir_all(&complete).unwrap();
    std::fs::create_dir_all(&legacy).unwrap();
    let stats = json!({"tracked_requests":4,"completion_cache_hits":0,
        "completion_cache_misses":0,"completion_cache_writes":0,
        "expected_invalidations":0,"unexpected_cache_breaks":0,
        "total_cache_read_input_tokens":9000,"total_cache_creation_input_tokens":1000,
        "total_input_tokens":1000,"input_tokens_observed_requests":4,
        "last_cache_creation_input_tokens":null,"last_cache_read_input_tokens":null,
        "last_request_hash":null,"last_completion_cache_key":null,
        "last_break_reason":null,"last_cache_source":null});
    std::fs::write(complete.join("stats.json"), stats.to_string()).unwrap();
    let mut old = stats.clone();
    old.as_object_mut().unwrap().remove("total_input_tokens");
    old.as_object_mut()
        .unwrap()
        .remove("input_tokens_observed_requests");
    std::fs::write(legacy.join("stats.json"), old.to_string()).unwrap();
    let mut rows = String::new();
    for (id, at) in [
        ("a", 1000),
        ("b", 1010),
        ("b", 1010),
        ("c", 1410),
        ("d", 5411),
    ] {
        rows.push_str(
            &json!({"gateway_request_id":id,"at_unix_secs":at,
            "input_tokens":100,"cache_read_input_tokens":9000,
            "cache_creation_input_tokens":1000})
            .to_string(),
        );
        rows.push('\n');
    }
    rows.push_str("{partial row\n");
    std::fs::write(complete.join("requests.jsonl"), rows).unwrap();
    let mut sess = env.spawn(&["cache", "stats", "--output-format", "json"]);
    sess.resize(100, 240).unwrap();
    sess.expect("response_completion").unwrap();
    sess.expect("unconfirmed").unwrap();
    assert_eq!(sess.expect_eof().unwrap(), 0);
    let screen = sess.render(screen_contents);
    for expected in [
        "\"duplicate_rows_ignored\": 1",
        "\"from_5m_to_1h\": 1",
        "\"over_1h\": 1",
        "\"malformed_rows\": 1",
        "\"read_share_pct\": null",
        "\"reuse_pct\": 90.0",
        "\"cache_metrics_spec_sha256\"",
    ] {
        assert!(screen.contains(expected), "missing {expected}: {screen}");
    }
    assert!(
        !screen.contains("session-complete"),
        "new exports omit session identifiers"
    );
    assert_eq!(env.captured_message_count(), 0);
}

#[test]
fn ttl_advisor_report_separates_forward_scenarios_from_training_and_bad_samples() {
    let env = TestEnv::new_mock("ttl-advisor-report");
    seed_ttl_advisor_report(&env);
    let mut cli = env.spawn(&["cache", "stats", "--output-format", "json"]);
    cli.resize(180, 400).unwrap();
    cli.expect("forward_comparison").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let screen = cli.render(screen_contents);
    let first = screen.find('{').unwrap();
    let last = screen.rfind('}').unwrap();
    let report: Value = serde_json::from_str(&screen[first..=last]).unwrap();
    let advisor = &report["per_session"][0]["ttl_advisor"];
    assert_eq!(advisor["mode"], "advisory_only");
    assert_eq!(advisor["applied"], false);
    assert_eq!(advisor["coverage"]["duplicate_snapshots_ignored"], 1);
    assert_eq!(advisor["coverage"]["legacy_or_malformed_rows"], 1);
    assert_eq!(advisor["coverage"]["ineligible_rows"], 4);
    let comparison = &advisor["forward_comparison"][0]["comparison"];
    assert_eq!(
        comparison["rows"], 9,
        "training rows must not enter forward evaluation"
    );
    assert!(
        comparison["fixed_1h_usd"].as_f64().unwrap() < comparison["fixed_5m_usd"].as_f64().unwrap()
    );
    assert_eq!(
        comparison["fixed_1h_usd"],
        comparison["boundary_policy_usd"]
    );
    for identifier in [
        "private-session-name",
        "private-route-key",
        "private-request-",
    ] {
        assert!(!advisor.to_string().contains(identifier));
    }
    assert_eq!(env.captured_message_count(), 0);
}

fn seed_ttl_advisor_report(env: &TestEnv) {
    std::fs::write(
        env.config_home().join("settings.json"),
        json!({"experimental":{"cacheTtlAdvisor":true}}).to_string(),
    )
    .unwrap();
    let dir = env
        .config_home()
        .join("cache/prompt-cache/private-session-name");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("stats.json"),
        json!({
            "tracked_requests":12,"completion_cache_hits":0,"completion_cache_misses":0,
            "completion_cache_writes":0,"expected_invalidations":0,"unexpected_cache_breaks":0,
            "total_cache_read_input_tokens":0,"total_cache_creation_input_tokens":120_000,
            "last_cache_creation_input_tokens":null,"last_cache_read_input_tokens":null,
            "last_request_hash":null,"last_completion_cache_key":null,
            "last_break_reason":null,"last_cache_source":null
        })
        .to_string(),
    )
    .unwrap();
    let scenario = json!({"rows":9,"eligible_intervals":8,"fixed_5m_usd":1.0,
        "fixed_1h_usd":0.5,"boundary_policy_usd":1.0,"transition_writes":0});
    std::fs::write(
        dir.join("ttl-advice.json"),
        json!({"decisions":[{
            "made_at_ms":1_000_000,"context_segment":0,"model":"claude-opus-5-5",
            "route_key":"private-route-key","configured_ttl_seconds":300,
            "suggested_ttl_seconds":3600,"reason":"hypothetical_1h_saving","training":scenario
        }]})
        .to_string(),
    )
    .unwrap();
    let mut rows = Vec::new();
    for index in 0..10u64 {
        let at = 500_000 + index * 600_000;
        rows.push(json!({"model":"claude-opus-5-5", "gateway_request_id":format!("private-request-{index}"),
            "at_unix_secs":at/1000,"input_tokens":0,
            "cache_creation_input_tokens":100_000,"cache_read_input_tokens":0,
            "observation":{"schema_version":1,"request_started_ms":at,
                "response_started_ms":at+1,"response_completed_ms":at+2,
                "attempts":1,"message_starts":1,"completed":true,"context_segment":0,
                "request_kind":"session_request","route_key":"private-route-key","settings_key":"s",
                "requested_ttls_seconds":[300],"upstream_ttl_seconds":null,"client_prefix_compatible":true}}));
    }
    rows.push(rows.last().unwrap().clone());
    for field in ["completed", "attempts", "requested_ttls_seconds"] {
        let mut bad = rows.last().unwrap().clone();
        bad["gateway_request_id"] = json!(format!("bad-{field}"));
        bad["observation"][field] = match field {
            "completed" => json!(false),
            "attempts" => json!(2),
            _ => json!([300, 3600]),
        };
        rows.push(bad);
    }
    rows.push(json!({"at_unix_secs":1,"model":"claude-opus-5-5"}));
    std::fs::write(
        dir.join("requests.jsonl"),
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
}
