//! Exercise the actual ACP process and provider request bodies on both transports.
use super::*;

const METHOD: &str = "_sudocode/session/host_context";
const A: &str = "Reviewed business revision A: revenue uses order date.";
const B: &str = "Reviewed business revision B: revenue uses shipment date.";

async fn context_request(client: &mut AcpTestClient, session: &str) -> Value {
    let (_, request) = client
        .recv_until(RECV_TIMEOUT, |value| value["method"] == METHOD)
        .await
        .expect("engine must resolve context before model work");
    assert_eq!(request["params"]["sessionId"], session);
    assert!(request["id"].is_string() || request["id"].is_number());
    let (_, status) = client
        .send_request("_sudocode/session/status", json!({"sessionId": session}))
        .await;
    assert_eq!(
        status["result"]["foreground"],
        json!({"state": "running"}),
        "host context waits remain queryable work, not user input: {status}"
    );
    request
}

async fn answer_context(client: &mut AcpTestClient, request: Value, text: &str) {
    client
        .send_raw(&json!({
            "jsonrpc": "2.0", "id": request["id"],
            "result": { "appendSystemPrompt": text },
        }))
        .await;
}

fn submission(session: &str, id: &str) -> Value {
    json!({
        "sessionId": session, "action": "submit", "clientId": id, "delivery": "queue",
        "content": [{"type": "text", "text": format!("{SCENARIO_PREFIX}streaming_text {id}")}],
    })
}

async fn input_finished(client: &mut AcpTestClient, kind: &str) {
    client
        .recv_until(RECV_TIMEOUT, |value| {
            value["method"] == "_sudocode/session/input_update"
                && value["params"]["update"]["kind"] == kind
        })
        .await
        .expect("input must reach its terminal outcome");
}

async fn verify_queued_context_refresh(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    session: &str,
) {
    client
        .send_request_no_wait("_sudocode/session/input", submission(session, "first-turn"))
        .await;
    let first_context = context_request(client, session).await;
    let (_, queued) = client
        .send_request(
            "_sudocode/session/input",
            submission(session, "queued-turn"),
        )
        .await;
    assert!(queued.get("error").is_none(), "{queued}");
    assert!(
        server.captured_requests().await.is_empty(),
        "no model request before host resolution"
    );
    answer_context(client, first_context, A).await;

    let second_context = context_request(client, session).await;
    let first = server.captured_requests().await;
    assert_eq!(
        first.len(),
        1,
        "queued input waits for its own current context"
    );
    let body: Value = serde_json::from_str(&first[0].raw_body).unwrap();
    assert!(body["system"].to_string().contains(A));
    assert!(!body["system"]
        .to_string()
        .contains("INITIAL_CONTEXT_MUST_BE_REPLACED"));
    answer_context(client, second_context, B).await;
    input_finished(client, "turn_complete").await;
    let requests = server.captured_requests().await;
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_str(&requests[1].raw_body).unwrap();
    let system = body["system"].to_string();
    assert!(system.contains(B) && !system.contains(A));
    assert!(
        body["messages"].to_string().contains("first-turn"),
        "history survives the refresh"
    );
    assert!(body["messages"].to_string().contains("queued-turn"));
}

async fn verify_context_failure_and_retry(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    session: &str,
) {
    client
        .send_request_no_wait(
            "_sudocode/session/input",
            submission(session, "denied-turn"),
        )
        .await;
    let denied = context_request(client, session).await;
    client
        .send_raw(&json!({"jsonrpc":"2.0", "id":denied["id"],
        "error":{"code":-32603, "message":"model read permission revoked"}}))
        .await;
    input_finished(client, "error").await;
    assert_eq!(
        server.captured_requests().await.len(),
        2,
        "no fallback to stale context"
    );
    let (_, recalled) = client
        .send_request(
            "_sudocode/session/input",
            json!({
                "sessionId": session, "action": "recall",
            }),
        )
        .await;
    assert_eq!(
        recalled["result"]["recalled"][0]["clientId"], "denied-turn",
        "failed input remains owned by its author"
    );

    let (_, retry_receipt) = client
        .send_request("_sudocode/session/input", submission(session, "retry-turn"))
        .await;
    assert_eq!(retry_receipt["result"]["state"]["paused"], true);
    client
        .send_request_no_wait(
            "_sudocode/session/input",
            json!({
                "sessionId":session, "action":"flush",
            }),
        )
        .await;
    let retry = context_request(client, session).await;
    answer_context(client, retry, B).await;
    input_finished(client, "turn_complete").await;
    let requests = server.captured_requests().await;
    assert_eq!(requests.len(), 3);
    let body: Value = serde_json::from_str(&requests[2].raw_body).unwrap();
    assert!(body["system"].to_string().contains(B));
    assert!(!body["messages"].to_string().contains("denied-turn"));
    assert!(body["messages"].to_string().contains("first-turn"));
}

async fn verify_context_resume_and_isolation(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
    session: &str,
) {
    let (_, loaded) = client
        .send_request(
            "session/load",
            json!({
                "sessionId":session, "cwd":workspace.root.to_string_lossy(), "mcpServers":[],
                "_meta":{"sudocode":{"hostContext":{"version":1}, "appendSystemPrompt": B}},
            }),
        )
        .await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    let resumed = client.send_request_no_wait("session/prompt", json!({
        "sessionId":session, "prompt":[{"type":"text","text":format!("{SCENARIO_PREFIX}streaming_text resumed-turn")}],
    })).await;
    let context = context_request(client, session).await;
    answer_context(client, context, A).await;
    let (_, completed) = client
        .recv_until(RECV_TIMEOUT, |value| value["id"] == resumed)
        .await
        .unwrap();
    assert!(completed.get("error").is_none(), "{completed}");
    let requests = server.captured_requests().await;
    let body: Value = serde_json::from_str(&requests.last().unwrap().raw_body).unwrap();
    assert!(body["system"].to_string().contains(A) && !body["system"].to_string().contains(B));
    assert!(body["messages"].to_string().contains("queued-turn"));

    let plain = session_id_of(&session_new_with_meta(client, &workspace.root, json!({})).await);
    let body = last_model_request_after_prompt(client, server, &plain, "unrelated").await;
    assert!(
        !body.contains(A) && !body.contains(B),
        "provider is scoped to its session"
    );
    for value in [json!(true), json!({"version":2}), json!({"version":"1"})] {
        let invalid =
            session_new_with_meta(client, &workspace.root, json!({"hostContext":value})).await;
        assert_eq!(invalid["error"]["code"], -32602);
    }
}

async fn verify_host_context_cancellation(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
) {
    let session = session_id_of(
        &session_new_with_meta(
            client,
            &workspace.root,
            json!({"hostContext": {"version": 1}}),
        )
        .await,
    );
    let initial_requests = server.captured_requests().await.len();
    let prompt = client
        .send_request_no_wait(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text",
                "text": format!("{SCENARIO_PREFIX}streaming_text cancelled-host-context")}]}),
        )
        .await;
    let cancelled_request = context_request(client, &session).await;
    client
        .send_raw(&json!({"jsonrpc": "2.0", "method": "session/cancel",
            "params": {"sessionId": session}}))
        .await;
    client
        .recv_until(RECV_TIMEOUT, |value| value["id"] == prompt)
        .await
        .expect("cancel must finish an unanswered host context request");
    let (_, status) = client
        .send_request("_sudocode/session/status", json!({"sessionId": session}))
        .await;
    assert_eq!(status["result"]["foreground"], json!({"state": "idle"}));
    assert_eq!(server.captured_requests().await.len(), initial_requests);
    answer_context(client, cancelled_request, "STALE_CANCELLED_CONTEXT").await;

    let next = client
        .send_request_no_wait(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text",
                "text": format!("{SCENARIO_PREFIX}streaming_text fresh-after-cancel")}]}),
        )
        .await;
    let current_request = context_request(client, &session).await;
    answer_context(client, current_request, "FRESH_CONTEXT_AFTER_CANCEL").await;
    let (_, completed) = client
        .recv_until(RECV_TIMEOUT, |value| value["id"] == next)
        .await
        .expect("a new turn must complete after host context cancellation");
    assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
    let requests = server.captured_requests().await;
    assert_eq!(requests.len(), initial_requests + 1);
    let body: Value = serde_json::from_str(&requests.last().unwrap().raw_body).unwrap();
    assert!(body["system"]
        .to_string()
        .contains("FRESH_CONTEXT_AFTER_CANCEL"));
    assert!(!body["system"]
        .to_string()
        .contains("STALE_CANCELLED_CONTEXT"));
    assert!(!body["messages"]
        .to_string()
        .contains("cancelled-host-context"));
}

async fn verify_context_refresh_preserves_questions(
    client: &mut AcpTestClient,
    server: &MockAnthropicService,
    workspace: &TestWorkspace,
) {
    let session = session_id_of(
        &session_new_with_meta(
            client,
            &workspace.root,
            json!({"hostContext": {"version": 1}}),
        )
        .await,
    );
    set_permission_mode(client, &session, "allow").await;
    for (index, context) in [A, B].into_iter().enumerate() {
        let answer = format!("HOST_CONTEXT_QUESTION_ANSWER_{index}");
        let calls = json!([{
            "id": format!("context-question-{index}"), "name": "AskUserQuestion",
            "input": {"question": format!("Question after context refresh {index}")},
        }]);
        let prompt = client
            .send_request_no_wait(
                "session/prompt",
                json!({
                    "sessionId": session, "prompt": [{"type": "text",
                        "text": format!("{SCENARIO_PREFIX}tool_concurrency TOOL_BATCH:{calls}")}],
                }),
            )
            .await;
        let request = context_request(client, &session).await;
        answer_context(client, request, context).await;
        let (_, question) = client
            .recv_until(RECV_TIMEOUT, |value| {
                is_server_request(value, "_scode/ask_user_question")
            })
            .await
            .expect("runtime rebuilding must retain the current turn's question bridge");
        let (_, status) = client
            .send_request("_sudocode/session/status", json!({"sessionId": session}))
            .await;
        assert_eq!(
            status["result"]["foreground"],
            json!({"state": "awaiting_input", "waiting_for": "question"})
        );
        client
            .send_raw(&json!({"jsonrpc": "2.0", "id": question["id"],
                "result": {"answers": [{"id": "q1", "value": answer, "label": answer}]},
            }))
            .await;
        let (_, completed) = client
            .recv_until(RECV_TIMEOUT, |value| is_response_to(value, prompt))
            .await
            .expect("the live answer must complete the original turn");
        assert_eq!(completed["result"]["stopReason"], "end_turn", "{completed}");
        let requests = server.captured_requests().await;
        let request = requests
            .iter()
            .rev()
            .find(|request| !request.path.contains("count_tokens"))
            .unwrap();
        let body: Value = serde_json::from_str(&request.raw_body).unwrap();
        assert!(body["system"].to_string().contains(context));
        assert!(body["messages"].to_string().contains(&answer));
        if index == 1 {
            assert!(!body["system"].to_string().contains(A));
        }
        let (_, status) = client
            .send_request("_sudocode/session/status", json!({"sessionId": session}))
            .await;
        assert_eq!(status["result"]["foreground"], json!({"state": "idle"}));
    }
}

#[tokio::test]
async fn acp_host_context_refreshes_queued_turns_and_fails_closed() {
    for is_websocket in [false, true] {
        let server = MockAnthropicService::spawn().await.unwrap();
        let workspace = TestWorkspace::new("live-host-context");
        workspace.create();
        workspace.write_sudocode_json(&server.base_url());
        let mut client = if is_websocket {
            spawn_ws_client(&workspace).await
        } else {
            spawn_stdio_client(&workspace)
        };
        let (_, initialized) = client
            .send_request("initialize", json!({"protocolVersion": 1}))
            .await;
        assert_eq!(
            initialized["result"]["_meta"]["sudocode"]["hostContext"]["version"],
            1
        );
        assert_eq!(
            initialized["result"]["_meta"]["sudocode"]["runtimeStatus"]["version"],
            1
        );
        let response = session_new_with_meta(
            &mut client,
            &workspace.root,
            json!({
                "appendSystemPrompt": "INITIAL_CONTEXT_MUST_BE_REPLACED",
                "hostContext": {"version": 1},
            }),
        )
        .await;
        let session = session_id_of(&response);

        verify_queued_context_refresh(&mut client, &server, &session).await;
        verify_context_failure_and_retry(&mut client, &server, &session).await;
        verify_context_resume_and_isolation(&mut client, &server, &workspace, &session).await;
        verify_host_context_cancellation(&mut client, &server, &workspace).await;
        verify_context_refresh_preserves_questions(&mut client, &server, &workspace).await;

        client.shutdown().await;
        workspace.cleanup();
    }
}
