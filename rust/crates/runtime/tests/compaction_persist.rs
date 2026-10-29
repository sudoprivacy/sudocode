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
    compact_session_sync, CompactionConfig, ContentBlock, ConversationMessage, MessageRole, Session,
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
    let original_len = session.messages().len();

    // when — force compaction, keeping only the last 2 messages.
    let result = compact_session_sync(
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
        compacted.messages().len() < original_len,
        "compacted session ({}) should be smaller than original ({original_len})",
        compacted.messages().len()
    );
    // The summary is injected as the first message with System role.
    assert!(
        matches!(compacted.messages()[0].role, MessageRole::System),
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
        matches!(restored.messages()[0].role, MessageRole::System),
        "summary System message must survive reload"
    );
    assert_eq!(
        restored.messages().len(),
        compacted.messages().len(),
        "reloaded message count must match the compacted session"
    );
}

#[test]
fn deletion_fences_compaction_and_late_transcript_writes() {
    let cwd = temp_path("deleted").with_extension("");
    fs::create_dir(&cwd).unwrap();
    let path = cwd.join("transcript.jsonl");
    let store = runtime::SessionStore::from_cwd(&cwd).unwrap();
    let mut session = Session::new()
        .with_workspace_root(cwd.clone())
        .with_persistence_path(path.clone());
    session.push_user_text("preserve original bytes").unwrap();
    let journal =
        runtime::subagent_completion::CompletionJournal::open(&session, &cwd, true).unwrap();
    session.save_compacted_to_path(&session, &path).unwrap();
    session
        .save_thinking_repaired_to_path_checked(&session, &path, None)
        .unwrap();
    let backups: Vec<_> = fs::read_dir(&cwd)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("transcript.jsonl.before-")
        })
        .collect();
    assert_eq!(
        backups.len(),
        2,
        "both real replacement operations preserved their source"
    );
    let original = fs::read(&path).unwrap();
    let compacted = session.clone();
    journal.purge(&journal.storage_ref(), "cleanup").unwrap();
    assert!(compacted
        .save_compacted_to_path(&session, &path)
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    assert!(session
        .rewrite_persisted()
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    assert!(session
        .persist_maintenance_metadata()
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    assert!(session
        .offload_tool_result("late", b"do not recreate")
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    assert_eq!(fs::read(&path).unwrap(), original);
    drop(journal);
    drop(session);
    drop(compacted);
    assert!(Session::load_from_path(&path)
        .unwrap_err()
        .to_string()
        .contains("session_deleted"));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while path.exists()
        || fs::read_dir(store.sessions_dir().join(".cleanup"))
            .unwrap()
            .next()
            .is_some()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "Session cleanup did not retire"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(path.with_extension("completions.json").exists());
    for backup in backups {
        assert!(!backup.exists());
        assert!(Session::load_from_path(&backup)
            .unwrap_err()
            .to_string()
            .contains("session_deleted"));
    }
    fs::remove_dir_all(cwd).unwrap();
}
