//! `send` must reach the peer no matter how the model spells or wraps
//! the call.
//!
//! This is the regression guard for a live failure. Three tools were
//! advertised at once — `SendMessage`, `send`, `send` — differing only
//! in which schema field carried the text, and only the literal name
//! `send` was intercepted for nexus A2A. A model on a cross-machine
//! session picked `send`, its reply was written to a local
//! `.sudocode-inbox/` file, the tool answered "Message sent to <peer>'s
//! inbox", and the peer on the other machine waited ten hours for a message
//! that had never left the host.
//!
//! Two independent things had to be true for that to happen, so both are
//! asserted here:
//!
//! * the intercept matched the RAW name, so any other spelling fell through;
//! * `ExecuteExtraTool` ran a SECOND dispatch path that knew nothing about
//!   A2A — and since `send` is a deferred tool, the envelope is its
//!   documented invocation, so even the correct name routed locally.
//!
//! The fake sender stands in for the gRPC one. The routing decision is made
//! entirely by `CliToolExecutor` before any transport is touched, so a daemon
//! would only slow the test down without testing more of the decision.

use std::sync::{Arc, Mutex};

use engine_host::tool_executor::CliToolExecutor;
use runtime::{ToolDispatchContext, ToolExecutor};
use tools::GlobalToolRegistry;

/// Records what the A2A transport was asked to deliver.
type Delivered = Arc<Mutex<Vec<(String, String)>>>;

fn executor_with_a2a() -> (CliToolExecutor, Delivered) {
    let delivered: Delivered = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&delivered);
    let mut executor = CliToolExecutor::new(None, GlobalToolRegistry::builtin(), None);
    executor.set_mailbox_sender(Arc::new(move |to: &str, body: &str| {
        sink.lock()
            .expect("sender sink poisoned")
            .push((to.to_string(), body.to_string()));
        Ok(())
    }));
    (executor, delivered)
}

/// Run one tool call to completion on a throwaway runtime.
fn call(executor: &CliToolExecutor, tool: &str, input: &str) -> Result<String, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
        .block_on(async {
            executor
                .execute_with_context(tool, input, &ToolDispatchContext::default())
                .await
                .map_err(|e| e.to_string())
        })
}

/// Every way a model can name, shape or wrap the call reaches the peer.
///
/// `SendMessage` is CC's spelling and `send_message` the A2A tool this
/// replaced — a model reaches for either by habit. `body` is the field the old
/// A2A schema used, and the field the wire envelope still uses, so a model
/// that has seen either will reach for it too. The `ExecuteExtraTool` envelope
/// is the documented way to invoke a deferred tool, which `send` is.
///
/// Not one of these may quietly become a local file write: the name, the field
/// and the wrapper are all things a model picks, and none of them is allowed
/// to decide whether a message crosses the machine.
#[test]
fn every_spelling_shape_and_wrapper_reaches_the_peer() {
    for (tool, input) in [
        ("send", r#"{"to":"mac-ai","message":"canonical name"}"#),
        ("SendMessage", r#"{"to":"mac-ai","message":"CC spelling"}"#),
        (
            "send_message",
            r#"{"to":"mac-ai","message":"superseded A2A name"}"#,
        ),
        // The old A2A input shape, field and all.
        ("send_message", r#"{"to":"mac-ai","body":"body field"}"#),
        ("send", r#"{"to":"mac-ai","body":"body field, new name"}"#),
        (
            "ExecuteExtraTool",
            r#"{"tool_name":"send","params":{"to":"mac-ai","message":"deferred envelope"}}"#,
        ),
        (
            "ExecuteExtraTool",
            r#"{"tool_name":"SendMessage","params":{"to":"mac-ai","message":"envelope, CC spelling"}}"#,
        ),
        (
            "ExecuteExtraTool",
            r#"{"tool_name":"send_message","params":{"to":"mac-ai","body":"envelope, old name and field"}}"#,
        ),
    ] {
        let (executor, delivered) = executor_with_a2a();
        let result =
            call(&executor, tool, input).unwrap_or_else(|e| panic!("`{tool}` must not fail: {e}"));

        let sent = delivered.lock().expect("sink poisoned").clone();
        assert_eq!(
            sent.len(),
            1,
            "`{tool}` must hand exactly one message to the A2A transport, got {sent:?}"
        );
        assert_eq!(sent[0].0, "mac-ai", "`{tool}` addressed the wrong peer");

        // The result string is what a human reads to decide whether a message
        // crossed the machine. "delivered to" is the network path; anything
        // mentioning an inbox file means it did not leave the host.
        assert!(
            result.contains("message delivered to mac-ai"),
            "`{tool}` must report network delivery, got: {result}"
        );
        assert!(
            !result.contains("inbox"),
            "`{tool}` reported a workspace-mailbox write while A2A was configured: {result}"
        );
    }
}

/// The body travels intact — a wrapper that reached the transport with an
/// empty or wrong-field message would pass every assertion above while
/// delivering nothing a peer could read.
#[test]
fn the_deferred_envelope_carries_the_body_through() {
    let (executor, delivered) = executor_with_a2a();
    call(
        &executor,
        "ExecuteExtraTool",
        r#"{"tool_name":"send","params":{"to":"mac-ai","message":"PING from win-ai"}}"#,
    )
    .expect("envelope send must succeed");

    let sent = delivered.lock().expect("sink poisoned").clone();
    assert_eq!(
        sent,
        vec![("mac-ai".to_string(), "PING from win-ai".to_string())],
        "the envelope must deliver the message verbatim"
    );
}

/// Without an A2A sender the same tool delivers locally. This is the contract,
/// not a fallback: one tool, and the host chooses the destination. If this
/// starts erroring, a plain workspace session has lost its mailbox.
#[test]
fn without_a2a_the_same_tool_delivers_to_the_workspace_mailbox() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = CliToolExecutor::new(None, GlobalToolRegistry::builtin(), None);
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("chdir into workspace");

    let result = call(
        &executor,
        "send",
        r#"{"to":"worker","message":"local hand-off","summary":"local hand-off"}"#,
    );

    std::env::set_current_dir(previous).expect("restore cwd");
    let result = result.expect("workspace delivery must succeed");
    assert!(
        result.contains("inbox"),
        "without A2A the tool must report a workspace-mailbox write, got: {result}"
    );
}

/// `ExecuteExtraTool` must not be a hole in `--allowedTools`.
///
/// The envelope used to pass the gate as itself and then dispatch whatever it
/// named, so an allow-list of read-only tools still let a model run anything
/// deferred by wrapping it.
#[test]
fn the_deferred_envelope_is_gated_by_the_allow_list() {
    let allowed = GlobalToolRegistry::builtin()
        .normalize_allowed_tools(&["ExecuteExtraTool".to_string(), "ToolSearch".to_string()])
        .expect("allow-list parses")
        .expect("allow-list is non-empty");
    let executor = CliToolExecutor::new(Some(allowed), GlobalToolRegistry::builtin(), None);

    let error = call(
        &executor,
        "ExecuteExtraTool",
        r#"{"tool_name":"send","params":{"to":"worker","message":"smuggled"}}"#,
    )
    .expect_err("a tool absent from --allowedTools must be refused inside the envelope too");
    assert!(
        error.contains("not enabled by the current --allowedTools setting"),
        "the refusal must name the allow-list, got: {error}"
    );
}
