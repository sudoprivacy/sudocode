//! What the prompt-cache record says about a prefix that was thrown away.
//!
//! The record exists to answer one question — "did we just discard a cached
//! prefix, and what did it?" — and the two ways it used to get that wrong are
//! pinned here. Both came out of live sessions against the pool, and neither is
//! reachable from the PTY layer: only the Anthropic provider writes these
//! records, and the mock backend never becomes one.
//!
//! ```bash
//! cargo test -p api --test prompt_cache_breaks
//! ```

use std::sync::{Mutex as StdMutex, OnceLock};

use api::{
    cache_break_cause, InputContentBlock, InputMessage, MessageRequest, PromptCache,
    ToolDefinition, Usage,
};
use serde_json::json;

/// Runs the public JSON scenarios through the real ledger and persisted rows.
/// The same contract also drives the endpoint probe's HTTP-backed scenarios.
#[test]
fn public_cache_contract_preserves_thinking_history_and_attributes_changes() {
    let _guard = env_lock();
    let _home = TempHome::new("public-contract");
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../spec/prompt_cache_observations.json"
    ))
    .expect("public contract must parse");
    assert_public_cache_defaults(&contract);
    for scenario in contract["ledger_scenarios"].as_array().unwrap() {
        let label = scenario["id"].as_str().unwrap();
        let cache = PromptCache::new(label);
        let mut before = request_with_tools(vec![cron_list(true)]);
        before.messages = serde_json::from_value(contract["history_fixture"].clone()).unwrap();
        let mut after = before.clone();
        match scenario["change"].as_str().unwrap() {
            "none" => {}
            "model" => after.model = "another-model".to_string(),
            "system" => after.system = Some("changed system".to_string()),
            "tools" => after.tools.as_mut().unwrap()[0].defer_loading = false,
            "append" => after.messages.push(InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text { text: "next turn".to_string() }],
            }),
            "append_tool_result" => after.messages.push(serde_json::from_value(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_fixture", "content": [{"type": "text", "text": "fixture contents"}]}]
            })).unwrap()),
            "thinking" | "signature" => {
                let InputContentBlock::Thinking { thinking, signature } = &mut after.messages[1].content[0] else {
                    panic!("signed thinking fixture is required");
                };
                if scenario["change"] == "thinking" {
                    thinking.push_str(" changed");
                } else {
                    *signature = Some("changed-synthetic-signature".to_string());
                }
            }
            "remove_thinking" => { after.messages[1].content.remove(0); }
            other => panic!("unknown public scenario {other}"),
        }
        let original_history = serde_json::to_value(&before.messages).unwrap();
        let _ = cache.record_usage(
            &before,
            &reads(
                scenario["previous_read"]
                    .as_u64()
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            api::ResponseTraceIds::default(),
        );
        let record = cache.record_usage(
            &after,
            &reads(
                scenario["current_read"]
                    .as_u64()
                    .unwrap()
                    .try_into()
                    .unwrap(),
            ),
            api::ResponseTraceIds {
                gateway: Some("gateway-correlation-fixture".to_string()),
                provider: Some("provider-request-fixture".to_string()),
            },
        );
        let causes = record
            .cache_break
            .map_or_else(Vec::new, |event| event.causes);
        assert_eq!(
            serde_json::to_value(causes).unwrap(),
            scenario["expected_causes"],
            "{label}"
        );
        assert_eq!(
            serde_json::to_value(&before.messages).unwrap(),
            original_history,
            "tracking must preserve the original signed history: {label}"
        );
        let ledger = std::fs::read_to_string(cache.paths().requests_path).unwrap();
        let row: serde_json::Value = serde_json::from_str(ledger.lines().last().unwrap()).unwrap();
        for field in contract["fields"].as_object().unwrap().keys() {
            assert!(
                row.get(field).is_some(),
                "shared field {field} missing from {label}"
            );
        }
        assert_eq!(row["gateway_request_id"], "gateway-correlation-fixture");
        assert_eq!(row["provider_request_id"], "provider-request-fixture");
    }
}

/// `record_usage` resolves its paths from `SUDO_CODE_CONFIG_HOME` at call time,
/// so tests that set it cannot overlap.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TempHome {
    path: std::path::PathBuf,
}

impl TempHome {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "api-prompt-cache-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should be after the epoch")
                .as_nanos()
        ));
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &path);
        Self { path }
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
    }
}

fn request_with_tools(tools: Vec<ToolDefinition>) -> MessageRequest {
    MessageRequest {
        model: "claude-3-7-sonnet-latest".to_string(),
        max_tokens: 64,
        messages: vec![InputMessage {
            role: "user".to_string(),
            content: vec![InputContentBlock::Text {
                text: "unchanged across both turns".to_string(),
            }],
        }],
        system: Some("system".to_string()),
        tools: Some(tools),
        ..Default::default()
    }
}

/// The one tool whose `defer_loading` flips when `ToolSearch` reveals it.
fn cron_list(defer_loading: bool) -> ToolDefinition {
    ToolDefinition {
        name: "CronList".to_string(),
        description: None,
        input_schema: json!({"type": "object"}),
        defer_loading,
    }
}

fn reads(cache_read_input_tokens: u32) -> Usage {
    Usage {
        cache_read_input_tokens,
        ..Usage::default()
    }
}

/// Revealing a deferred tool mid-session is the prefix rewrite this record was
/// least able to report.
///
/// `defer_loading` is serialised when true and omitted when false, so
/// discovering a tool changes the `tools` bytes — and `tools` sits ahead of
/// system and messages in the prompt, so the provider rebuilds the *entire*
/// prefix, not just the tools block.
///
/// The break was always detected. But it was filed under `expected` and only
/// `last_break_reason` survived into the session rollup, so a session that did
/// this ten times was indistinguishable from one that never did. What is
/// asserted here is the cause reaching `stats.json`, because that — not the
/// per-request ledger — is what `scode cache stats` aggregates.
#[test]
fn revealing_a_deferred_tool_is_attributed_to_tools() {
    let _guard = env_lock();
    let _home = TempHome::new("tools-cause");
    let cache = PromptCache::new("tools-cause-session");

    let _ = cache.record_usage(
        &request_with_tools(vec![cron_list(true)]),
        &reads(300_000),
        api::ResponseTraceIds::default(),
    );
    let record = cache.record_usage(
        &request_with_tools(vec![cron_list(false)]),
        &reads(0),
        api::ResponseTraceIds::default(),
    );

    let event = record
        .cache_break
        .expect("a 300k read collapse is a break by any measure");
    assert_eq!(
        event.causes,
        vec![cache_break_cause::TOOLS.to_string()],
        "only the tools array changed, so nothing else may be blamed"
    );
    assert_eq!(event.reason, "tool definitions changed");
    assert_eq!(
        event.token_drop, 300_000,
        "the whole prefix went cold, not just the tools block"
    );
    assert_eq!(
        record.stats.breaks_by_cause.get(cache_break_cause::TOOLS),
        Some(&1),
        "the cause has to be countable across sessions, got {:?}",
        record.stats.breaks_by_cause
    );
}

/// A prefix discarded before it was ever read has no drop to show.
///
/// These are the numbers from a real three-turn session: turn 1 wrote a
/// 7926-token prefix and read none of it, turn 2 revealed `CronList` through
/// `ToolSearch` and so rewrote the whole thing (read 0, written 8421).
///
/// Detection used to gate on a drop in cache reads *before* comparing
/// fingerprints, so reads of 0 -> 0 meant the only break in the session was
/// recorded as no break at all: `expected_invalidations: 0`,
/// `breaks_by_cause: {}`. The most expensive break in the session was the one
/// shape the instrument could not see.
#[test]
fn a_prefix_discarded_before_it_was_read_is_still_a_break() {
    let _guard = env_lock();
    let _home = TempHome::new("zero-drop");
    let cache = PromptCache::new("zero-drop-session");

    let _ = cache.record_usage(
        &request_with_tools(vec![cron_list(true)]),
        &Usage {
            cache_creation_input_tokens: 7_926,
            cache_read_input_tokens: 0,
            ..Usage::default()
        },
        api::ResponseTraceIds::default(),
    );
    let record = cache.record_usage(
        &request_with_tools(vec![cron_list(false)]),
        &Usage {
            cache_creation_input_tokens: 8_421,
            cache_read_input_tokens: 0,
            ..Usage::default()
        },
        api::ResponseTraceIds::default(),
    );

    let event = record
        .cache_break
        .expect("a rewritten tools array is a break even with no read to lose");
    assert_eq!(event.causes, vec![cache_break_cause::TOOLS.to_string()]);
    assert_eq!(
        event.token_drop, 0,
        "the drop is zero here — which is exactly why it cannot be the gate"
    );
}

/// The other half of the same rule: with the fingerprint steady there is no
/// evidence but the token counts, so a small wobble has to stay quiet.
///
/// Without this, moving the drop gate would have turned every request into a
/// break and the breakdown would name nothing at all.
#[test]
fn a_steady_fingerprint_with_no_drop_is_not_a_break() {
    let _guard = env_lock();
    let _home = TempHome::new("steady");
    let cache = PromptCache::new("steady-session");

    let request = request_with_tools(vec![cron_list(true)]);
    let _ = cache.record_usage(&request, &reads(8_000), api::ResponseTraceIds::default());
    let record = cache.record_usage(&request, &reads(8_000), api::ResponseTraceIds::default());

    assert!(
        record.cache_break.is_none(),
        "nothing changed and nothing was lost; this must not be reported"
    );
    assert!(
        record.stats.breaks_by_cause.is_empty(),
        "got {:?}",
        record.stats.breaks_by_cause
    );
}

fn assert_public_cache_defaults(contract: &serde_json::Value) {
    let defaults = api::PromptCacheConfig::default();
    assert_eq!(
        defaults.prompt_ttl.as_secs(),
        contract["defaults"]["prompt_ttl_seconds"]
    );
    assert_eq!(
        defaults.completion_ttl.as_secs(),
        contract["defaults"]["completion_ttl_seconds"]
    );
    assert_eq!(
        u64::from(defaults.cache_break_min_drop),
        contract["defaults"]["cache_break_min_drop"]
    );
}
