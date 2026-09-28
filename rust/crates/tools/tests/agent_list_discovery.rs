//! End-to-end proof that `agent_list` discovers a peer provisioned through the
//! `Mailbox` conversation contract — over the session's backend, not a probed
//! filename on local disk.
//!
//! This is the regression guard for the discovery bug: `agent_list` used to
//! probe `<pair-root>/agents/<name>/chat-with-me`, a file the conversation
//! contract stopped creating, so it discovered nobody real while its
//! hand-planted unit fixtures kept CI green. Here presence is provisioned the
//! way production does it — `Mailbox::ensure_presence()` — so the next
//! convention change breaks this test instead of hiding behind it.

use std::sync::Arc;

use runtime::mailbox::{InboxConvention, Mailbox, MailboxScope};

/// A unique temp dir for one test's mailbox root + agent store.
fn temp_root(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("agent-list-e2e-{label}-{nanos}"))
}

#[test]
fn agent_list_discovers_a_peer_provisioned_through_the_mailbox() {
    let root = temp_root("peer");
    std::fs::create_dir_all(&root).unwrap();
    // Isolate the sub-agent store so no stray manifest leaks into the list.
    let store = root.join("agent-store");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    // A peer "alice" announces herself in the shared namespace, exactly as a
    // second scode would on first run.
    let alice = Mailbox::workspace_local(&root, "alice".to_string());
    alice.ensure_presence().expect("alice announces presence");

    // "me" is the session doing the discovery. Scope its mailbox onto this
    // thread so `collect_agent_list` (via `sending_mailbox()`) uses it — the
    // same handle `send` resolves.
    let me = Arc::new(Mailbox::workspace_local(&root, "me".to_string()));
    let _scope = MailboxScope::enter(me);

    // The host filesystem, because `SUDOCODE_AGENT_STORE` above names a host
    // directory: the sub-agent half of the list is read through the session's
    // backend, which for this test's session is the host.
    let rows = tools::collect_agent_list(false, &runtime::fs_backend::StdFsBackend)
        .expect("the namespace is readable here");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();

    assert!(
        names.contains(&"alice"),
        "agent_list must discover the presence-provisioned peer; got {names:?}"
    );
    assert!(
        !names.contains(&"me"),
        "an agent is not its own peer; got {names:?}"
    );
    let alice_row = rows.iter().find(|r| r.name == "alice").unwrap();
    assert_eq!(alice_row.kind, "peer");

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&root);
}

/// `readdir` on the agents directory returns whatever the mount holds, and for a
/// co-host that mount is a ZONE: its own storage directories come back as entries,
/// and where the agent and conversation trees alias one zone, so do conversation
/// ids and session ids. Live, `agent_list` offered `raft` and `sm` as peers — which
/// is worse than an empty list, because a model told those are its peers addresses
/// them, and the wrong answer arrives looking like a right one.
///
/// So an entry counts only when it carries what `ensure_presence` creates. Planted
/// here as a bare directory with no chat list, which is exactly the shape a zone's
/// storage dir has.
#[test]
fn a_directory_that_never_announced_itself_is_not_a_peer() {
    let root = temp_root("shape");
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("SUDOCODE_AGENT_STORE", root.join("agent-store"));

    let alice = Mailbox::workspace_local(&root, "alice".to_string());
    alice.ensure_presence().expect("alice announces presence");

    // Siblings of `alice` with nothing inside them — the shape a zone's storage
    // directory has. The path comes from the same convention the mailbox uses, so
    // they land where production looks rather than where the test guesses.
    let agents_dir = InboxConvention::new(root.to_string_lossy().to_string()).agents_dir();
    for not_an_agent in ["raft", "sm"] {
        std::fs::create_dir_all(std::path::Path::new(&agents_dir).join(not_an_agent)).unwrap();
    }

    let me = Arc::new(Mailbox::workspace_local(&root, "me".to_string()));
    let _scope = MailboxScope::enter(me);
    let rows = tools::collect_agent_list(false, &runtime::fs_backend::StdFsBackend)
        .expect("the namespace is readable here");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();

    assert!(
        names.contains(&"alice"),
        "the announced agent must still be there; got {names:?}"
    );
    for not_an_agent in ["raft", "sm"] {
        assert!(
            !names.contains(&not_an_agent),
            "{not_an_agent} never announced itself and must not be offered as a peer; got {names:?}"
        );
    }
}
