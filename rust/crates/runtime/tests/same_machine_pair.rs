//! Same-machine standalone pair: two `scode` identities sharing one pair root
//! exchange a message with no daemon, over the unified
//! `{root}/agents/{name}/chat-with-me` path and `StdFsBackend`.
//!
//! This is the deterministic core of the feature the REPL wires up (a live
//! two-process PTY exchange is covered by `pty_agent_duet` against a real
//! daemon). It proves the three things the design turns on:
//!   1. A sends to B by bare name; B reads it — one shared pair root, distinct
//!      names, no cross-talk to a third name.
//!   2. `local_agent_name` derivation disambiguates same-basename folders, so
//!      two projects under the shared root don't collide.
//!   3. The receive cursor is keyed by (root, agent): the same name under two
//!      roots keeps independent positions.

use std::path::Path;
use std::sync::Arc;

use runtime::agent_mailbox::MailboxEnvelope;
use runtime::fs_backend::StdFsBackend;
use runtime::mailbox::{local_agent_name, InboxConvention, InboxCursor, Mailbox};

fn tmp_root(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("pair-{label}-{nanos}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn mailbox(root: &Path, self_id: &str) -> Mailbox {
    Mailbox::new(
        Arc::new(StdFsBackend),
        self_id.to_string(),
        InboxConvention::PerRecipient {
            root: root.to_string_lossy().into_owned(),
        },
    )
}

fn note(from: &str, to: &str, body: &str) -> MailboxEnvelope {
    MailboxEnvelope {
        from: from.to_string(),
        to: to.to_string(),
        body: body.to_string(),
        summary: None,
        timestamp: 0,
        color: None,
        kind: runtime::agent_mailbox::kinds::MESSAGE.to_string(),
        request_id: None,
    }
}

#[test]
fn a_sends_to_b_over_shared_pair_root() {
    let root = tmp_root("ab");
    // Distinct identities, one shared root — the same-folder-independent case.
    let alice = mailbox(&root, "alice");
    let bob = mailbox(&root, "bob");

    alice.send(note("alice", "bob", "hi bob")).unwrap();

    // Bob reads his own inbox; alice's write landed there, addressed by name.
    let bob_inbox = bob.read_all("bob").unwrap();
    assert_eq!(bob_inbox.len(), 1);
    assert_eq!(bob_inbox[0].body, "hi bob");
    assert_eq!(bob_inbox[0].from, "alice");

    // A third name nobody wrote to stays empty — no cross-talk.
    assert!(bob.read_all("carol").unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn derived_names_disambiguate_same_basename_folders() {
    // Two different projects that happen to share a basename must not collide
    // inside the shared pair root.
    let a = local_agent_name(None, Path::new("/home/me/x/app"));
    let b = local_agent_name(None, Path::new("/home/me/y/app"));
    assert_ne!(a, b);

    let root = tmp_root("basename");
    let sender = mailbox(&root, "sender");
    sender.send(note("sender", &a, "for x/app")).unwrap();

    // Only the exact derived name sees it; the collision candidate does not.
    let to_a = mailbox(&root, &a).read_all(&a).unwrap();
    assert_eq!(to_a.len(), 1, "the addressed folder's agent receives it");
    assert!(
        mailbox(&root, &b).read_all(&b).unwrap().is_empty(),
        "the same-basename sibling must not receive it"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cursor_is_keyed_by_root_and_agent() {
    // The same agent name under two different roots must keep independent
    // cursor files — otherwise a position from one stream is applied to the
    // other and inbound messages are silently skipped.
    let root_a = tmp_root("cursor-a");
    let root_b = tmp_root("cursor-b");

    let cur_a = InboxCursor::local(&root_a, "worker");
    let cur_b = InboxCursor::local(&root_b, "worker");
    assert_ne!(
        cur_a.path(),
        cur_b.path(),
        "same name under two roots must not share one cursor file"
    );

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}
