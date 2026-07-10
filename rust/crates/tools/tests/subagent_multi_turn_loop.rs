//! Integration tests for the sub-agent multi-turn loop
//! (`run_multi_turn_loop` inside `tools::run_agent_job_returning_text`).
//!
//! Prior to the multi-turn refactor, `run_agent_job_returning_text`
//! was a one-shot: run one `run_turn`, return final text, exit.
//! SendMessage plain-text envelopes accumulated on disk with no
//! consumer. The multi-turn loop closes that gap — after each
//! `run_turn`, the loop drains new envelopes from the agent's
//! mailbox and treats them as the next user-turn's prompt.
//!
//! ## Scenarios exercised (long-workflow, data-flow chained)
//!
//! 1. **Empty mailbox → one-shot exit** — no envelopes ever arrive,
//!    loop matches pre-refactor behavior (regression sentinel).
//! 2. **SendMessage → resume with synth prompt** — parent writes a
//!    plain-text envelope AFTER turn 1; loop drains it, calls
//!    `run_turn_fn` again with the composed
//!    `<mailbox-message from="…">…</mailbox-message>` block; loop
//!    exits after turn 2 because inbox is empty again.
//! 3. **shutdown_request → clean exit** — parent writes a
//!    `shutdown_request` envelope; loop sees it in the drain, exits
//!    with the last final text WITHOUT calling `run_turn` again
//!    (mirrors CC-fork's `resumeAgentBackground` → abort path).
//! 4. **Abort signal fired mid-turn → immediate exit** — a
//!    `SendMessage(shutdown_request)` flips the abort registry
//!    while `run_turn` is running; loop exits right after the
//!    interrupted turn returns, ignoring any envelopes.
//! 5. **Two envelopes back-to-back → single synth turn** — parent
//!    fires two messages between our drains; both are folded into
//!    ONE synthetic user turn (order preserved).
//! 6. **Max multi-turn cap → force-exit** — envelopes never stop
//!    arriving; loop caps at N to prevent runaway.
//!
//! Every test writes envelopes with the SAME `agent_mailbox` API
//! that production `SendMessage` uses (`append_envelope`), so the
//! JSONL wire format is exercised end-to-end.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use runtime::agent_mailbox::{self, kinds, MailboxEnvelope};
use runtime::HookAbortSignal;
use tools::testing::run_multi_turn_loop_for_test;

fn unique_workspace(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "sudocode-multi-turn-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir workspace");
    path
}

fn envelope(kind: &str, from: &str, text: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        from: from.to_string(),
        to: String::new(), // filled by append_envelope
        text: text.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: kind.to_string(),
        request_id: None,
    }
}

#[test]
fn empty_mailbox_yields_single_turn() {
    let ws = unique_workspace("empty");
    let agent_id = "agent-empty-mbox";
    let abort = HookAbortSignal::default();

    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_seen = turn_count.clone();
    let prompts_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_seen_clone = prompts_seen.clone();

    let final_text = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("hello"),
        16,
        move |prompt| {
            turn_count_seen.fetch_add(1, Ordering::SeqCst);
            prompts_seen_clone.lock().unwrap().push(prompt);
            Ok(String::from("assistant reply 1"))
        },
    )
    .expect("loop should exit cleanly");

    assert_eq!(turn_count.load(Ordering::SeqCst), 1);
    assert_eq!(final_text, "assistant reply 1");
    let prompts = prompts_seen.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0], "hello");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn message_after_turn_1_triggers_synthetic_turn_2_then_exit() {
    let ws = unique_workspace("resume");
    let agent_id = "agent-resume";
    let abort = HookAbortSignal::default();

    // We'll write a mailbox envelope AFTER turn 1 completes so the
    // loop reads it during its drain and feeds it into turn 2.
    // The turn callback below is invoked twice (once for `hello`,
    // once for the synthesised prompt containing the message).
    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_cb = turn_count.clone();
    let prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_cb = prompts.clone();
    let ws_cb = ws.clone();

    let final_text = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("hello"),
        16,
        move |prompt| {
            let turn_idx = turn_count_cb.fetch_add(1, Ordering::SeqCst);
            prompts_cb.lock().unwrap().push(prompt.clone());
            if turn_idx == 0 {
                // Just after turn 1's `run_turn`, the loop drains
                // the mailbox. Write an envelope BEFORE returning so
                // the drain sees it.
                agent_mailbox::append_envelope(
                    &ws_cb,
                    agent_id,
                    envelope(kinds::MESSAGE, "team-lead", "continue the plan"),
                )
                .expect("append envelope");
                Ok(String::from("worker finished turn 1"))
            } else {
                Ok(String::from("worker finished turn 2"))
            }
        },
    )
    .expect("loop should exit cleanly");

    assert_eq!(
        turn_count.load(Ordering::SeqCst),
        2,
        "loop must call run_turn_fn twice — original + resume"
    );
    assert_eq!(final_text, "worker finished turn 2");

    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts[0], "hello");
    // Synth prompt must wrap the envelope in the mailbox-message
    // XML block and preserve the sender + body.
    assert!(
        prompts[1].contains("<mailbox-message from=\"team-lead\">"),
        "turn 2 prompt should carry the mailbox-message header; got {:?}",
        prompts[1]
    );
    assert!(prompts[1].contains("continue the plan"));

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn shutdown_request_envelope_causes_immediate_exit_without_extra_turn() {
    let ws = unique_workspace("shutdown");
    let agent_id = "agent-shutdown";
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
            if idx == 0 {
                // Parent decided to stop the worker: writes a
                // shutdown_request envelope. Loop must NOT run a
                // second turn.
                agent_mailbox::append_envelope(
                    &ws_cb,
                    agent_id,
                    envelope(kinds::SHUTDOWN_REQUEST, "team-lead", "stop now"),
                )
                .expect("append shutdown");
                Ok(String::from("worker's last words"))
            } else {
                panic!("shutdown_request must not trigger another turn");
            }
        },
    )
    .expect("loop should exit cleanly");

    assert_eq!(turn_count.load(Ordering::SeqCst), 1);
    assert_eq!(final_text, "worker's last words");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn abort_signal_after_turn_exits_without_draining_or_resuming() {
    let ws = unique_workspace("abort-after");
    let agent_id = "agent-abort-after";
    let abort = HookAbortSignal::default();
    let abort_cb = abort.clone();

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
            if idx == 0 {
                // Simulate an out-of-band abort (e.g., TaskStop or
                // shutdown_request via the abort registry) AND ALSO
                // drop a plain-text envelope onto the mailbox to
                // prove abort wins over the envelope drain.
                abort_cb.abort();
                agent_mailbox::append_envelope(
                    &ws_cb,
                    agent_id,
                    envelope(kinds::MESSAGE, "team-lead", "irrelevant continuation"),
                )
                .expect("append envelope");
                Ok(String::from("interrupted mid-work"))
            } else {
                panic!("abort must prevent further turns");
            }
        },
    )
    .expect("loop should exit cleanly on abort");

    assert_eq!(turn_count.load(Ordering::SeqCst), 1);
    assert_eq!(final_text, "interrupted mid-work");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn two_envelopes_between_drains_are_folded_into_one_synth_turn() {
    let ws = unique_workspace("two-envs");
    let agent_id = "agent-two-envs";
    let abort = HookAbortSignal::default();

    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_cb = turn_count.clone();
    let prompts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let prompts_cb = prompts.clone();
    let ws_cb = ws.clone();

    let _ = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("hello"),
        16,
        move |prompt| {
            let idx = turn_count_cb.fetch_add(1, Ordering::SeqCst);
            prompts_cb.lock().unwrap().push(prompt);
            if idx == 0 {
                // Two envelopes between turn 1 and turn 2.
                agent_mailbox::append_envelope(
                    &ws_cb,
                    agent_id,
                    envelope(kinds::MESSAGE, "team-lead", "first follow-up"),
                )
                .expect("append 1");
                agent_mailbox::append_envelope(
                    &ws_cb,
                    agent_id,
                    envelope(kinds::MESSAGE, "team-lead", "second follow-up"),
                )
                .expect("append 2");
            }
            Ok(format!("reply-{idx}"))
        },
    )
    .expect("loop should exit cleanly");

    assert_eq!(
        turn_count.load(Ordering::SeqCst),
        2,
        "two envelopes must collapse into ONE resume turn, not two"
    );
    let prompts = prompts.lock().unwrap();
    // Turn 2's prompt must contain BOTH bodies, in write order.
    let idx_first = prompts[1]
        .find("first follow-up")
        .expect("first body present");
    let idx_second = prompts[1]
        .find("second follow-up")
        .expect("second body present");
    assert!(idx_first < idx_second, "order preserved");

    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn max_multi_turns_cap_prevents_infinite_resume() {
    let ws = unique_workspace("cap");
    let agent_id = "agent-cap";
    let abort = HookAbortSignal::default();

    let turn_count = Arc::new(AtomicUsize::new(0));
    let turn_count_cb = turn_count.clone();
    let ws_cb = ws.clone();

    // A hostile parent that keeps writing envelopes on every drain.
    // With cap=3 the loop must exit after 3 turns even though there
    // are always fresh envelopes waiting.
    let _ = run_multi_turn_loop_for_test(
        agent_id,
        &ws,
        abort,
        String::from("hello"),
        3,
        move |_prompt| {
            let idx = turn_count_cb.fetch_add(1, Ordering::SeqCst);
            agent_mailbox::append_envelope(
                &ws_cb,
                agent_id,
                envelope(kinds::MESSAGE, "spam", &format!("spam #{idx}")),
            )
            .expect("append spam");
            Ok(format!("reply-{idx}"))
        },
    )
    .expect("cap path should not error");

    assert_eq!(
        turn_count.load(Ordering::SeqCst),
        3,
        "cap must limit total resume turns"
    );

    let _ = std::fs::remove_dir_all(&ws);
}
