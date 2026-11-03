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

const USAGE_FIXTURES: &str = include_str!("../../../../spec/cache_usage_fixtures.json");
const USAGE_FIELDS: [&str; 4] = [
    "input_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
    "output_tokens",
];

fn usage_fixture(id: &str) -> Value {
    let fixtures: Value = serde_json::from_str(USAGE_FIXTURES).unwrap();
    fixtures["fixtures"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fixture| fixture["id"] == id)
        .unwrap_or_else(|| panic!("missing usage fixture {id}"))
        .clone()
}

/// The fixture's counting rule: the last snapshot of a request identity is final.
fn final_snapshots(fixture: &Value) -> Vec<&Value> {
    let identity = |s: &Value| {
        (
            s["request_id"].clone(),
            s["request_started_ms"].clone(),
            s["context_segment"].clone(),
        )
    };
    let mut finals: Vec<&Value> = Vec::new();
    for snapshot in fixture["snapshots"].as_array().unwrap() {
        match finals
            .iter()
            .position(|s| identity(s) == identity(snapshot))
        {
            Some(position) => finals[position] = snapshot,
            None => finals.push(snapshot),
        }
    }
    finals
}

fn usage_sum(snapshots: &[&Value]) -> Value {
    let mut usage = serde_json::Map::new();
    for field in USAGE_FIELDS {
        let sum: u64 = snapshots
            .iter()
            .map(|s| s["usage"][field].as_u64().unwrap())
            .sum();
        usage.insert(field.to_owned(), json!(sum));
    }
    Value::Object(usage)
}

/// Keeps the expected totals tied to the snapshots, so a fixture edit cannot
/// leave them stale.
fn assert_fixture_totals(fixture: &Value) -> Vec<&Value> {
    let expected = &fixture["expected"];
    let finals = final_snapshots(fixture);
    let snapshots = fixture["snapshots"].as_array().unwrap().len();
    assert_eq!(expected["requests"], finals.len());
    assert_eq!(
        expected["duplicate_snapshots_ignored"],
        snapshots - finals.len()
    );
    assert_eq!(expected["session_usage"], usage_sum(&finals));
    let mut partitioned = 0;
    for segment in expected["segments"].as_array().unwrap() {
        let members: Vec<&Value> = finals
            .iter()
            .copied()
            .filter(|s| s["context_segment"] == segment["context_segment"])
            .collect();
        assert_eq!(segment["requests"], members.len());
        assert_eq!(segment["usage"], usage_sum(&members));
        partitioned += members.len();
    }
    assert_eq!(partitioned, finals.len(), "segments partition the session");
    finals
}

/// Writes a fixture as the ledger and rollup that `scode cache stats` reads.
/// Every snapshot becomes a ledger row, repeats included. The rollup holds the
/// fixture's expected totals, so only the ledger rows test the report's counting.
fn write_usage_fixture(env: &TestEnv, fixture: &Value) {
    std::fs::write(
        env.config_home().join("settings.json"),
        json!({"experimental":{"cacheTtlAdvisor":true}}).to_string(),
    )
    .unwrap();
    let dir = env
        .config_home()
        .join("cache/prompt-cache")
        .join(fixture["session_id"].as_str().unwrap());
    std::fs::create_dir_all(&dir).unwrap();
    let expected = &fixture["expected"];
    let usage = &expected["session_usage"];
    std::fs::write(
        dir.join("stats.json"),
        json!({"tracked_requests":expected["requests"],"completion_cache_hits":0,
            "completion_cache_misses":0,"completion_cache_writes":0,
            "expected_invalidations":0,"unexpected_cache_breaks":0,
            "total_cache_read_input_tokens":usage["cache_read_input_tokens"],
            "total_cache_creation_input_tokens":usage["cache_creation_input_tokens"],
            "total_input_tokens":usage["input_tokens"],
            "input_tokens_observed_requests":expected["requests"],
            "last_cache_creation_input_tokens":null,"last_cache_read_input_tokens":null,
            "last_request_hash":null,"last_completion_cache_key":null,
            "last_break_reason":null,"last_cache_source":null})
        .to_string(),
    )
    .unwrap();
    let mut rows = String::new();
    for s in fixture["snapshots"].as_array().unwrap() {
        let row = json!({
            "at_unix_secs": s["response_completed_ms"].as_u64().unwrap() / 1000,
            "gateway_request_id": s["request_id"], "model": fixture["model"],
            "input_tokens": s["usage"]["input_tokens"],
            "cache_read_input_tokens": s["usage"]["cache_read_input_tokens"],
            "cache_creation_input_tokens": s["usage"]["cache_creation_input_tokens"],
            "observation": {"schema_version":1,
                "request_started_ms": s["request_started_ms"],
                "response_started_ms": s["response_started_ms"],
                "response_completed_ms": s["response_completed_ms"],
                "attempts":1, "message_starts":1, "completed":true,
                "context_segment": s["context_segment"], "request_kind": s["request_kind"],
                "route_key":"fixture-route", "settings_key":"fixture-settings",
                "requested_ttls_seconds": s["requested_ttls_seconds"],
                "upstream_ttl_seconds":null, "client_prefix_compatible":true}
        });
        rows.push_str(&row.to_string());
        rows.push('\n');
    }
    std::fs::write(dir.join("requests.jsonl"), rows).unwrap();
}

fn usage_fixture_report(id: &str) -> (Value, Value) {
    let fixture = usage_fixture(id);
    let env = TestEnv::new_mock(&format!("cache-usage-{id}"));
    write_usage_fixture(&env, &fixture);
    let report = cache_stats_report(&env);
    (fixture, report)
}

fn cache_stats_report(env: &TestEnv) -> Value {
    let mut cli = env.spawn(&["cache", "stats", "--output-format", "json"]);
    cli.resize(180, 400).unwrap();
    cli.expect("forward_comparison").unwrap();
    assert_eq!(cli.expect_eof().unwrap(), 0);
    let screen = cli.render(screen_contents);
    let first = screen.find('{').unwrap();
    let last = screen.rfind('}').unwrap();
    let report: Value = serde_json::from_str(&screen[first..=last]).unwrap();
    assert_eq!(env.captured_message_count(), 0);
    report
}

fn assert_report_counts_fixture(fixture: &Value, report: &Value) {
    let expected = &fixture["expected"];
    assert_eq!(report["sessions"], expected["sessions"]);
    assert_eq!(
        report["uncached_input_tokens"],
        expected["session_usage"]["input_tokens"]
    );
    let session = &report["per_session"][0];
    for metric in ["read_share_pct", "write_share_pct", "reuse_pct"] {
        let actual = session["cache_efficiency"][metric].as_f64().unwrap();
        let rounded = expected["cache_efficiency"][metric].as_f64().unwrap();
        assert!(
            (actual - rounded).abs() <= 0.005_000_1,
            "{metric}: report {actual}, fixture {rounded}"
        );
    }
    let intervals = &session["completion_intervals"];
    assert_eq!(intervals["rows"], expected["requests"]);
    assert_eq!(
        intervals["duplicate_rows_ignored"],
        expected["duplicate_snapshots_ignored"]
    );
    let gaps: u64 = ["under_5m", "from_5m_to_1h", "over_1h"]
        .iter()
        .map(|bucket| intervals[bucket].as_u64().unwrap())
        .sum();
    assert_eq!(
        gaps + 1,
        expected["requests"].as_u64().unwrap(),
        "intervals continue across segments"
    );
    let coverage = &session["ttl_advisor"]["coverage"];
    assert_eq!(coverage["rows"], expected["requests"]);
    assert_eq!(
        coverage["duplicate_snapshots_ignored"],
        expected["duplicate_snapshots_ignored"]
    );
    assert_eq!(coverage["legacy_or_malformed_rows"], 0);
}

#[test]
fn usage_fixture_counts_each_final_snapshot_once() {
    let (fixture, report) = usage_fixture_report("repeated_final_snapshots");
    let finals = assert_fixture_totals(&fixture);
    assert_report_counts_fixture(&fixture, &report);
    let twins: Vec<_> = finals
        .iter()
        .filter(|s| s["usage"] == finals[1]["usage"])
        .collect();
    assert_eq!(twins.len(), 2, "matching usage must not merge requests");
    assert_ne!(twins[0]["request_id"], twins[1]["request_id"]);
}

#[test]
fn growing_snapshots_replace_usage_without_merging_distinct_attempts() {
    let fixture = usage_fixture("growing_snapshots_distinct_attempts");
    let env = TestEnv::new_mock("cache-growing-attempts");
    // Verify growth before adding the reused-ID attempt. Otherwise counting an
    // extra partial snapshot and dropping that attempt can cancel each other.
    let mut growing = fixture.clone();
    growing["snapshots"].as_array_mut().unwrap().pop();
    growing["expected"] = usage_fixture("repeated_final_snapshots")["expected"].clone();
    growing["expected"]["duplicate_snapshots_ignored"] = json!(4);
    assert_fixture_totals(&growing);
    write_usage_fixture(&env, &growing);
    assert_report_counts_fixture(&growing, &cache_stats_report(&env));

    write_usage_fixture(&env, &fixture);
    let report = cache_stats_report(&env);
    let finals = assert_fixture_totals(&fixture);
    assert_report_counts_fixture(&fixture, &report);
    let attempts: Vec<_> = finals
        .iter()
        .filter(|s| s["request_id"] == "dedup-request-2")
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_ne!(
        attempts[0]["request_started_ms"],
        attempts[1]["request_started_ms"]
    );
    assert_eq!(attempts[0]["usage"], attempts[1]["usage"]);
    assert_eq!(
        attempts[0]["response_completed_ms"],
        attempts[1]["response_completed_ms"]
    );
}

#[test]
fn automatic_ttl_state_and_final_snapshots_share_one_report() {
    let fixture = usage_fixture("growing_snapshots_distinct_attempts");
    let env = TestEnv::new_mock("cache-auto-final-snapshots");
    write_usage_fixture(&env, &fixture);
    let directory = env
        .config_home()
        .join("cache/prompt-cache")
        .join(fixture["session_id"].as_str().unwrap());
    // A saved automatic boundary decision is report input, not evidence of
    // provider cache hits or savings. Its later snapshots still count once.
    std::fs::write(
        directory.join("ttl-advice.json"),
        json!({"decisions":[{
            "made_at_ms":0, "context_segment":0, "model":fixture["model"],
            "route_key":"fixture-route", "configured_ttl_seconds":300,
            "suggested_ttl_seconds":300, "reason":"insufficient_evidence",
            "training":{"rows":0,"eligible_intervals":0,"fixed_5m_usd":0.0,
                "fixed_1h_usd":0.0,"boundary_policy_usd":0.0,"transition_writes":0},
            "trigger":"boundary", "applied":true
        }]})
        .to_string(),
    )
    .unwrap();
    let report = cache_stats_report(&env);
    assert_report_counts_fixture(&fixture, &report);
    let advisor = &report["per_session"][0]["ttl_advisor"];
    assert_eq!(advisor["mode"], "automatic");
    assert_eq!(advisor["applied"], true);
}

#[test]
fn stats_keep_attempt_metadata_and_malformed_adjacency_barriers() {
    let env = TestEnv::new_mock("cache-attempt-barriers");
    let fixture = usage_fixture("repeated_final_snapshots");
    write_usage_fixture(&env, &fixture);
    let path = env
        .config_home()
        .join("cache/prompt-cache")
        .join(fixture["session_id"].as_str().unwrap())
        .join("requests.jsonl");
    let original = std::fs::read_to_string(&path).unwrap();
    let row: Value = serde_json::from_str(original.lines().next().unwrap()).unwrap();
    let mut rows = vec![row.clone()];
    for (field, value) in [
        ("attempts", json!(2)),
        ("route_key", json!("other-route")),
        ("context_segment", json!(1)),
    ] {
        let mut distinct = row.clone();
        distinct["observation"][field] = value;
        rows.push(distinct);
    }
    // Malformed data must interrupt adjacency; a later unidentified request is
    // retained, rather than guessed to be another snapshot of the earlier one.
    let mut unknown = row;
    unknown
        .as_object_mut()
        .unwrap()
        .remove("gateway_request_id");
    let mut ledger = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    ledger.push_str("\n{partial row\n");
    ledger.push_str(&unknown.to_string());
    std::fs::write(path, ledger).unwrap();
    let report = cache_stats_report(&env);
    let session = &report["per_session"][0];
    assert_eq!(session["completion_intervals"]["rows"], 5);
    assert_eq!(session["completion_intervals"]["malformed_rows"], 1);
    assert_eq!(session["completion_intervals"]["under_5m"], 3);
    assert_eq!(session["ttl_advisor"]["coverage"]["rows"], 6);
    assert_eq!(
        session["ttl_advisor"]["coverage"]["legacy_or_malformed_rows"],
        1
    );
    assert_eq!(
        session["ttl_advisor"]["coverage"]["duplicate_snapshots_ignored"],
        0
    );
}

#[test]
fn untimed_observations_preserve_known_metadata_and_exact_repeats() {
    let env = TestEnv::new_mock("cache-untimed-observations");
    let fixture = usage_fixture("repeated_final_snapshots");
    write_usage_fixture(&env, &fixture);
    let path = env
        .config_home()
        .join("cache/prompt-cache")
        .join(fixture["session_id"].as_str().unwrap())
        .join("requests.jsonl");
    let original = std::fs::read_to_string(&path).unwrap();
    let mut row: Value = serde_json::from_str(original.lines().next().unwrap()).unwrap();
    row["observation"]["request_started_ms"] = json!(0);
    let mut rows = vec![row.clone()];
    for (field, value) in [
        ("attempts", json!(2)),
        ("route_key", json!("other-route")),
        ("context_segment", json!(1)),
        ("requested_ttls_seconds", json!([3600])),
    ] {
        let mut distinct = row.clone();
        distinct["observation"][field] = value;
        rows.push(distinct);
    }
    let mut legacy = row.clone();
    legacy.as_object_mut().unwrap().remove("observation");
    rows.extend([legacy.clone(), row, legacy]);
    std::fs::write(
        path,
        rows.iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let report = cache_stats_report(&env);
    let session = &report["per_session"][0];
    assert_eq!(session["completion_intervals"]["rows"], 6);
    assert_eq!(session["completion_intervals"]["duplicate_rows_ignored"], 2);
    let advisor = &session["ttl_advisor"];
    assert_eq!(advisor["coverage"]["rows"], 6);
    assert_eq!(advisor["coverage"]["duplicate_snapshots_ignored"], 2);
    assert_eq!(advisor["coverage"]["ineligible_rows"], 6);
    assert_eq!(advisor["coverage"]["legacy_or_malformed_rows"], 1);
    assert_eq!(
        advisor["decisions"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
    assert_eq!(
        advisor["forward_comparison"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
}

#[test]
fn legacy_missing_usage_is_not_a_zero_usage_snapshot() {
    let env = TestEnv::new_mock("cache-legacy-missing-usage");
    std::fs::write(
        env.config_home().join("settings.json"),
        json!({"experimental":{"cacheTtlAdvisor":true}}).to_string(),
    )
    .unwrap();
    let directory = env.config_home().join("cache/prompt-cache/legacy-usage");
    std::fs::create_dir_all(&directory).unwrap();
    let stats = engine_core::PromptCacheStats {
        tracked_requests: 2,
        ..Default::default()
    };
    std::fs::write(
        directory.join("stats.json"),
        serde_json::to_vec(&stats).unwrap(),
    )
    .unwrap();
    let missing = json!({"gateway_request_id":"legacy-request", "model":"claude-opus-5-5",
        "at_unix_secs":1000, "input_tokens":0, "cache_read_input_tokens":0});
    let mut zero = missing.clone();
    zero["cache_creation_input_tokens"] = json!(0);
    std::fs::write(
        directory.join("requests.jsonl"),
        format!("{missing}\n{zero}\n"),
    )
    .unwrap();
    let report = cache_stats_report(&env);
    let session = &report["per_session"][0];
    assert_eq!(session["completion_intervals"]["rows"], 2);
    assert_eq!(session["completion_intervals"]["duplicate_rows_ignored"], 0);
    assert_eq!(session["ttl_advisor"]["coverage"]["rows"], 2);
    assert_eq!(
        session["ttl_advisor"]["coverage"]["legacy_or_malformed_rows"],
        2
    );
    assert_eq!(
        session["ttl_advisor"]["coverage"]["duplicate_snapshots_ignored"],
        0
    );
    assert_eq!(session["cache_efficiency"]["read_share_pct"], Value::Null);
}

#[test]
fn usage_fixture_ttl_switch_keeps_observations_explicit_and_claims_no_savings() {
    let (fixture, report) = usage_fixture_report("ttl_switch_5m_to_1h");
    let finals = assert_fixture_totals(&fixture);
    assert_report_counts_fixture(&fixture, &report);
    for group in fixture["expected"]["by_requested_ttl"].as_array().unwrap() {
        let members: Vec<&Value> = finals
            .iter()
            .copied()
            .filter(|s| s["requested_ttls_seconds"] == json!([group["requested_ttl_seconds"]]))
            .collect();
        assert_eq!(group["requests"], members.len());
        assert_eq!(group["usage"], usage_sum(&members));
    }
    let transition = &fixture["ttl_transition"];
    let switch = finals
        .iter()
        .position(|s| s["request_id"] == transition["first_request_id"])
        .unwrap();
    let requested = |s: &&Value| s["requested_ttls_seconds"].clone();
    assert!(finals[..switch]
        .iter()
        .all(|s| requested(s) == json!([transition["from_ttl_seconds"]])));
    assert!(finals[switch..]
        .iter()
        .all(|s| requested(s) == json!([transition["to_ttl_seconds"]])));
    let advisor = &report["per_session"][0]["ttl_advisor"];
    assert_eq!(advisor["mode"], "advisory_only");
    assert_eq!(advisor["applied"], false);
    assert_eq!(advisor["upstream_ttl"], "unknown");
    assert_eq!(
        advisor["comparison_kind"],
        "hypothetical_not_causal_or_billed_savings"
    );
    assert_eq!(advisor["coverage"]["ineligible_rows"], 0);
    assert_eq!(
        advisor["decisions"].as_array().unwrap().as_slice(),
        [] as [serde_json::Value; 0]
    );
    assert!(
        advisor["forward_comparison"].as_array().unwrap().is_empty(),
        "observations alone must not produce a savings figure"
    );
}

#[test]
fn usage_fixture_compaction_keeps_one_session_and_both_totals() {
    let (fixture, report) = usage_fixture_report("compaction_same_session");
    let finals = assert_fixture_totals(&fixture);
    assert_report_counts_fixture(&fixture, &report);
    for boundary in fixture["expected"]["segment_boundaries"]
        .as_array()
        .unwrap()
    {
        let at = finals
            .iter()
            .position(|s| s["request_id"] == boundary["after_request_id"])
            .unwrap();
        assert_eq!(finals[at]["request_kind"], "compaction");
        assert_eq!(finals[at]["context_segment"], boundary["from_segment"]);
        assert_eq!(finals[at + 1]["context_segment"], boundary["to_segment"]);
    }
    assert_eq!(report["per_session"].as_array().unwrap().len(), 1);
    let compactions = finals
        .iter()
        .filter(|s| s["request_kind"] == "compaction")
        .count();
    assert_eq!(
        report["per_session"][0]["ttl_advisor"]["coverage"]["ineligible_rows"], compactions,
        "a compaction request counts in the session but is not an advisor interval"
    );
}
