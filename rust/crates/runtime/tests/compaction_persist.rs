//! FLAG B regression: a compacted session's summary must survive persist + reload.
//!
//! The ACP pre-turn auto-compaction persists the compacted session immediately
//! (before the still-over-limit early return) so a resumed long conversation
//! carries the compacted *summary* rather than the full uncompacted history that
//! would just re-overflow. This exercises the underlying
//! `compact_session -> save_to_path -> load_from_path` chain end to end — the
//! part `persists_compaction_metadata` (which records compaction directly via
//! `record_compaction_with_usage`) does not cover.

use std::fs;

use runtime::{
    compact_session, CompactionConfig, ContentBlock, ConversationMessage, MessageRole, Session,
};

fn temp_path(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "compaction-persist-{label}-{}-{nanos}.jsonl",
        std::process::id()
    ))
}

#[test]
fn compacted_summary_survives_save_and_reload() {
    // given — a session with enough turns that compaction has something to remove.
    let mut session = Session::new();
    for i in 0..8 {
        session
            .push_user_text(format!("user turn {i}"))
            .expect("push user message");
        session
            .push_message(ConversationMessage::assistant(vec![ContentBlock::Text {
                text: format!("assistant reply {i}"),
            }]))
            .expect("push assistant message");
    }
    let original_len = session.messages.len();

    // when — force compaction, keeping only the last 2 messages.
    let result = compact_session(
        &session,
        CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 0,
        },
    );
    assert!(
        result.removed_message_count > 0,
        "compaction should remove messages"
    );
    let compacted = &result.compacted_session;
    assert!(
        compacted.messages.len() < original_len,
        "compacted session ({}) should be smaller than original ({original_len})",
        compacted.messages.len()
    );
    // The summary is injected as the first message with System role.
    assert!(
        matches!(compacted.messages[0].role, MessageRole::System),
        "summary must be the first (System) message"
    );

    // and — after save + reload, the compaction record AND the summary survive.
    let path = temp_path("roundtrip");
    compacted
        .save_to_path(&path)
        .expect("compacted session should save");
    let restored = Session::load_from_path(&path).expect("compacted session should reload");
    let _ = fs::remove_file(&path);

    assert!(
        restored.compaction.is_some(),
        "compaction record must persist across reload"
    );
    assert!(
        matches!(restored.messages[0].role, MessageRole::System),
        "summary System message must survive reload"
    );
    assert_eq!(
        restored.messages.len(),
        compacted.messages.len(),
        "reloaded message count must match the compacted session"
    );
}
