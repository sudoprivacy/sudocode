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
use runtime::mailbox::{local_agent_name, InboxConvention, Mailbox};

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
        InboxConvention::new(root.to_string_lossy().into_owned()),
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

    // Bob reads the conversation he shares with alice: one transcript, which
    // alice appended to and he reads from.
    let bob_inbox = bob.read_conversation("alice").unwrap();
    assert_eq!(bob_inbox.len(), 1);
    assert_eq!(bob_inbox[0].body, "hi bob");
    assert_eq!(bob_inbox[0].from, "alice");

    // A conversation nobody wrote to stays empty — no cross-talk between
    // pairs, which is the property per-pair transcripts buy.
    assert!(bob.read_conversation("carol").unwrap().is_empty());

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
    let to_a = mailbox(&root, &a).read_conversation("sender").unwrap();
    assert_eq!(to_a.len(), 1, "the addressed folder's agent receives it");
    assert!(
        mailbox(&root, &b)
            .read_conversation("sender")
            .unwrap()
            .is_empty(),
        "the same-basename sibling must not receive it"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_read_position_is_scoped_to_its_root() {
    // The same agent name under two different roots must keep independent read
    // positions — otherwise a position from one conversation is applied to the
    // other and inbound messages are silently skipped.
    //
    // It now falls out of WHERE the position lives: inside the conversation,
    // under that root, beside the transcript it describes. The cursor file this
    // replaced sat outside any conversation and had to key itself by
    // (root, agent) by hand to get the same property.
    let root_a = tmp_root("pos-a");
    let root_b = tmp_root("pos-b");

    let in_a = InboxConvention::new(root_a.to_string_lossy().into_owned())
        .reader_path("worker", "peer", "worker");
    let in_b = InboxConvention::new(root_b.to_string_lossy().into_owned())
        .reader_path("worker", "peer", "worker");
    assert_ne!(
        in_a, in_b,
        "same name under two roots must not share one read position"
    );

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}
