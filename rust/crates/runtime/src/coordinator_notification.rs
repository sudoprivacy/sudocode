//! Durable coordinator notifications. Reading retains delivery receipts until
//! the runtime successfully persists the turn that contains the notifications.

use std::path::Path;

use crate::agent_mailbox::{kinds, MailboxEnvelope};
use crate::mailbox::Mailbox;

/// Well-known recipient string for the coordinator's inbox.
pub const COORDINATOR_INBOX_RECIPIENT: &str = "coordinator";

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
        delivery: crate::turn_input::InputDelivery::Steer,
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

/// Read pending notifications without consuming their durable positions.
pub fn drain(workspace_root: &Path) -> Result<crate::mailbox::PendingMessages, String> {
    if !crate::coordinator_mode::is_coordinator_mode() {
        return Ok(crate::mailbox::PendingMessages::default());
    }
    Mailbox::workspace_local(workspace_root, COORDINATOR_INBOX_RECIPIENT.to_string()).read_pending()
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
