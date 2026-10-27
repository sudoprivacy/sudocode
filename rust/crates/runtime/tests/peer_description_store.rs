//! Persistent descriptions use the actual native or cohost mailbox filesystem.

use std::sync::Arc;

use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::Kernel;
use runtime::mailbox::Mailbox;
use runtime::{FsBackend, KernelFsBackend};

fn assert_stale_refresh_preserves_the_committed_description(mailbox: &Mailbox) {
    mailbox.ensure_presence().unwrap();
    let store = mailbox.description_store(mailbox.self_id()).unwrap();
    assert!(store.read().unwrap().is_none());
    let slow = store.snapshot("slow-session").unwrap();
    let current = store.snapshot("current-session").unwrap();
    assert!(store
        .publish(&current.update("Current session recovery work").unwrap())
        .unwrap());
    let published = store.read().unwrap().unwrap();
    assert_eq!(published.source_session_id, "current-session");
    assert!(!store
        .publish(&slow.update("Obsolete task").unwrap())
        .unwrap());
    assert_eq!(store.read().unwrap().unwrap(), published);
}

#[test]
fn native_mailbox_reopens_the_same_description_after_a_stale_compaction() {
    let root = runtime::test_support::temp_dir_path("peer-description-native");
    std::fs::create_dir_all(&root).unwrap();
    {
        let mailbox = Mailbox::workspace_local(&root, "peer-author".into());
        assert_stale_refresh_preserves_the_committed_description(&mailbox);
    }
    let reopened = Mailbox::workspace_local(&root, "peer-observer".into());
    let description = reopened
        .description_store("peer-author")
        .unwrap()
        .read()
        .unwrap()
        .unwrap();
    assert_eq!(description.description, "Current session recovery work");
    assert_eq!(description.source_session_id, "current-session");
    let bytes = std::fs::read(root.join("agents/peer-author/description.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["updated_at"],
        description.updated_at
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cohost_mailbox_keeps_description_in_the_agents_vfs_directory() {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let descriptor = AgentDescriptor {
        pid: "description-runtime-7".into(),
        name: "peer-author".into(),
        owner_id: "test-owner".into(),
        zone_id: "root".into(),
        ..AgentDescriptor::default()
    };
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &descriptor,
        "/proc/7/workspace",
    ));
    let mailbox = Mailbox::daemon_absolute(Arc::clone(&fs), descriptor.name.clone());
    assert_stale_refresh_preserves_the_committed_description(&mailbox);
    let bytes = fs.read("/agents/peer-author/description.json").unwrap();
    let description: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(description["source_session_id"], "current-session");
    assert_eq!(description["description"], "Current session recovery work");
    let observer = AgentDescriptor {
        pid: "description-runtime-8".into(),
        name: "peer-observer".into(),
        ..descriptor
    };
    let observer_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        kernel,
        &observer,
        "/proc/8/workspace",
    ));
    let observer_mailbox = Mailbox::daemon_absolute(observer_fs, observer.name);
    let reopened = observer_mailbox
        .description_store("peer-author")
        .unwrap()
        .read()
        .unwrap()
        .unwrap();
    assert_eq!(reopened.description, "Current session recovery work");
    assert_eq!(reopened.source_session_id, "current-session");
}
