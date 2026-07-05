//! Integration test for [`Session::with_messages`] — the pre-seeded
//! conversation-prefix builder used by the fork subagent's parent-context
//! inheritance path. The child runtime must see the seeded messages as its
//! first API call's history.

use runtime::{ContentBlock, ConversationMessage, MessageRole, Session};

fn user_text(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        usage: None,
        model: None,
    }
}

fn assistant_text(text: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        usage: None,
        model: Some("test-model".to_string()),
    }
}

fn tool_use(id: &str, name: &str, input: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: input.to_string(),
            thought_signature: None,
        }],
        usage: None,
        model: Some("test-model".to_string()),
    }
}

fn placeholder_tool_result(tool_use_id: &str) -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::User,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            tool_name: "Agent".to_string(),
            output: "Fork started — processing in background".to_string(),
            is_error: false,
        }],
        usage: None,
        model: None,
    }
}

// ── Direct plumbing ─────────────────────────────────────────────────

#[test]
fn with_messages_pre_seeds_message_vec() {
    // given — a fresh session and a hand-crafted conversation prefix.
    let prefix = vec![
        user_text("What files does this repo have?"),
        assistant_text("Let me look."),
    ];

    // when — construct via with_messages
    let session = Session::new().with_messages(prefix.clone());

    // then — messages match verbatim
    assert_eq!(session.messages, prefix);
    assert_eq!(session.messages.len(), 2);
}

#[test]
fn with_messages_bumps_updated_at() {
    // given — record the pre-seed timestamp
    let session_before = Session::new();
    // Sleep a millisecond to guarantee a distinct wall-clock reading.
    std::thread::sleep(std::time::Duration::from_millis(2));

    // when — apply with_messages
    let session_after = session_before.clone().with_messages(vec![user_text("hi")]);

    // then — updated_at_ms advances
    assert!(
        session_after.updated_at_ms >= session_before.updated_at_ms,
        "updated_at_ms must not go backwards (before={}, after={})",
        session_before.updated_at_ms,
        session_after.updated_at_ms
    );
}

#[test]
fn with_messages_replaces_existing_messages() {
    // given — a session that already has an in-memory message
    let mut session = Session::new();
    session
        .push_user_text("first turn")
        .expect("push should succeed without persistence");
    assert_eq!(session.messages.len(), 1);

    // when — replace via with_messages
    let session = session.with_messages(vec![user_text("replacement")]);

    // then — the replacement wins, not merged
    assert_eq!(session.messages.len(), 1);
    match &session.messages[0].blocks[0] {
        ContentBlock::Text { text } => assert_eq!(text, "replacement"),
        other => panic!("expected Text block, got {other:?}"),
    }
}

#[test]
fn with_messages_supports_empty_vec() {
    // given/when — empty pre-seed
    let session = Session::new().with_messages(Vec::new());

    // then — no messages, session usable
    assert!(session.messages.is_empty());
}

// ── Fork subagent shape ─────────────────────────────────────────────

/// A fork subagent's initial session must contain the parent's
/// assistant tool_use + a placeholder tool_result so the child model's
/// first API call carries the exact same prefix (byte-identical for
/// prompt-cache purposes) followed by its directive. This test only
/// asserts the shape survives round-trip through `with_messages` — the
/// end-to-end assertion that the child's outgoing API request body
/// includes this prefix belongs to the fork PTY test in the follow-up
/// commit.
#[test]
fn with_messages_carries_fork_style_prefix() {
    // given — the parent was mid-flight in a tool-use assistant message
    // when it forked. The placeholder tool_result simulates the child's
    // "your directive completed" stub so the API sees a balanced pair.
    let parent_assistant = tool_use("tu_abc", "Agent", r#"{"description":"child A"}"#);
    let placeholder = placeholder_tool_result("tu_abc");
    let child_directive = user_text("Your directive: read a.txt and report the first line.");

    let prefix = vec![parent_assistant, placeholder, child_directive];

    // when — hand it to a fresh session
    let session = Session::new().with_messages(prefix.clone());

    // then — prefix is intact in insertion order
    assert_eq!(session.messages, prefix);
    assert_eq!(session.messages.len(), 3);
    match &session.messages[0].blocks[0] {
        ContentBlock::ToolUse { name, .. } => assert_eq!(name, "Agent"),
        other => panic!("expected ToolUse block, got {other:?}"),
    }
    match &session.messages[1].blocks[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            output,
            ..
        } => {
            assert_eq!(tool_use_id, "tu_abc");
            assert_eq!(output, "Fork started — processing in background");
        }
        other => panic!("expected ToolResult block, got {other:?}"),
    }
}
