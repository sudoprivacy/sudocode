//! Exercise non-streaming Chat Completions through the real HTTP client.
use api::{
    InputMessage, MessageRequest, OpenAiCompatClient, OpenAiCompatConfig, OutputContentBlock,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[tokio::test]
async fn non_streaming_reasoning_fields_preserve_thinking_answer_and_usage() {
    for (case, fields, expected) in [
        ("current", json!({"reasoning":"思考"}), Some("思考")),
        ("legacy", json!({"reasoning_content":"思考"}), Some("思考")),
        (
            "both",
            json!({"reasoning":"思考","reasoning_content":"do not duplicate"}),
            Some("思考"),
        ),
        (
            "null-current",
            json!({"reasoning":null,"reasoning_content":"思考"}),
            Some("思考"),
        ),
        (
            "empty-current",
            json!({"reasoning":"","reasoning_content":"思考"}),
            Some("思考"),
        ),
        (
            "null-legacy",
            json!({"reasoning":"思考","reasoning_content":null}),
            Some("思考"),
        ),
        ("absent", json!({}), None),
        ("empty", json!({"reasoning":""}), None),
    ] {
        let mut message = fields;
        message["role"] = json!("assistant");
        message["content"] = json!("answer");
        let body = json!({
            "id":"reasoning-http","model":"glm-fixture",
            "choices":[{"message":message,"finish_reason":"stop"}],
            "usage":{"prompt_tokens":17,"completion_tokens":9,"total_tokens":26},
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            assert!(line.starts_with("POST /v1/chat/completions "));
            let mut length = 0;
            loop {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut request = vec![0; length];
            socket.read_exact(&mut request).await.unwrap();
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["stream"], false);
            assert_eq!(request["model"], "glm-fixture");
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            socket
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });
        let client =
            OpenAiCompatClient::new("fixture-key", OpenAiCompatConfig::openai()).with_base_url(url);
        let response = client
            .send_message(
                &MessageRequest {
                    model: "glm-fixture".into(),
                    max_tokens: 256,
                    messages: vec![InputMessage::user_text("explain the answer")],
                    ..MessageRequest::default()
                },
                None,
            )
            .await
            .unwrap_or_else(|error| panic!("{case}: {error}"));
        server.await.unwrap();
        let mut blocks = Vec::new();
        if let Some(thinking) = expected {
            blocks.push(OutputContentBlock::Thinking {
                thinking: thinking.into(),
                signature: None,
            });
        }
        blocks.push(OutputContentBlock::Text {
            text: "answer".into(),
        });
        assert_eq!(response.content, blocks, "{case}: normalized blocks");
        assert_eq!(response.usage.input_tokens, 17, "{case}: input usage");
        assert_eq!(response.usage.output_tokens, 9, "{case}: output usage");
        assert_eq!(response.stop_reason.as_deref(), Some("end_turn"));
    }
}
