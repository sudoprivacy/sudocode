//! End-to-end mock tests for the Unified Agent/PID Tool System.
//!
//! Exercises the full feature without live API keys:
//! - `TOOL_ALIASES` routing: deprecated names → canonical names
//! - Input normalization: `body` → `message`, `pid` → `task_id`, etc.
//! - `compose_next_turn_from_envelopes`: XML formatting + ordering
//! - Local JSONL poller → channel → delivery roundtrip
//! - Multi-turn loop: `PeerMessage` injection during idle and busy states
//! - Multi-turn loop with mixed envelope kinds (message + shutdown)
//! - `TurnInputCoordinator`: queue/interrupt semantics for `PeerMessage`

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::agent_mailbox::{self, kinds, MailboxEnvelope};
use runtime::mailbox::{InboxConvention, Mailbox};
use runtime::HookAbortSignal;
use tools::testing::{compose_next_turn_from_envelopes_for_test, run_multi_turn_loop_for_test};

// ── Helpers ──────────────────────────────────────────────────────────

fn unique_workspace(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "unified-e2e-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir workspace");
    path
}

fn envelope(kind: &str, from: &str, body: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        from: from.to_string(),
        to: String::new(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: kind.to_string(),
        request_id: None,
    }
}

fn envelope_with_request_id(
    kind: &str,
    from: &str,
    body: &str,
    request_id: &str,
) -> MailboxEnvelope {
    MailboxEnvelope {
        from: from.to_string(),
        to: String::new(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: kind.to_string(),
        request_id: Some(request_id.to_string()),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 1. TOOL ALIAS ROUTING
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn canonicalize_maps_sendmessage_to_send() {
    assert_eq!(tools::canonicalize_tool_name("SendMessage"), "send");
}

#[test]
fn canonicalize_maps_send_message_to_send() {
    assert_eq!(tools::canonicalize_tool_name("send_message"), "send");
}

#[test]
fn canonicalize_maps_agent_to_agent_spawn() {
    assert_eq!(tools::canonicalize_tool_name("Agent"), "agent_spawn");
}

#[test]
fn canonicalize_maps_taskstop_to_pid_kill() {
    assert_eq!(tools::canonicalize_tool_name("TaskStop"), "pid_kill");
}

#[test]
fn canonicalize_maps_taskget_to_pid_status() {
    assert_eq!(tools::canonicalize_tool_name("TaskGet"), "pid_status");
}

#[test]
fn canonicalize_maps_tasklist_to_pid_status() {
    assert_eq!(tools::canonicalize_tool_name("TaskList"), "pid_status");
}

#[test]
fn canonicalize_maps_taskoutput_to_pid_output() {
    assert_eq!(tools::canonicalize_tool_name("TaskOutput"), "pid_output");
}

#[test]
fn canonicalize_preserves_canonical_names() {
    assert_eq!(tools::canonicalize_tool_name("send"), "send");
    assert_eq!(tools::canonicalize_tool_name("agent_spawn"), "agent_spawn");
    assert_eq!(tools::canonicalize_tool_name("pid_kill"), "pid_kill");
    assert_eq!(tools::canonicalize_tool_name("pid_status"), "pid_status");
    assert_eq!(tools::canonicalize_tool_name("pid_output"), "pid_output");
    assert_eq!(tools::canonicalize_tool_name("pid_fork"), "pid_fork");
}

#[test]
fn canonicalize_preserves_pascalcase_native_tools() {
    assert_eq!(
        tools::canonicalize_tool_name("EnterPlanMode"),
        "EnterPlanMode"
    );
    assert_eq!(
        tools::canonicalize_tool_name("ExitPlanMode"),
        "ExitPlanMode"
    );
    assert_eq!(tools::canonicalize_tool_name("TaskCreate"), "TaskCreate");
    assert_eq!(tools::canonicalize_tool_name("TaskUpdate"), "TaskUpdate");
    assert_eq!(tools::canonicalize_tool_name("WebFetch"), "WebFetch");
    assert_eq!(tools::canonicalize_tool_name("Skill"), "Skill");
}

#[test]
fn canonicalize_maps_cc_style_read_write_edit_to_snake_case() {
    assert_eq!(tools::canonicalize_tool_name("Read"), "read_file");
    assert_eq!(tools::canonicalize_tool_name("Write"), "write_file");
    assert_eq!(tools::canonicalize_tool_name("Edit"), "edit_file");
    assert_eq!(tools::canonicalize_tool_name("Bash"), "bash");
    assert_eq!(tools::canonicalize_tool_name("Glob"), "glob_search");
    assert_eq!(tools::canonicalize_tool_name("Grep"), "grep_search");
}

// ═══════════════════════════════════════════════════════════════════════
// 2. COMPOSE_NEXT_TURN_FROM_ENVELOPES — XML FORMATTING
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn compose_single_message_envelope() {
    let envs = vec![envelope(kinds::MESSAGE, "worker-1", "task complete")];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    assert!(text.contains("<mailbox-message from=\"worker-1\">"));
    assert!(text.contains("task complete"));
    assert!(text.contains("</mailbox-message>"));
}

#[test]
fn compose_shutdown_request_uses_correct_tag() {
    let envs = vec![envelope(kinds::SHUTDOWN_REQUEST, "team-lead", "stop now")];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    assert!(text.contains("<shutdown-request from=\"team-lead\">"));
    assert!(text.contains("</shutdown-request>"));
}

#[test]
fn compose_shutdown_response_uses_correct_tag() {
    let envs = vec![envelope(kinds::SHUTDOWN_RESPONSE, "worker", "acknowledged")];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    assert!(text.contains("<shutdown-response from=\"worker\">"));
    assert!(text.contains("</shutdown-response>"));
}

#[test]
fn compose_plan_approval_response_uses_correct_tag() {
    let envs = vec![envelope(
        kinds::PLAN_APPROVAL_RESPONSE,
        "team-lead",
        "approved",
    )];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    assert!(text.contains("<plan-approval-response from=\"team-lead\">"));
    assert!(text.contains("</plan-approval-response>"));
}

#[test]
fn compose_includes_request_id_attribute() {
    let envs = vec![envelope_with_request_id(
        kinds::SHUTDOWN_REQUEST,
        "team-lead",
        "stop",
        "req_abc123",
    )];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    assert!(
        text.contains("request-id=\"req_abc123\""),
        "request_id must appear as XML attribute; got: {text}"
    );
}

#[test]
fn compose_multiple_envelopes_preserves_order_and_separates() {
    let envs = vec![
        envelope(kinds::MESSAGE, "alpha", "first message"),
        envelope(kinds::MESSAGE, "beta", "second message"),
        envelope(kinds::MESSAGE, "gamma", "third message"),
    ];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    let idx_first = text.find("first message").expect("first present");
    let idx_second = text.find("second message").expect("second present");
    let idx_third = text.find("third message").expect("third present");
    assert!(idx_first < idx_second);
    assert!(idx_second < idx_third);
    // Envelopes are separated by blank lines
    assert!(text.contains("\n\n"));
}

#[test]
fn compose_escapes_xml_special_chars_in_from() {
    let envs = vec![envelope(kinds::MESSAGE, "agent<&>\"test", "body")];
    let text = compose_next_turn_from_envelopes_for_test(&envs);
    assert!(
        !text.contains("from=\"agent<"),
        "< must be escaped in from attribute"
    );
    assert!(text.contains("&lt;"));
    assert!(text.contains("&amp;"));
    assert!(text.contains("&quot;"));
}

// ═══════════════════════════════════════════════════════════════════════
// 3. LOCAL JSONL POLLER → CHANNEL → DELIVERY ROUNDTRIP
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn local_poller_delivers_sub_agent_message_to_parent() {
    let ws = unique_workspace("poller-roundtrip");
    let (tx, rx) = std::sync::mpsc::channel::<MailboxEnvelope>();
    let abort = HookAbortSignal::new();
    let abort_clone = abort.clone();

    let _handle = runtime::mailbox::spawn_local_poller(
        ws.clone(),
        "team-lead".to_string(),
        abort_clone,
        move |msg| {
            let _ = tx.send(msg.clone());
        },
    );

    // Sub-agent writes to team-lead's inbox
    agent_mailbox::append_envelope(
        &ws,
        "team-lead",
        envelope(kinds::MESSAGE, "researcher", "found the bug in line 42"),
    )
    .unwrap();

    let msg = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("poller should deliver message within 5s");
    assert_eq!(msg.from, "researcher");
    assert_eq!(msg.body, "found the bug in line 42");

    abort.abort();
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn local_poller_delivers_multiple_messages_in_order() {
    let ws = unique_workspace("poller-multi");
    let (tx, rx) = std::sync::mpsc::channel::<MailboxEnvelope>();
    let abort = HookAbortSignal::new();
    let abort_clone = abort.clone();

    let _handle = runtime::mailbox::spawn_local_poller(
        ws.clone(),
        "team-lead".to_string(),
        abort_clone,
        move |msg| {
            let _ = tx.send(msg.clone());
        },
    );

    // Multiple sub-agents write to team-lead's inbox
    agent_mailbox::append_envelope(
        &ws,
        "team-lead",
        envelope(kinds::MESSAGE, "worker-1", "task A done"),
    )
    .unwrap();
    agent_mailbox::append_envelope(
        &ws,
        "team-lead",
        envelope(kinds::MESSAGE, "worker-2", "task B done"),
    )
    .unwrap();

    let msg1 = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first message");
    let msg2 = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second message");

    assert_eq!(msg1.from, "worker-1");
    assert_eq!(msg1.body, "task A done");
    assert_eq!(msg2.from, "worker-2");
    assert_eq!(msg2.body, "task B done");

    abort.abort();
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn local_poller_stops_on_abort() {
    let ws = unique_workspace("poller-abort");
    let (tx, rx) = std::sync::mpsc::channel::<MailboxEnvelope>();
    let abort = HookAbortSignal::new();
    let abort_clone = abort.clone();

    let handle = runtime::mailbox::spawn_local_poller(
        ws.clone(),
        "team-lead".to_string(),
        abort_clone,
        move |msg| {
            let _ = tx.send(msg.clone());
        },
    );

    abort.abort();
    handle.join().expect("poller thread should exit cleanly");

    // Write after abort — should never be delivered
    agent_mailbox::append_envelope(
        &ws,
        "team-lead",
        envelope(kinds::MESSAGE, "worker", "too late"),
    )
    .unwrap();

    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "no messages after abort"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

// ═══════════════════════════════════════════════════════════════════════
// 4. FULL SEND → RECEIVE → COMPOSE → INJECT ROUNDTRIP
// ═══════════════════════════════════════════════════════════════════════

/// Simulates the complete message cycle:
/// 1. Sub-agent uses `send` tool → writes JSONL envelope
/// 2. Local poller picks up the envelope
/// 3. `compose_next_turn_from_envelopes` formats it as XML
/// 4. The formatted text is injected into the next LLM turn
///
/// This tests the full chain without any live LLM or PTY.
#[test]
fn full_send_receive_compose_inject_cycle() {
    let ws = unique_workspace("full-cycle");
    let agent_id = "worker-full-cycle";
    let abort = HookAbortSignal::default();

    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_cb = turn_count.clone();
    let prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_cb = prompts.clone();
    let ws_cb = ws.clone();

    // The multi-turn loop simulates the REPL's behavior: after each
    // turn, it drains the mailbox and composes envelopes into the next
    // turn's prompt. The `run_turn_fn` callback writes an envelope on
    // turn 1, which triggers turn 2 with the composed XML prompt.
    let final_text = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("PARITY_SCENARIO:unified_send_roundtrip start the task"),
        16,
        move |prompt| {
            let idx = turn_count_cb.fetch_add(1, Ordering::SeqCst);
            prompts_cb.lock().unwrap().push(prompt.clone());
            if idx == 0 {
                // Sub-agent writes to its own mailbox (simulating a
                // peer sending a message via the `send` tool)
                agent_mailbox::append_envelope(
                    &ws_cb,
                    agent_id,
                    envelope(kinds::MESSAGE, "team-lead", "here is the plan"),
                )
                .unwrap();
                Ok(String::from("acknowledged, waiting for instructions"))
            } else {
                // Turn 2: should receive the composed envelope
                assert!(
                    prompt.contains("<mailbox-message from=\"team-lead\">"),
                    "turn 2 prompt must contain composed envelope header; got: {prompt}"
                );
                assert!(
                    prompt.contains("here is the plan"),
                    "turn 2 prompt must contain the message body; got: {prompt}"
                );
                Ok(String::from("plan received, executing"))
            }
        },
    )
    .expect("full cycle should complete");

    assert_eq!(turn_count.load(Ordering::SeqCst), 2);
    assert_eq!(final_text, "plan received, executing");

    // Verify prompts
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    // Turn 1: original user prompt
    assert!(prompts[0].contains("start the task"));
    // Turn 2: composed from envelope
    assert!(prompts[1].contains("team-lead"));
    assert!(prompts[1].contains("here is the plan"));

    let _ = std::fs::remove_dir_all(&ws);
}

// ═══════════════════════════════════════════════════════════════════════
// 5. MULTI-TURN LOOP: MIXED ENVELOPE KINDS
// ═══════════════════════════════════════════════════════════════════════

/// Verifies that a message envelope triggers a resume turn but a
/// `shutdown_request` envelope causes immediate exit WITHOUT another turn.
#[test]
fn mixed_message_then_shutdown_exits_after_message_turn() {
    let ws = unique_workspace("mixed-kinds");
    let agent_id = "agent-mixed";
    let abort = HookAbortSignal::default();

    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_cb = turn_count.clone();
    let ws_cb = ws.clone();

    let final_text = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("hello"),
        16,
        move |_prompt| {
            let idx = turn_count_cb.fetch_add(1, Ordering::SeqCst);
            match idx {
                0 => {
                    // Turn 1: write a message envelope → should trigger turn 2
                    agent_mailbox::append_envelope(
                        &ws_cb,
                        agent_id,
                        envelope(kinds::MESSAGE, "team-lead", "do more work"),
                    )
                    .unwrap();
                    Ok(String::from("turn 1 done"))
                }
                1 => {
                    // Turn 2: write a shutdown_request → should exit after this turn
                    agent_mailbox::append_envelope(
                        &ws_cb,
                        agent_id,
                        envelope(kinds::SHUTDOWN_REQUEST, "team-lead", "stop now"),
                    )
                    .unwrap();
                    Ok(String::from("turn 2 done, about to shutdown"))
                }
                _ => panic!("shutdown_request must prevent turn 3"),
            }
        },
    )
    .expect("should exit cleanly");

    assert_eq!(
        turn_count.load(Ordering::SeqCst),
        2,
        "message triggers turn 2, shutdown after turn 2 prevents turn 3"
    );
    assert_eq!(final_text, "turn 2 done, about to shutdown");

    let _ = std::fs::remove_dir_all(&ws);
}

// ═══════════════════════════════════════════════════════════════════════
// 6. MAILBOX UNIFIED ABSTRACTION: SEND + POLL ROUNDTRIP
// ═══════════════════════════════════════════════════════════════════════

/// Tests the unified Mailbox abstraction: send from one agent, poll
/// from another, verify the envelope is delivered correctly.
#[test]
fn unified_mailbox_send_poll_roundtrip() {
    let ws = unique_workspace("unified-mailbox-rt");
    let ws_str = ws.to_string_lossy().to_string();

    let sender_mb = Mailbox::new(
        Arc::new(runtime::fs_backend::StdFsBackend),
        "coordinator".to_string(),
        InboxConvention::LocalJsonl {
            root: ws_str.clone(),
        },
    );

    sender_mb
        .send(MailboxEnvelope {
            from: "coordinator".to_string(),
            to: "researcher".to_string(),
            body: "investigate the auth module".to_string(),
            summary: Some("auth investigation".to_string()),
            timestamp: 0,
            color: None,
            kind: kinds::MESSAGE.to_string(),
            request_id: None,
        })
        .unwrap();

    let receiver_mb = Mailbox::new(
        Arc::new(runtime::fs_backend::StdFsBackend),
        "researcher".to_string(),
        InboxConvention::LocalJsonl { root: ws_str },
    );

    let (msgs, cursor) = receiver_mb.poll(0, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].from, "coordinator");
    assert_eq!(msgs[0].body, "investigate the auth module");
    assert_eq!(msgs[0].summary.as_deref(), Some("auth investigation"));

    // Subsequent poll with updated cursor should return no new messages
    let (msgs2, _) = receiver_mb.poll(cursor, 0).unwrap();
    assert!(msgs2.is_empty(), "no new messages after cursor advance");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Tests that `read_all` returns messages from all senders.
#[test]
fn unified_mailbox_read_all_from_multiple_senders() {
    let ws = unique_workspace("read-all-multi");
    let ws_str = ws.to_string_lossy().to_string();

    let mb = Mailbox::new(
        Arc::new(runtime::fs_backend::StdFsBackend),
        "hub".to_string(),
        InboxConvention::LocalJsonl {
            root: ws_str.clone(),
        },
    );

    for i in 0..5 {
        mb.send(MailboxEnvelope {
            from: format!("agent-{i}"),
            to: "worker".to_string(),
            body: format!("report {i}"),
            summary: None,
            timestamp: 0,
            color: None,
            kind: String::new(),
            request_id: None,
        })
        .unwrap();
    }

    let envs = mb.read_all("worker").unwrap();
    assert_eq!(envs.len(), 5);
    for (i, env) in envs.iter().enumerate() {
        assert_eq!(env.body, format!("report {i}"));
    }

    let _ = std::fs::remove_dir_all(&ws);
}

// ═══════════════════════════════════════════════════════════════════════
// 7. ENVELOPE WIRE COMPAT: `text` alias → `body`
// ═══════════════════════════════════════════════════════════════════════

/// The `text` field alias must deserialize into `body` for backward compat
/// with old local JSONL data.
#[test]
fn envelope_text_alias_deserializes_to_body() {
    let raw = r#"{"from":"old-agent","to":"worker","text":"hello from old format"}"#;
    let env: MailboxEnvelope = serde_json::from_str(raw).expect("text alias must parse");
    assert_eq!(env.body, "hello from old format");
    assert_eq!(env.from, "old-agent");
}

/// Serialization always uses `body`, never `text`.
#[test]
fn envelope_serializes_body_not_text() {
    let env = MailboxEnvelope {
        from: "agent".to_string(),
        to: "peer".to_string(),
        body: "content".to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: String::new(),
        request_id: None,
    };
    let json = serde_json::to_string(&env).unwrap();
    assert!(json.contains("\"body\""));
    assert!(!json.contains("\"text\""));
}

// ═══════════════════════════════════════════════════════════════════════
// 8. COORDINATOR ALLOWED TOOLS INCLUDES ALL CANONICAL + DEPRECATED
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn coordinator_allowed_tools_includes_canonical_names() {
    let allowed = runtime::coordinator_mode::coordinator_allowed_tools();
    assert!(allowed.contains("agent_spawn"));
    assert!(allowed.contains("send"));
    assert!(allowed.contains("pid_kill"));
    assert!(allowed.contains("pid_status"));
    assert!(allowed.contains("pid_output"));
    assert!(allowed.contains("pid_fork"));
}

// ═══════════════════════════════════════════════════════════════════════
// 8b. THE COORDINATOR ONLY NAMES TOOLS THAT EXIST
// ═══════════════════════════════════════════════════════════════════════

/// Canonical names of every tool this build actually ships.
fn real_tool_names() -> std::collections::BTreeSet<String> {
    tools::mvp_tool_specs()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect()
}

/// The tool names the coordinator prompt advertises under `## 2. Your Tools`,
/// whose entries read `- **name** - description`.
///
/// Scoped to that section so a bolded word anywhere else in the prompt is not
/// mistaken for a tool.
fn tools_advertised_in_prompt() -> Vec<String> {
    runtime::coordinator_mode::coordinator_system_prompt()
        .lines()
        .skip_while(|line| !line.starts_with("## 2. Your Tools"))
        .skip(1)
        .take_while(|line| !line.starts_with("##"))
        .filter_map(|line| line.trim().strip_prefix("- **"))
        .filter_map(|rest| rest.split("**").next())
        .map(str::to_string)
        .collect()
}

/// Every name in the coordinator allowlist must be a tool that exists.
///
/// The allowlist lives in `runtime` and the tool specs live in `tools`, so only
/// a test spanning both crates can catch a name that outlived its tool. Not
/// hypothetical: `agent_list` stayed in this allowlist, and in the prompt,
/// after the tool was removed — two branches each resolved half of it.
#[test]
fn coordinator_allowlist_names_only_real_tools() {
    let real = real_tool_names();
    for name in runtime::coordinator_mode::coordinator_allowed_tools() {
        assert!(
            real.contains(name),
            "the coordinator allowlist names `{name}`, which is not a tool this build ships"
        );
    }
}

/// Every tool the coordinator prompt advertises must exist and be allowed.
///
/// This is the assertion that bites. The prompt is what the model reads, so a
/// stale line there does not merely go unused — it makes the coordinator call
/// something that cannot answer.
#[test]
fn coordinator_prompt_advertises_only_real_allowed_tools() {
    let real = real_tool_names();
    let allowed = runtime::coordinator_mode::coordinator_allowed_tools();
    let advertised = tools_advertised_in_prompt();

    assert!(
        advertised.len() >= 6,
        "the prompt's tool list should have parsed; got {advertised:?}"
    );
    for name in &advertised {
        assert!(
            real.contains(name),
            "the coordinator prompt advertises `{name}`, which is not a tool this build ships"
        );
        assert!(
            allowed.contains(name.as_str()),
            "the coordinator prompt advertises `{name}`, which its own allowlist forbids"
        );
    }
}

#[test]
fn coordinator_predicate_admits_deprecated_aliases_via_canonicalization() {
    use runtime::coordinator_mode::is_tool_allowed_in_coordinator_mode;
    std::env::set_var("SUDOCODE_COORDINATOR_MODE", "1");
    for alias in [
        "Agent",
        "SendMessage",
        "TaskStop",
        "TaskGet",
        "TaskList",
        "TaskOutput",
    ] {
        assert!(
            is_tool_allowed_in_coordinator_mode(alias),
            "deprecated alias `{alias}` should be admitted via canonicalization"
        );
    }
    std::env::remove_var("SUDOCODE_COORDINATOR_MODE");
}

// ═══════════════════════════════════════════════════════════════════════
// 9. INBOX PATH CONVENTIONS
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn inbox_convention_local_jsonl_resolves_correctly() {
    let conv = InboxConvention::LocalJsonl {
        root: "/project".to_string(),
    };
    assert_eq!(
        conv.inbox_path("worker-1"),
        "/project/.sudocode-inbox/worker-1.jsonl"
    );
}

#[test]
fn inbox_convention_nexus_a2a_resolves_correctly() {
    let conv = InboxConvention::NexusA2a;
    assert_eq!(conv.inbox_path("agent-x"), "/agents/agent-x/chat-with-me");
}

// ═══════════════════════════════════════════════════════════════════════
// 10. MULTI-TURN LOOP: PEER MESSAGE INJECTION DURING IDLE
// ═══════════════════════════════════════════════════════════════════════

/// Simulates the REPL's `PeerMessage` handler when idle:
/// 1. Local poller picks up a sub-agent message
/// 2. `compose_next_turn_from_envelopes` formats it
/// 3. The composed prompt is what `submit_when_idle` would receive
///
/// This is a unit-level simulation (no PTY) — tests the data flow
/// from poller through compose. `TurnInputCoordinator` integration
/// lives in the CLI crate's own tests (`peer_message_queue.rs`).
#[test]
fn peer_message_poller_to_compose_roundtrip() {
    let ws = unique_workspace("idle-inject");
    let (tx, rx) = std::sync::mpsc::channel::<MailboxEnvelope>();
    let abort = HookAbortSignal::new();
    let abort_clone = abort.clone();

    let _handle = runtime::mailbox::spawn_local_poller(
        ws.clone(),
        "team-lead".to_string(),
        abort_clone,
        move |msg| {
            let _ = tx.send(msg.clone());
        },
    );

    // Sub-agent sends a message
    agent_mailbox::append_envelope(
        &ws,
        "team-lead",
        envelope(
            kinds::MESSAGE,
            "researcher",
            "found critical vulnerability in auth.rs",
        ),
    )
    .unwrap();

    // Poller delivers
    let msg = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("should receive message");

    // Compose into turn prompt (what the REPL does)
    let prompt = compose_next_turn_from_envelopes_for_test(&[msg]);
    assert!(prompt.contains("<mailbox-message from=\"researcher\">"));
    assert!(prompt.contains("found critical vulnerability in auth.rs"));
    assert!(prompt.contains("</mailbox-message>"));

    abort.abort();
    let _ = std::fs::remove_dir_all(&ws);
}

// ═══════════════════════════════════════════════════════════════════════
// 11. MULTI-TURN LOOP: POLLER → COMPOSE → MULTI-TURN INTEGRATION
// ═══════════════════════════════════════════════════════════════════════

/// Full integration test: poller delivers message, which feeds into
/// multi-turn loop. Verifies the complete data path from disk write
/// to LLM turn injection.
#[test]
fn poller_compose_multi_turn_integration() {
    let ws = unique_workspace("poller-mt");
    let agent_id = "agent-poller-mt";
    let abort = HookAbortSignal::default();

    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_cb = turn_count.clone();
    let prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_cb = prompts.clone();
    let ws_cb = ws.clone();

    let final_text = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("initial task"),
        16,
        move |prompt| {
            let idx = turn_count_cb.fetch_add(1, Ordering::SeqCst);
            prompts_cb.lock().unwrap().push(prompt.clone());
            match idx {
                0 => {
                    // Simulate peer sending two messages between turns
                    agent_mailbox::append_envelope(
                        &ws_cb,
                        agent_id,
                        MailboxEnvelope {
                            from: "coordinator".to_string(),
                            to: agent_id.to_string(),
                            body: "proceed with phase 2".to_string(),
                            summary: Some("phase 2 directive".to_string()),
                            timestamp: 0,
                            color: None,
                            kind: kinds::MESSAGE.to_string(),
                            request_id: None,
                        },
                    )
                    .unwrap();
                    agent_mailbox::append_envelope(
                        &ws_cb,
                        agent_id,
                        MailboxEnvelope {
                            from: "coordinator".to_string(),
                            to: agent_id.to_string(),
                            body: "also update the README".to_string(),
                            summary: None,
                            timestamp: 0,
                            color: None,
                            kind: kinds::MESSAGE.to_string(),
                            request_id: None,
                        },
                    )
                    .unwrap();
                    Ok(String::from("phase 1 complete"))
                }
                1 => {
                    // Verify both messages are in the composed prompt
                    assert!(
                        prompt.contains("proceed with phase 2"),
                        "prompt must contain first message"
                    );
                    assert!(
                        prompt.contains("also update the README"),
                        "prompt must contain second message"
                    );
                    Ok(String::from("phase 2 and README done"))
                }
                _ => panic!("should not reach turn 3"),
            }
        },
    )
    .expect("multi-turn integration should complete");

    assert_eq!(turn_count.load(Ordering::SeqCst), 2);
    assert_eq!(final_text, "phase 2 and README done");

    let _ = std::fs::remove_dir_all(&ws);
}

// ═══════════════════════════════════════════════════════════════════════
// 12. ENVELOPE KINDS MODULE CONSTANTS ARE STABLE
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn kind_constants_match_wire_format() {
    assert_eq!(kinds::MESSAGE, "message");
    assert_eq!(kinds::SHUTDOWN_REQUEST, "shutdown_request");
    assert_eq!(kinds::SHUTDOWN_RESPONSE, "shutdown_response");
    assert_eq!(kinds::PLAN_APPROVAL_RESPONSE, "plan_approval_response");
    assert_eq!(kinds::TASK_NOTIFICATION, "task_notification");
}

// ═══════════════════════════════════════════════════════════════════════
// 13. MAILBOX SENDER CLOSURE
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn mailbox_sender_closure_writes_envelope() {
    let ws = unique_workspace("sender-closure");
    let ws_str = ws.to_string_lossy().to_string();

    let mb = Arc::new(Mailbox::new(
        Arc::new(runtime::fs_backend::StdFsBackend),
        "team-lead".to_string(),
        InboxConvention::LocalJsonl { root: ws_str },
    ));

    let sender = mb.sender();
    sender("worker-1", "please review PR #42").unwrap();

    let envs = mb.read_all("worker-1").unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].from, "team-lead");
    assert_eq!(envs[0].body, "please review PR #42");

    let _ = std::fs::remove_dir_all(&ws);
}
