//! Integration test that proves `<task-notification>` XML blocks
//! deposited by background sub-agents get prepended to the FIRST
//! api-request user message of the NEXT `ConversationRuntime::run_turn`.
//!
//! The wiring lives inside `run_turn_with_blocks` in `conversation.rs`
//! (moved there from the CLI's own `run_turn` so every entry point —
//! CLI REPL, one-shot print, ACP stdio, ACP WebSocket, ACP SDK, MCP
//! servers — inherits it automatically).  This test drives the runtime
//! directly, mimicking what any ACP-hosting server does when sudowork
//! sends a user turn: build a `ConversationRuntime`, call `run_turn`.
//!
//! ## Long-workflow, data-flow chained
//!
//! 1. Set `SUDOCODE_COORDINATOR_MODE=1` for this test (env-mutex
//!    serialised — the coord flag is process-global).
//! 2. `chdir` into a temp workspace so `coordinator_notification::drain`
//!    reads its inbox from there (not the developer's cwd).
//! 3. Emit a synthetic `<task-notification>` envelope via
//!    `coordinator_notification::emit` (the same call site
//!    `persist_agent_terminal_state` uses in production).
//! 4. Build a `ConversationRuntime` with a **capturing** ApiClient that
//!    records the exact `ApiRequest.messages` list on its very first
//!    call, then answers with a trivial completion so `run_turn` finishes.
//! 5. Call `runtime.run_turn("hello")`.
//! 6. **Assert**: the first captured request's LAST user message is a
//!    Text block whose text begins with the emitted XML block AND
//!    contains "hello" at the tail — i.e. drained notifications are
//!    prepended to (not silently substituted for) the incoming user
//!    input.
//!
//! ## Why this test guards a real gap
//!
//! Before the wiring moved into `run_turn_with_blocks`, the CLI-side
//! drain only fired for `CodeCli::run_turn`.  ACP paths (which is what
//! sudowork uses to talk to sudocode) called `runtime.run_turn`
//! directly, bypassing the CLI wrapper.  Under the pre-fix code the
//! captured request WOULD NOT have the XML prefix — the model would
//! never see the notification.  The wiring fix makes this assertion pass;
//! a regression that moves the drain back out would fail it loudly.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use runtime::{
    coordinator_mode::COORDINATOR_ENV_VAR,
    coordinator_notification::{self, COORDINATOR_INBOX_RECIPIENT},
    ApiClient, ApiRequest, AssistantEvent, AssistantEventStream, ContentBlock, ConversationMessage,
    ConversationRuntime, MessageRole, PermissionMode, PermissionPolicy, RuntimeError, Session,
    StaticToolExecutor, SystemPrompt,
};

/// Process-wide env-lock — `COORDINATOR_ENV_VAR` is process-global,
/// so parallel tests that set it would race each other.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn unique_ws(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "coord-push-wire-{label}-{nanos}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("mkdir ws");
    path
}

/// ApiClient that records the messages of every request it receives
/// and returns a trivial one-shot "done" completion so `run_turn` ends
/// quickly.
struct CapturingApiClient {
    captured: Arc<Mutex<Vec<Vec<ConversationMessage>>>>,
}

#[async_trait]
impl ApiClient for CapturingApiClient {
    async fn stream(&mut self, request: ApiRequest) -> Result<AssistantEventStream, RuntimeError> {
        self.captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.messages.clone());
        let events = vec![
            AssistantEvent::TextDelta("done".to_string()),
            AssistantEvent::MessageStop,
        ];
        Ok(events_to_stream(events))
    }
}

fn events_to_stream(events: Vec<AssistantEvent>) -> AssistantEventStream {
    use futures::stream::StreamExt;
    Box::pin(futures::stream::iter(events).map(Ok))
}

#[tokio::test(flavor = "current_thread")]
async fn run_turn_prepends_drained_task_notification_to_first_user_message() {
    let _g = env_lock();
    std::env::set_var(COORDINATOR_ENV_VAR, "1");

    let ws = unique_ws("prepend");
    let prior_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&ws).expect("chdir");

    // Emit — same call the production `persist_agent_terminal_state`
    // makes when a sub-agent completes under coord mode.
    let xml = "<task-notification>\n\
        <task-id>agent-abc</task-id>\n\
        <status>completed</status>\n\
        <summary>Agent \"finder\" completed</summary>\n\
        <result>found the bug at line 42</result>\n\
        </task-notification>";
    coordinator_notification::emit(&ws, "agent-abc", xml).expect("emit ok");

    // Drop a stray sanity file so the mailbox path is verifiable.
    let mailbox = runtime::agent_mailbox::mailbox_path(&ws, COORDINATOR_INBOX_RECIPIENT);
    assert!(
        mailbox.exists(),
        "envelope should be on disk before run_turn"
    );

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = ConversationRuntime::new(
        Session::new(),
        CapturingApiClient {
            captured: captured.clone(),
        },
        StaticToolExecutor::new(),
        PermissionPolicy::new(PermissionMode::ReadOnly),
        SystemPrompt::default(),
    );

    runtime
        .run_turn("hello", None, None)
        .await
        .expect("run_turn should succeed");

    // Restore cwd + env BEFORE assertions so a panic doesn't leak
    // state to the next test.
    std::env::set_current_dir(prior_cwd).ok();
    std::env::remove_var(COORDINATOR_ENV_VAR);

    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1, "one API call expected");
    let first = &requests[0];
    let last_user = first
        .iter()
        .rfind(|m| m.role == MessageRole::User)
        .expect("at least one user message in the request");

    // The last user message's blocks: [drained_prefix, "hello"] OR
    // combined as a single Text block containing both.  Our current
    // `prepend_pending_task_notifications` uses the two-block form so
    // the model sees the notification as a distinct paragraph before
    // the user's actual turn.
    let joined_text: String = last_user
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        joined_text.contains("<task-notification>"),
        "the drained task-notification XML MUST appear in the FIRST \
         request's last user message.  Joined text:\n{joined_text}"
    );
    assert!(
        joined_text.contains("agent-abc"),
        "task-id from the drained envelope MUST survive to the request"
    );
    assert!(
        joined_text.contains("hello"),
        "user's own input MUST still be in the message, not replaced"
    );
    // Ordering: the prefix comes BEFORE the user's own text.
    let notif_pos = joined_text
        .find("<task-notification>")
        .expect("notification present");
    let hello_pos = joined_text.find("hello").expect("hello present");
    assert!(
        notif_pos < hello_pos,
        "task-notification MUST be prepended (position {notif_pos}), \
         not appended (hello at {hello_pos})"
    );

    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test(flavor = "current_thread")]
async fn run_turn_does_not_touch_user_input_when_coord_mode_off() {
    // Fast-path guarantee: non-coord sessions pay 0 for the drain +
    // the user's own input reaches the model untouched.
    let _g = env_lock();
    std::env::remove_var(COORDINATOR_ENV_VAR);

    let ws = unique_ws("noop");
    let prior_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&ws).expect("chdir");

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = ConversationRuntime::new(
        Session::new(),
        CapturingApiClient {
            captured: captured.clone(),
        },
        StaticToolExecutor::new(),
        PermissionPolicy::new(PermissionMode::ReadOnly),
        SystemPrompt::default(),
    );

    runtime
        .run_turn("hello world", None, None)
        .await
        .expect("run_turn ok");

    std::env::set_current_dir(prior_cwd).ok();

    let requests = captured.lock().unwrap();
    let joined: String = requests[0]
        .iter()
        .rfind(|m| m.role == MessageRole::User)
        .expect("user msg")
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("<task-notification>"),
        "non-coord mode MUST NOT inject anything"
    );
    assert!(joined.contains("hello world"));

    // Sanity: no mailbox file should have been created.
    let mailbox = runtime::agent_mailbox::mailbox_path(&ws, COORDINATOR_INBOX_RECIPIENT);
    assert!(!mailbox.exists());

    let _ = std::fs::remove_dir_all(&ws);
}
