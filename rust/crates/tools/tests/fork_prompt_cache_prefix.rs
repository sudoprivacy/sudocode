//! Integration test for prompt-cache prefix sharing across N fork
//! sub-agents (Commit 14).
//!
//! ## Why this matters
//!
//! When a parent spawns 3+ fork children in parallel, each child
//! immediately sends a `/v1/messages` request whose message list is:
//!
//!   [full_parent_assistant_message,
//!    user_message = [tool_result_1, tool_result_2, …, directive_text]]
//!
//! The parent assistant message + all tool_result blocks are meant
//! to be BYTE-IDENTICAL across every fork so the Anthropic prompt
//! cache treats them as one shared prefix — only the final
//! directive text (which carries the child-specific instructions)
//! should differ. This can turn a triple-cost 3x parallel fan-out
//! into a single-prefix + 3x tail, dropping the effective input
//! cost dramatically.
//!
//! ## What this test proves (long-workflow chain)
//!
//! For 3 forks (a/b/c) with DIFFERENT directives but the SAME
//! parent assistant message:
//!
//! 1. Message 0 (assistant) is byte-identical across a, b, c.
//! 2. Message 1's ToolResult blocks are byte-identical across
//!    a, b, c — same tool_use_id, same tool_name, same placeholder
//!    output, same is_error flag.
//! 3. Message 1's trailing Text block DIFFERS across a, b, c
//!    (this is the only block that carries child-specific content).
//! 4. There is exactly ONE Text block in each user message — a
//!    regression here would break the shape.
//!
//! Not a PTY live test — the plan explicitly de-scopes this because
//! the sharing property is structural, not behavioural.

use runtime::{ContentBlock, ConversationMessage, MessageRole};
use tools::testing::build_forked_messages_for_test;

fn parent_assistant_with_two_tool_uses() -> ConversationMessage {
    ConversationMessage {
        role: MessageRole::Assistant,
        blocks: vec![
            ContentBlock::Text {
                text: "I'll delegate two parallel explorations.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "toolu_001".to_string(),
                name: "Agent".to_string(),
                input: r#"{"description":"read a.txt","prompt":"read a.txt","subagent_type":"fork"}"#
                    .to_string(),
                thought_signature: None,
            },
            ContentBlock::ToolUse {
                id: "toolu_002".to_string(),
                name: "Agent".to_string(),
                input: r#"{"description":"read b.txt","prompt":"read b.txt","subagent_type":"fork"}"#
                    .to_string(),
                thought_signature: None,
            },
        ],
        usage: None,
        model: Some("claude-opus-4-8".to_string()),
    }
}

/// Slot a `Vec<ConversationMessage>` into a shape a test can slice
/// per-block: (parent_assistant_message, user_message_blocks).
fn split(messages: &[ConversationMessage]) -> (&ConversationMessage, Vec<&ContentBlock>) {
    assert_eq!(messages.len(), 2, "fork must produce 2 messages");
    let assistant = &messages[0];
    let user_blocks: Vec<&ContentBlock> = messages[1].blocks.iter().collect();
    (assistant, user_blocks)
}

#[test]
fn parallel_forks_share_parent_assistant_message_byte_identical() {
    let parent = parent_assistant_with_two_tool_uses();

    let a = build_forked_messages_for_test("read a.txt", &parent);
    let b = build_forked_messages_for_test("read b.txt", &parent);
    let c = build_forked_messages_for_test("read c.txt", &parent);

    let (a_assist, _) = split(&a);
    let (b_assist, _) = split(&b);
    let (c_assist, _) = split(&c);

    assert_eq!(a_assist, b_assist, "parent assistant differs a vs b");
    assert_eq!(b_assist, c_assist, "parent assistant differs b vs c");
    // Sanity: it really is the parent assistant we passed in.
    assert_eq!(a_assist, &parent);
}

#[test]
fn parallel_forks_share_user_tool_result_blocks_byte_identical() {
    let parent = parent_assistant_with_two_tool_uses();

    let a = build_forked_messages_for_test("read a.txt", &parent);
    let b = build_forked_messages_for_test("read b.txt", &parent);
    let c = build_forked_messages_for_test("read c.txt", &parent);

    let (_, a_blocks) = split(&a);
    let (_, b_blocks) = split(&b);
    let (_, c_blocks) = split(&c);

    // Each user message = [tool_result_1, tool_result_2, text]
    assert_eq!(a_blocks.len(), 3);
    assert_eq!(b_blocks.len(), 3);
    assert_eq!(c_blocks.len(), 3);

    for i in 0..2 {
        assert_eq!(a_blocks[i], b_blocks[i], "tool_result block {i} differs a vs b");
        assert_eq!(b_blocks[i], c_blocks[i], "tool_result block {i} differs b vs c");
        // Sanity: it IS a ToolResult and carries the shared placeholder.
        match a_blocks[i] {
            ContentBlock::ToolResult { output, is_error, .. } => {
                assert!(!is_error);
                assert!(
                    output.contains("Fork started"),
                    "placeholder must be shared across forks"
                );
            }
            other => panic!("expected ToolResult in position {i}, got {other:?}"),
        }
    }
}

#[test]
fn parallel_forks_differ_only_in_trailing_directive_text_block() {
    let parent = parent_assistant_with_two_tool_uses();

    let a = build_forked_messages_for_test("read a.txt", &parent);
    let b = build_forked_messages_for_test("read b.txt", &parent);
    let c = build_forked_messages_for_test("read c.txt", &parent);

    let (_, a_blocks) = split(&a);
    let (_, b_blocks) = split(&b);
    let (_, c_blocks) = split(&c);

    let a_last = a_blocks[2];
    let b_last = b_blocks[2];
    let c_last = c_blocks[2];

    let (a_text, b_text, c_text) = match (a_last, b_last, c_last) {
        (
            ContentBlock::Text { text: at },
            ContentBlock::Text { text: bt },
            ContentBlock::Text { text: ct },
        ) => (at, bt, ct),
        _ => panic!("trailing block must be Text (the fork directive)"),
    };

    // The 3 directives must produce 3 DIFFERENT trailing texts.
    assert_ne!(a_text, b_text, "a and b should carry different directives");
    assert_ne!(b_text, c_text, "b and c should carry different directives");
    assert_ne!(a_text, c_text, "a and c should carry different directives");

    // Each must actually contain its child-specific instruction.
    assert!(a_text.contains("read a.txt"));
    assert!(b_text.contains("read b.txt"));
    assert!(c_text.contains("read c.txt"));
}

#[test]
fn fork_with_zero_parent_tool_uses_still_produces_shared_prefix_shape() {
    // A parent assistant that hadn't yet issued tool_uses when the
    // fork spawned still must produce a byte-identical message list
    // EXCEPT for the directive text. This is the degenerate case
    // build_forked_messages handles by returning just a
    // user_text message.
    let parent_no_tools = ConversationMessage {
        role: MessageRole::Assistant,
        blocks: vec![ContentBlock::Text {
            text: "Preparing to delegate…".to_string(),
        }],
        usage: None,
        model: Some("claude-opus-4-8".to_string()),
    };

    let a = build_forked_messages_for_test("directive-a", &parent_no_tools);
    let b = build_forked_messages_for_test("directive-b", &parent_no_tools);

    assert_eq!(a.len(), 1, "no-tool-use parent -> single user message");
    assert_eq!(b.len(), 1);
    // The two messages MUST differ (different directives), but
    // must both be role=User with a single Text block.
    assert_ne!(a[0], b[0]);
    assert_eq!(a[0].role, MessageRole::User);
    assert_eq!(b[0].role, MessageRole::User);
    assert_eq!(a[0].blocks.len(), 1);
    assert_eq!(b[0].blocks.len(), 1);
}
