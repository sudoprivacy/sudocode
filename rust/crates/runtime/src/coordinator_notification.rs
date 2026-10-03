//! Coordinator-mode push notification queue.
//!
//! Ports CC-fork's task-notification queue pattern (see the design
//! research summary in `notes/plans/subagent-cc-fork-parity.md`
//! §9.9):
//!
//! - When a sub-agent reaches terminal state under
//!   [`crate::coordinator_mode::is_coordinator_mode`] the tools crate
//!   calls [`emit`] to append a `<task-notification>` XML block to
//!   the coordinator's inbox (an append-only JSONL file at
//!   `<workspace>/.sudocode-inbox/coordinator.jsonl`).
//! - Between coordinator user turns the REPL calls [`drain`] to
//!   read every envelope that has arrived since the previous drain,
//!   batch them into a single follow-up prompt prefix, and prepend
//!   that to the user's next input.
//! - A sidecar consumed-count file
//!   (`<workspace>/.sudocode-inbox/coordinator.consumed`) tracks the
//!   next unread index so drains are idempotent — a crashing REPL
//!   won't lose notifications and a re-drain won't double-inject.
//!
//! ## Coordinator inbox recipient
//!
//! The well-known recipient string [`COORDINATOR_INBOX_RECIPIENT`] is
//! `coordinator`. Sub-agents write to this recipient via the same
//! [`crate::agent_mailbox::append_envelope`] path `send` uses,
//! so the wire format is unchanged.
//!
//! ## Non-coordinator sessions
//!
//! When [`crate::coordinator_mode::is_coordinator_mode`] is off,
//! [`emit`] is a no-op (returns `Ok(())`) and [`drain`] returns an
//! empty vec. Callers can safely invoke both without guarding.

use std::path::Path;

use crate::agent_mailbox::{kinds, MailboxEnvelope};
use crate::mailbox::Mailbox;

/// Well-known recipient string for the coordinator's inbox.
pub const COORDINATOR_INBOX_RECIPIENT: &str = "coordinator";

/// Sidecar filename that stores the next-unread index. Kept next to
/// the JSONL file so a `git-ignore`-friendly cleanup that removes
/// `.sudocode-inbox/` gets both together.
const CONSUMED_OFFSET_FILE: &str = "coordinator.consumed";

/// Append a rendered `<task-notification>` XML block to the
/// coordinator's inbox. Silently no-ops when coordinator mode is
/// off — the tools crate calls this unconditionally from
/// terminal-state, and the coord-mode check lives here so the
/// caller doesn't have to remember.
///
/// `from` should be the sub-agent's `agent_id` so an eventual
/// debugger can trace which agent generated the notification.
///
/// # Errors
///
/// Returns a `String` error only when the mailbox directory can't
/// be created or the JSONL append fails.
pub fn emit(workspace_root: &Path, from: &str, task_notification_xml: &str) -> Result<(), String> {
    if !crate::coordinator_mode::is_coordinator_mode() {
        return Ok(());
    }
    let envelope = MailboxEnvelope {
        from: from.to_string(),
        to: COORDINATOR_INBOX_RECIPIENT.to_string(),
        body: task_notification_xml.to_string(),
        summary: None,
        timestamp: 0, // filled by append_envelope
        color: None,
        kind: kinds::TASK_NOTIFICATION.to_string(),
        request_id: None,
    };
    // Through the mailbox, like any other message. This wrote straight to
    // `{ws}/agents/coordinator/chat-with-me` — its own path, its own reader,
    // its own consumed-offset file — which made the coordinator queue the last
    // user of a shape nothing else addressed any more.
    Mailbox::workspace_local(workspace_root, from.to_string()).send(envelope)
}

/// Drain all `<task-notification>` envelopes that arrived since the
/// previous drain, in FIFO order. Advances the consumed-count
/// sidecar so a subsequent drain returns only newer envelopes.
///
/// Returns rendered XML strings — callers just concatenate them
/// (typically with a blank line between blocks) and prepend the
/// result to the coordinator's next user prompt.
///
/// Non-`task_notification` envelopes in the inbox (e.g., a
/// hypothetical future send_message-to-coordinator flow) are skipped
/// but ALSO count toward the consumed offset, so mixing kinds in
/// the same file is safe.
///
/// # Errors
///
/// Returns a `String` error only when the mailbox exists but
/// can't be read (permissions, IO). A missing inbox / missing
/// sidecar are treated as "nothing to drain."
pub fn drain(workspace_root: &Path) -> Result<Vec<String>, String> {
    // Fast-path exit under non-coord sessions — spares the stat()
    // that `read_all` would otherwise do every user turn. The read
    // is cheap but this hook fires per user prompt on every scode
    // session in the world, so the "0 cost when off" invariant
    // matters.
    if !crate::coordinator_mode::is_coordinator_mode() {
        return Ok(Vec::new());
    }
    let mailbox = Mailbox::workspace_local(workspace_root, COORDINATOR_INBOX_RECIPIENT.to_string());
    // One transcript per sub-agent rather than one inbox for all of them, so
    // the drain walks the chat list. `take_unread` advances each conversation's
    // own recorded position, which is what the consumed-offset sidecar used to
    // do for the single file.
    let mut batch: Vec<(u64, String)> = Vec::new();
    for peer in mailbox.list_conversations()? {
        for envelope in mailbox.take_unread(&peer)? {
            if envelope.kind == kinds::TASK_NOTIFICATION {
                batch.push((envelope.timestamp, envelope.body));
            }
        }
    }
    // Ordered by send time. There is no single log to read in order any more,
    // and a batch that arrives in chat-list order would read as whichever
    // sub-agent happens to sort first rather than as what happened when.
    batch.sort_by_key(|(sent_at, _)| *sent_at);
    Ok(batch.into_iter().map(|(_, body)| body).collect())
}

/// Format a batch of drained task-notifications into a single prompt
/// prefix ready to prepend to the user's next input. Mirrors CC's
/// `queued_command` batching (all same-mode items into one turn).
///
/// The prefix ends with a blank line separator before the caller's
/// user-supplied text, matching the coordinator prompt's example
/// that shows a fresh user turn following each `<task-notification>`
/// block.
#[must_use]
pub fn format_drain_batch(notifications: &[String]) -> String {
    if notifications.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(notifications.iter().map(String::len).sum::<usize>() + 16);
    for (i, n) in notifications.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(n);
    }
    out.push_str("\n\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unique_ws(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "coord-notif-{label}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn enable_coord() -> std::sync::MutexGuard<'static, ()> {
        let g = guard();
        std::env::set_var(crate::coordinator_mode::COORDINATOR_ENV_VAR, "1");
        g
    }

    fn disable_coord() {
        std::env::remove_var(crate::coordinator_mode::COORDINATOR_ENV_VAR);
    }

    #[test]
    fn emit_is_noop_when_coord_mode_off() {
        let _g = guard();
        disable_coord();
        let ws = unique_ws("emit-noop");
        emit(&ws, "agent-x", "<task-notification>x</task-notification>").expect("ok");
        let transcript = std::path::PathBuf::from(
            crate::mailbox::InboxConvention::new(ws.to_string_lossy().into_owned())
                .transcript_path("agent-x", COORDINATOR_INBOX_RECIPIENT),
        );
        assert!(
            !transcript.exists(),
            "no transcript created when coord mode is off, expected none at {}",
            transcript.display()
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn emit_appends_and_drain_returns_batch_then_empties() {
        let _g = enable_coord();
        let ws = unique_ws("emit-drain");
        emit(&ws, "agent-a", "<task-notification>A</task-notification>").unwrap();
        emit(&ws, "agent-b", "<task-notification>B</task-notification>").unwrap();

        let batch = drain(&ws).unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch[0].contains(">A<"), "FIFO order — first-in first");
        assert!(batch[1].contains(">B<"));

        // Second drain must return empty (offset advanced).
        let batch2 = drain(&ws).unwrap();
        assert!(batch2.is_empty());

        disable_coord();
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn drain_returns_only_new_envelopes_after_partial_drain() {
        let _g = enable_coord();
        let ws = unique_ws("drain-incremental");
        emit(&ws, "agent-1", "<task-notification>1</task-notification>").unwrap();
        emit(&ws, "agent-2", "<task-notification>2</task-notification>").unwrap();
        assert_eq!(drain(&ws).unwrap().len(), 2);

        // A new emit AFTER the drain — next drain returns only it.
        emit(&ws, "agent-3", "<task-notification>3</task-notification>").unwrap();
        let batch = drain(&ws).unwrap();
        assert_eq!(batch.len(), 1);
        assert!(batch[0].contains(">3<"));

        disable_coord();
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn drain_skips_non_task_notification_kinds_but_advances_offset() {
        let _g = enable_coord();
        let ws = unique_ws("drain-mixed");

        // A plain message alongside a task-notification, sent the way any
        // peer sends one — through its own mailbox, into its own conversation
        // with the coordinator. Two senders means two transcripts, which is
        // also what makes this cover the drain walking the chat list.
        Mailbox::workspace_local(&ws, "team-lead".to_string())
            .send(MailboxEnvelope {
                from: "team-lead".to_string(),
                to: COORDINATOR_INBOX_RECIPIENT.to_string(),
                body: "just chatting".to_string(),
                summary: None,
                timestamp: 0,
                color: None,
                kind: kinds::MESSAGE.to_string(),
                request_id: None,
            })
            .unwrap();
        emit(&ws, "agent-tn", "<task-notification>tn</task-notification>").unwrap();

        let batch = drain(&ws).unwrap();
        assert_eq!(batch.len(), 1, "MESSAGE kind filtered out");
        assert!(batch[0].contains(">tn<"));

        // Every conversation's position advanced past what was taken — a
        // MESSAGE arriving AFTER this drain would show up next time, but the
        // ones already drained must not, filtered out or otherwise.
        assert!(drain(&ws).unwrap().is_empty());

        disable_coord();
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn format_drain_batch_joins_with_blank_lines_and_trailing_separator() {
        let out = format_drain_batch(&[
            "<task-notification>A</task-notification>".to_string(),
            "<task-notification>B</task-notification>".to_string(),
        ]);
        assert!(out.contains("A</task-notification>\n\n<task-notification>B"));
        assert!(
            out.ends_with("\n\n"),
            "trailing blank line separates from user input"
        );

        // Empty input -> empty output (no accidental leading blanks).
        assert!(format_drain_batch(&[]).is_empty());
    }
}
