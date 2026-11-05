//! Real HTTP provider continuation after a checkpoint admits part of a batch.
use mock_anthropic_service::MockAnthropicService;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
};

fn calls() -> Vec<Value> {
    ["one", "two", "three"]
        .into_iter()
        .map(|id| json!({"id":id,"name":"Bash","input":{"command":format!("echo {id}"),"run_in_background":true}}))
        .collect()
}

fn prompt(calls: &[Value]) -> Value {
    json!({"role":"user","content":[{"type":"text","text":format!("PARITY_SCENARIO:tool_concurrency TOOL_BATCH:{}",json!(calls))}]})
}

fn exchange(calls: &[Value]) -> [Value; 2] {
    [
        json!({"role":"assistant","content":calls.iter().map(|call|json!({"type":"tool_use","id":call["id"],"name":call["name"],"input":call["input"]})).collect::<Vec<_>>()}),
        json!({"role":"user","content":calls.iter().map(|call|json!({"type":"tool_result","tool_use_id":call["id"],"content":[{"type":"text","text":"background task started"}],"is_error":false})).collect::<Vec<_>>()}),
    ]
}

fn notification() -> Value {
    json!({"role":"user","content":[{"type":"text","text":"<task-notification><status>completed</status><result>one finished</result></task-notification>"}]})
}

async fn request(service: &MockAnthropicService, messages: &[Value], stream: bool) -> Vec<Value> {
    let body =
        json!({"model":"claude-sonnet-4-6","max_tokens":256,"stream":stream,"messages":messages})
            .to_string();
    let mut socket = TcpStream::connect(service.base_url().trim_start_matches("http://"))
        .await
        .unwrap();
    let wire = format!("POST /v1/messages HTTP/1.1\r\nHost: fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    socket.write_all(wire.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), socket.read_to_end(&mut response))
        .await
        .expect("provider response must not wait on a completed tool's stream gate")
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    if stream {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    } else {
        vec![serde_json::from_str(body).unwrap()]
    }
}

fn tool_ids(response: &[Value], stream: bool) -> Vec<String> {
    let blocks = if stream {
        response
            .iter()
            .filter_map(|event| event.get("content_block"))
            .collect::<Vec<_>>()
    } else {
        response[0]["content"].as_array().unwrap().iter().collect()
    };
    blocks
        .into_iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| block["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn partial_batch_continues_without_replaying_completed_tools() {
    let service = MockAnthropicService::spawn().await.unwrap();
    let calls = calls();
    for stream in [true, false] {
        let mut messages = vec![prompt(&calls)];
        assert_eq!(
            tool_ids(&request(&service, &messages, stream).await, stream),
            ["one", "two", "three"]
        );
        messages.extend(exchange(&calls[..2]));
        messages.push(notification());
        assert_eq!(
            tool_ids(&request(&service, &messages, stream).await, stream),
            ["three"],
            "a completion notification must advance the remaining batch"
        );
        messages.extend(exchange(&calls[2..]));
        messages.push(notification());
        assert_eq!(
            tool_ids(&request(&service, &messages, stream).await, stream),
            Vec::<String>::new()
        );
        messages.push(prompt(&calls));
        assert_eq!(
            tool_ids(&request(&service, &messages, stream).await, stream),
            ["one", "two", "three"],
            "a fresh explicit batch cannot inherit the previous turn's results"
        );
    }
    assert_eq!(service.captured_requests().await.len(), 8);
}

#[tokio::test]
async fn completed_stream_gate_does_not_block_remaining_calls() {
    let service = MockAnthropicService::spawn().await.unwrap();
    let mut calls = calls();
    let gate =
        std::env::temp_dir().join(format!("completed-mock-stream-gate-{}", std::process::id()));
    assert!(!gate.exists(), "the completed stream's gate must be absent");
    calls[0]["stream_wait_for"] = json!(gate);
    let mut messages = vec![prompt(&calls)];
    messages.extend(exchange(&calls[..2]));
    messages.push(notification());
    assert_eq!(
        tool_ids(&request(&service, &messages, true).await, true),
        ["three"]
    );
}
