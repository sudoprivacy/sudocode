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

use runtime::directory::{Directory, Member};
use runtime::mailbox::{Mailbox, MailboxScope};
use serde_json::{json, Value};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A unique temp dir for one test's mailbox root + agent store.
fn temp_root(label: &str) -> std::path::PathBuf {
    // pid plus a counter, not a clock. These tests run on threads of one binary and
    // a nanosecond read is not guaranteed to differ between them — on macOS it
    // measurably does not. Two tests sharing a root would see each other's agents,
    // and this file's whole subject is which agents are visible.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("agent-list-e2e-{label}-{}-{n}", std::process::id()))
}

#[test]
fn a_fresh_local_namespace_allows_discovery_and_delivery_to_an_attached_peer() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_root("fresh-namespace");
    let local_root = root.join("local");
    let nexus_root = root.join("nexus");
    let store = root.join("agent-store");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);
    let peer = Mailbox::workspace_local(&nexus_root, "alice".into());
    peer.ensure_presence().unwrap();
    let _scope = MailboxScope::enter(Arc::new(Directory::new(vec![
        Member {
            label: "local",
            mailbox: Arc::new(Mailbox::workspace_local(&local_root, "me".into())),
        },
        Member {
            label: "nexus",
            mailbox: Arc::new(Mailbox::workspace_local(&nexus_root, "me".into())),
        },
    ])));
    let listing: Value =
        serde_json::from_str(&tools::execute_tool("agent_list", &json!({})).unwrap()).unwrap();
    assert_eq!(listing["agents"][0]["send_to"], "nexus:alice");
    tools::execute_tool(
        "send",
        &json!({"to": "alice", "message": "first message", "summary": "fresh namespace"}),
    )
    .unwrap();
    let (messages, _) = peer.poll_conversation("me", 0, 0).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].body, "first message");
    assert!(
        !local_root.exists(),
        "delivery must use the attached peer's namespace"
    );
    std::env::remove_var("SUDOCODE_AGENT_STORE");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn agent_list_discovers_a_peer_provisioned_through_the_mailbox() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    let _scope = MailboxScope::enter(Arc::new(runtime::directory::Directory::single("local", me)));

    // The host filesystem, because `SUDOCODE_AGENT_STORE` above names a host
    // directory: the sub-agent half of the list is read through the session's
    // backend, which for this test's session is the host.
    let rows = tools::collect_agent_list(false, &runtime::fs_backend::StdFsBackend)
        .expect("collect_agent_list should succeed when the namespace is readable");
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
    assert_eq!(alice_row.send_to, "local:alice");

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn collect_agent_list_surfaces_a_read_failure_instead_of_empty() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // "Can't read the namespace" must NOT masquerade as "nobody is here" — the
    // regression sudocode#786 flagged (unwrap_or_default swallowing the Err).
    // Force a genuine read error by making the agents path a regular FILE, so
    // readdir fails rather than returning an empty listing.
    let root = temp_root("readfail");
    std::fs::create_dir_all(&root).unwrap();
    let store = root.join("agent-store");
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);

    let me = Arc::new(Mailbox::workspace_local(&root, "me".to_string()));
    // agents_dir is `<root>/agents`; plant a file there so readdir errors.
    std::fs::write(root.join("agents"), b"not a directory").unwrap();
    let _scope = MailboxScope::enter(Arc::new(runtime::directory::Directory::single("local", me)));

    let result = tools::collect_agent_list(false, &runtime::fs_backend::StdFsBackend);
    assert!(
        result.is_err(),
        "a namespace read failure must surface as Err, not an empty list; got {result:?}"
    );

    std::env::remove_var("SUDOCODE_AGENT_STORE");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn copied_addresses_preserve_same_name_peers_and_tasks_and_fail_closed() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let root = temp_root("copied-addresses");
    let store = root.join("agent-store");
    std::fs::create_dir_all(&store).unwrap();
    std::env::set_var("SUDOCODE_AGENT_STORE", &store);
    seed_same_name_tasks(&store);
    let local_root = root.join("local");
    let nexus_root = root.join("nexus");
    let local_peer = Mailbox::workspace_local(&local_root, "alice".into());
    let nexus_peer = Mailbox::workspace_local(&nexus_root, "alice".into());
    local_peer.ensure_presence().unwrap();
    nexus_peer.ensure_presence().unwrap();
    let local = Arc::new(Mailbox::workspace_local(&local_root, "me".into()));
    let nexus = Arc::new(Mailbox::workspace_local(&nexus_root, "me".into()));
    let _scope = MailboxScope::enter(Arc::new(Directory::new(vec![
        Member {
            label: "local",
            mailbox: Arc::clone(&local),
        },
        Member {
            label: "nexus",
            mailbox: Arc::clone(&nexus),
        },
    ])));
    let listing: Value =
        serde_json::from_str(&tools::execute_tool("agent_list", &json!({})).unwrap()).unwrap();
    let rows = listing["agents"].as_array().unwrap();
    assert_eq!(
        rows.len(),
        4,
        "same-name peers and tasks must remain distinct"
    );
    for row in rows {
        assert_discovered_address_routes(row);
    }
    for (peer, address) in [(&local_peer, "local:alice"), (&nexus_peer, "nexus:alice")] {
        let (messages, _) = peer.poll_conversation("me", 0, 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, address);
    }
    let transcript = local.transcript_path("alice");
    let before = std::fs::read(&transcript).unwrap();
    let error = tools::execute_tool(
        "send",
        &json!({
            "to": "unattached:alice", "message": "must not deliver", "summary": "negative",
        }),
    )
    .unwrap_err();
    assert!(error.contains("not attached"), "{error}");
    assert_eq!(std::fs::read(&transcript).unwrap(), before);

    assert_manifest_errors_preserve_destination(&store, &local_root, &local);
    assert_self_identity_is_scoped_to_its_namespace(&local_root, &nexus);

    let bad_root = root.join("unreadable");
    std::fs::create_dir_all(&bad_root).unwrap();
    std::fs::write(bad_root.join("agents"), "not a directory").unwrap();
    let _failing_scope = MailboxScope::enter(Arc::new(Directory::new(vec![
        Member {
            label: "local",
            mailbox: local,
        },
        Member {
            label: "unreadable",
            mailbox: Arc::new(Mailbox::workspace_local(&bad_root, "me".into())),
        },
    ])));
    assert!(tools::execute_tool(
        "send",
        &json!({
            "to": "alice", "message": "must not deliver", "summary": "negative",
        })
    )
    .is_err());
    assert_eq!(std::fs::read(&transcript).unwrap(), before);
    std::env::remove_var("SUDOCODE_AGENT_STORE");
    std::fs::remove_dir_all(root).unwrap();
}

fn assert_self_identity_is_scoped_to_its_namespace(
    local_root: &std::path::Path,
    nexus: &Arc<Mailbox>,
) {
    let _scope = MailboxScope::enter(Arc::new(Directory::new(vec![
        Member {
            label: "local",
            mailbox: Arc::new(Mailbox::workspace_local(local_root, "alice".into())),
        },
        Member {
            label: "nexus",
            mailbox: Arc::clone(nexus),
        },
    ])));
    let listing: Value =
        serde_json::from_str(&tools::execute_tool("agent_list", &json!({})).unwrap()).unwrap();
    let rows = listing["agents"].as_array().unwrap();
    assert!(!rows.iter().any(|row| row["send_to"] == "local:alice"));
    assert!(rows.iter().any(|row| row["send_to"] == "nexus:alice"));
}

fn assert_discovered_address_routes(row: &Value) {
    assert_eq!(row["name"], "alice");
    assert!(row.get("pid").is_none(), "a task ID is not an OS PID");
    let address = row["send_to"].as_str().unwrap();
    if row["kind"] == "peer" {
        assert!(row.get("active").is_none());
        tools::execute_tool(
            "send",
            &json!({
                "to": address, "message": address, "summary": "address roundtrip",
            }),
        )
        .unwrap();
    } else {
        assert_eq!(row["kind"], "subagent");
        assert_eq!(row["task_id"], address);
        assert_eq!(row["active"], false, "a manifest cannot prove liveness");
        let error = tools::execute_tool(
            "send",
            &json!({
                "to": address, "message": "must not deliver", "summary": "stale task",
            }),
        )
        .unwrap_err();
        assert!(error.contains("no active runtime"), "{error}");
    }
}

fn assert_manifest_errors_preserve_destination(
    store: &std::path::Path,
    local_root: &std::path::Path,
    local: &Mailbox,
) {
    std::fs::write(store.join("broken-task.json"), "{incomplete manifest").unwrap();
    std::fs::create_dir(store.join("unreadable-task.json")).unwrap();
    for task in ["broken-task", "unreadable-task"] {
        let transcript = local.transcript_path(task);
        assert!(!std::path::Path::new(&transcript).exists());
        let error = tools::execute_tool(
            "send",
            &json!({
                "to": task, "message": "must not deliver", "summary": "invalid task",
            }),
        )
        .expect_err("an unreadable task manifest must not turn a task handle into a peer name");
        assert!(error.contains("manifest"), "{error}");
        assert!(!std::path::Path::new(&transcript).exists());

        // An explicit peer namespace selects a different destination even
        // when a task with the same handle has an unreadable saved manifest.
        tools::execute_tool(
            "send",
            &json!({"to": format!("local:{task}"), "message": task, "summary": "explicit peer"}),
        )
        .unwrap();
        let peer = Mailbox::workspace_local(local_root, task.into());
        let (messages, _) = peer.poll_conversation("me", 0, 0).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, task);
    }
}

fn seed_same_name_tasks(store: &std::path::Path) {
    for id in ["task-one", "task-two"] {
        let path = store.join(format!("{id}.json"));
        std::fs::write(
            &path,
            serde_json::json!({
                "agentId": id, "name": "alice", "description": "test worker",
                "status": "running", "outputFile": "unused", "manifestFile": path,
                "createdAt": "1", "derivedState": "running",
            })
            .to_string(),
        )
        .unwrap();
    }
}
