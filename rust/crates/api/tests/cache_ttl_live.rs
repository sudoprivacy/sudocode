//! The prompt-cache TTL rules `engine_core::cache_ttl` schedules switches by,
//! measured against the real API through the real client. Opt-in and billed:
//!
//! `ANTHROPIC_BASE_URL=<gateway> ANTHROPIC_API_KEY=<key> [SCODE_LIVE_MODEL=claude-sonnet-4-6]`,
//! then `cargo test -p api --test cache_ttl_live -- --ignored --nocapture`.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use api::{AnthropicClient, CacheHints, InputContentBlock, InputMessage, MessageRequest};

/// Long enough to be cached, and unique per run so no earlier entry matches.
fn system(tag: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let rules: Vec<String> = (1..=420)
        .map(|i| {
            format!(
                "Rule {i} of the fixture: the quick brown fox number {i} jumps over lazy dog {}.",
                i * 7
            )
        })
        .collect();
    format!("nonce {tag}-{nonce}\n{}", rules.join("\n"))
}

fn message(role: &str, text: String) -> InputMessage {
    InputMessage {
        role: role.into(),
        content: vec![InputContentBlock::Text { text }],
    }
}

/// One turn, or two: the second appends content the way a real turn does.
fn request(system: &str, turns: usize) -> MessageRequest {
    let mut messages = vec![message("user", "First question: reply OK.".into())];
    if turns >= 2 {
        let output: Vec<String> = (1..=160)
            .map(|i| format!("Tool output line {i}: appended turn content {}.", i * 3))
            .collect();
        messages.push(message("assistant", "OK.".into()));
        messages.push(message(
            "user",
            format!("Second turn:\n{}\nReply OK.", output.join("\n")),
        ));
    }
    MessageRequest {
        model: std::env::var("SCODE_LIVE_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
        max_tokens: 8,
        messages,
        cache_hints: Some(CacheHints {
            system_static: None,
            system_dynamic: Some(system.to_owned()),
            breakpoint_last_message: true,
        }),
        ..MessageRequest::default()
    }
}

/// `(cache_read_input_tokens, cache_creation_input_tokens)`.
async fn send(client: &AnthropicClient, request: &MessageRequest) -> (u32, u32) {
    let usage = client
        .send_message(request, None)
        .await
        .expect("live request")
        .usage;
    // A cache entry is readable once its response has started; leave margin.
    tokio::time::sleep(Duration::from_secs(3)).await;
    (
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
    )
}

#[tokio::test]
#[ignore = "real API and billed; see the module docs"]
async fn a_ttl_switch_keeps_the_cache_except_5m_to_1h_on_a_growing_request() {
    let mut client =
        AnthropicClient::new(std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY"));
    if let Ok(base_url) = std::env::var("ANTHROPIC_BASE_URL") {
        client = client.with_base_url(base_url);
    }
    let client = client.with_cache_ttl_1h(false);
    let ttl = client.cache_ttl().clone();

    // A switch on unchanged content keeps the cached prefix, either way.
    let same = system("same");
    let (_, written) = send(&client, &request(&same, 1)).await;
    assert!(
        written > 1024,
        "the fixture must be cached: wrote {written}"
    );
    ttl.set_one_hour(true);
    let (read, _) = send(&client, &request(&same, 1)).await;
    assert_eq!(
        read, written,
        "5m to 1h on unchanged content keeps the prefix"
    );
    ttl.set_one_hour(false);
    let (read, _) = send(&client, &request(&same, 1)).await;
    assert_eq!(
        read, written,
        "1h to 5m on unchanged content keeps the prefix"
    );

    // 1h to 5m on a request that grows keeps reading the 1h prefix.
    let down = system("down");
    ttl.set_one_hour(true);
    let (_, written) = send(&client, &request(&down, 1)).await;
    ttl.set_one_hour(false);
    let (read, _) = send(&client, &request(&down, 2)).await;
    assert_eq!(
        read, written,
        "1h to 5m on a growing request keeps the prefix"
    );

    // 5m to 1h on a request that grows rewrites the whole prefix: the cost the
    // engine defers to a cold point.
    let up = system("up");
    ttl.set_one_hour(false);
    let (_, written) = send(&client, &request(&up, 1)).await;
    ttl.set_one_hour(true);
    let (read, created) = send(&client, &request(&up, 2)).await;
    assert_eq!(read, 0, "5m to 1h on a growing request reads nothing");
    assert!(
        created > written,
        "it rewrites the whole prefix: {created} vs {written}"
    );

    // The control: the same growth without a switch reads the prefix.
    let control = system("control");
    ttl.set_one_hour(false);
    let (_, written) = send(&client, &request(&control, 1)).await;
    let (read, _) = send(&client, &request(&control, 2)).await;
    assert_eq!(
        read, written,
        "without a switch the growing request reads the prefix"
    );
}
