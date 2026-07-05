//! Integration test for the fork subagent's runtime primitives:
//! [`ToolDispatchContext::is_inside_fork_child`] and the
//! [`FORK_BOILERPLATE_TAG`] SSOT.
//!
//! Complements `session_pre_seed.rs` (the [`Session::with_messages`]
//! plumbing test) — together they exercise every runtime-crate piece
//! the fork subagent rebuild in the tools crate depends on, without
//! spawning a PTY, without hitting an LLM, and without going through
//! the mock harness (which has known scenario-inheritance limitations
//! for subagent-spawning tests).
//!
//! What's NOT here: the tools-crate wiring that reads
//! `is_inside_fork_child` and returns the "recursive fork detected"
//! error. That's a `prepare_agent_job` behavior; it's exercised
//! end-to-end via live-mode PTY tests scheduled for the fork feature's
//! next iteration once the mock-harness scenario-inheritance gap is
//! resolved. The current integration tests still lock in the
//! runtime-side contract that path depends on.

use runtime::{
    ContentBlock, ConversationMessage, MessageRole, ToolDispatchContext, FORK_BOILERPLATE_TAG,
};

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

fn assistant_tool_use(id: &str, name: &str, input: &str) -> ConversationMessage {
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

// ── FORK_BOILERPLATE_TAG SSOT ───────────────────────────────────────

/// The tag string is exported from runtime and re-used verbatim by
/// `tools::build_fork_child_message` when it wraps a directive. Any
/// change to the string here must stay byte-identical to the writer
/// in tools — otherwise the recursion guard's scan misses fresh fork
/// children.
#[test]
fn fork_boilerplate_tag_is_stable() {
    assert_eq!(FORK_BOILERPLATE_TAG, "fork-boilerplate");
}

// ── is_inside_fork_child ────────────────────────────────────────────

#[test]
fn empty_context_is_not_inside_fork_child() {
    let ctx = ToolDispatchContext::default();
    assert!(!ctx.is_inside_fork_child());
}

#[test]
fn assistant_only_history_is_not_inside_fork_child() {
    // Assistant messages don't count — the boilerplate lives in a
    // user-role message inside a fork child's session.
    let ctx = ToolDispatchContext {
        parent_assistant_message: None,
        parent_session_messages: vec![
            assistant_text("hi"),
            assistant_text(&format!("<{FORK_BOILERPLATE_TAG}> ignore me")),
        ],
    };
    assert!(!ctx.is_inside_fork_child());
}

#[test]
fn user_without_boilerplate_is_not_inside_fork_child() {
    let ctx = ToolDispatchContext {
        parent_assistant_message: None,
        parent_session_messages: vec![
            user_text("please spawn a fork subagent"),
            assistant_text("Sure."),
        ],
    };
    assert!(!ctx.is_inside_fork_child());
}

#[test]
fn user_with_boilerplate_tag_is_inside_fork_child() {
    let seeded_directive = format!(
        "<{FORK_BOILERPLATE_TAG}>\nRULES...\n</{FORK_BOILERPLATE_TAG}>\nYour directive: read a.txt"
    );
    let ctx = ToolDispatchContext {
        parent_assistant_message: None,
        parent_session_messages: vec![user_text(&seeded_directive)],
    };
    assert!(ctx.is_inside_fork_child());
}

#[test]
fn boilerplate_in_any_prior_user_message_is_detected() {
    // Even if the fork boilerplate is buried deep in the history
    // (parent had many turns before spawning this fork child), the
    // scan MUST still catch it — a fork child's session prefix
    // survives across turns.
    let boilerplate_message = user_text(&format!("<{FORK_BOILERPLATE_TAG}> stub"));
    let ctx = ToolDispatchContext {
        parent_assistant_message: None,
        parent_session_messages: vec![
            boilerplate_message,
            assistant_text("did stuff"),
            user_text("do more"),
            assistant_text("more stuff"),
            user_text("keep going"),
        ],
    };
    assert!(ctx.is_inside_fork_child());
}

// ── parent_assistant_message ────────────────────────────────────────

/// When the parent's just-emitted assistant message is available, the
/// fork spawn path clones its tool_use blocks into the child's
/// inherited-messages prefix. Assert the accessor returns exactly
/// what the runtime tool loop stuffed into it.
#[test]
fn parent_assistant_message_is_preserved_verbatim() {
    let parent = assistant_tool_use("tu_abc", "Agent", r#"{"description":"child"}"#);
    let ctx = ToolDispatchContext {
        parent_assistant_message: Some(parent.clone()),
        parent_session_messages: vec![],
    };
    assert_eq!(ctx.parent_assistant_message.as_ref(), Some(&parent));
}

#[test]
fn default_context_has_no_parent_assistant_message() {
    let ctx = ToolDispatchContext::default();
    assert!(ctx.parent_assistant_message.is_none());
    assert!(ctx.parent_session_messages.is_empty());
}
