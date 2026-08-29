//! Nexus-backed A2A mailbox — the standalone counterpart to the local
//! [`crate::agent_mailbox`] (`.sudocode-inbox`).
//!
//! A message is one framed append to the recipient's replicated DT_STREAM
//! inbox at `/agents/<recipient>/chat-with-me`; under auth-on the node
//! stamps an unforgeable `from` by the authenticated writer. The wire type
//! is [`a2a::MailboxEnvelope`] — the A2A SSOT the in-process co-host
//! ([`crate::spawn_task`]) also uses — so a standalone `scode` and a co-host
//! agent interoperate over the very same stream, byte-identically, BY
//! CONSTRUCTION (one envelope type, not two that "happen to match").
//!
//! Transport only: send is a `stream_write`, poll is a cursor-advancing
//! `stream_read_at` loop. Cursor persistence + REPL surfacing live in the
//! caller (the cursor is ephemeral read position, never persisted here).

use a2a::MailboxEnvelope;
use nexus_vfs_client::NexusVfsClient;

/// VFS path of an agent's replicated A2A inbox.
#[must_use]
pub fn inbox_path(agent: &str) -> String {
    format!("/agents/{agent}/chat-with-me")
}

/// Append a message to `to`'s inbox and return the offset it landed at.
///
/// The `from` we write is advisory: under auth-on the daemon's
/// `MailboxStampingHook` overwrites it with the authenticated caller's
/// identity (so it cannot be forged); under auth-off it is used as-is.
///
/// # Errors
/// Returns a `String` error if the `stream_write` RPC fails.
pub fn send(
    client: &NexusVfsClient,
    from: &str,
    to: &str,
    body: &str,
    auth_token: &str,
) -> Result<u64, String> {
    let env = MailboxEnvelope {
        from: from.to_string(),
        to: to.to_string(),
        body: body.to_string(),
    };
    let path = inbox_path(to);
    client
        .stream_write(&path, env.to_bytes(), auth_token)
        .map_err(|e| format!("A2A stream_write to {path}: {e}"))
}

/// One inbound A2A message (self-writes and empty bodies already filtered).
#[derive(Debug, Clone)]
pub struct Inbound {
    pub from: String,
    pub body: String,
}

/// Drain every frame in `self_agent`'s inbox from `cursor` forward
/// (non-blocking — stops at the first `eof`). Returns the new inbound
/// messages and the advanced cursor to persist for the next poll.
///
/// Skips our OWN writes (`from == self_agent`) so a shared read/write stream
/// never echoes to us, and skips senderless / empty-body frames — the same
/// filter the co-host loop applies in `parse_inbound`.
///
/// # Errors
/// Returns a `String` error if a `stream_read_at` RPC fails.
pub fn poll_new(
    client: &NexusVfsClient,
    self_agent: &str,
    mut cursor: u64,
    auth_token: &str,
) -> Result<(Vec<Inbound>, u64), String> {
    let path = inbox_path(self_agent);
    let mut out = Vec::new();
    loop {
        let (data, next, eof) = client
            .stream_read_at(&path, cursor, auth_token)
            .map_err(|e| format!("A2A stream_read_at {path}@{cursor}: {e}"))?;
        if eof {
            break;
        }
        if let Some(env) = MailboxEnvelope::from_bytes(&data) {
            if !env.from.is_empty() && env.from != self_agent && !env.body.is_empty() {
                out.push(Inbound {
                    from: env.from,
                    body: env.body,
                });
            }
        }
        if next <= cursor {
            // No forward progress — guard against an infinite loop on a
            // stream that returns the same offset (a buggy server must not
            // wedge the poller).
            break;
        }
        cursor = next;
    }
    Ok((out, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_path_is_the_a2a_mailbox() {
        assert_eq!(inbox_path("win-ai"), "/agents/win-ai/chat-with-me");
    }

    #[test]
    fn envelope_round_trips_via_the_a2a_ssot_type() {
        // We reuse the a2a-crate envelope (the SSOT the co-host writes), so a
        // round-trip through the same to_bytes/from_bytes the co-host uses
        // must recover from/body — this is what makes standalone ⇄ co-host
        // interop byte-identical by construction.
        let env = MailboxEnvelope {
            from: "operator".into(),
            to: "win-ai".into(),
            body: "hi".into(),
        };
        let back = MailboxEnvelope::from_bytes(&env.to_bytes()).expect("a2a envelope round-trip");
        assert_eq!(back.from, "operator");
        assert_eq!(back.body, "hi");
    }
}
