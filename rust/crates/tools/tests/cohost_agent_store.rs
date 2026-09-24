//! A co-hosted agent's sub-agent store lives in the VFS it says it lives in.
//!
//! The root moved into the agent's own subtree (`/agents/<name>/subagents`) while
//! every read and write around it was still `std::fs` — which on a Linux daemon
//! aims that path at the filesystem root and on Windows at the current drive. The
//! directory the store reported and the directory it used were two different
//! places, and nothing failed loudly about it.
//!
//! So this drives the real `prepare_agent_job` against a real `Kernel`: the store
//! is created, the manifest is written and the first output file is laid down, and
//! all three are asserted INSIDE the VFS and absent from the host.

use std::sync::Arc;

use kernel::kernel::Kernel;
use runtime::{FsBackend, KernelFsBackend};

const AGENT: &str = "scode-agent";

/// A kernel whose `/agents` subtree can hold content, and one co-hosted agent's
/// filesystem over it.
fn cohost_fs() -> (Arc<Kernel>, Arc<dyn FsBackend>) {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/agents",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
        Arc::clone(&kernel),
        "test-owner",
        "root",
        AGENT,
        "/proc/7/workspace".to_string(),
    ));
    (kernel, fs)
}

#[test]
fn a_cohosted_agents_subagent_store_is_written_into_its_own_vfs_subtree() {
    let (_kernel, fs) = cohost_fs();

    let manifest =
        tools::testing::prepare_agent_job_on(Arc::clone(&fs), "general-purpose", "do the thing")
            .expect("preparing a sub-agent job should succeed on a co-hosted filesystem");

    // The paths the manifest advertises are the agent's own subtree — not a
    // directory derived from the daemon's process directory, which every
    // co-hosted agent on that daemon would share.
    let expected_root = format!("/agents/{AGENT}/subagents/");
    assert!(
        manifest.manifest_file.starts_with(&expected_root),
        "the manifest should live under {expected_root}; got {}",
        manifest.manifest_file
    );
    assert!(
        manifest.output_file.starts_with(&expected_root),
        "and so should its output; got {}",
        manifest.output_file
    );

    // And they are really there, on the filesystem that rooted them.
    assert!(
        fs.exists(&manifest.manifest_file).unwrap_or(false),
        "the manifest should be readable through the agent's filesystem"
    );
    assert!(
        fs.exists(&manifest.output_file).unwrap_or(false),
        "the output file should be too"
    );

    // Nowhere on the host. A `std::fs` regression would put them here instead —
    // silently, because a daemon's cwd is always writable.
    assert!(
        !std::path::Path::new(&manifest.manifest_file).exists(),
        "nothing should have been written to the host filesystem"
    );

    // The store's own enumeration reads what was written: this is what
    // `agent_list` and `pid_status` answer from, and reading it with `std::fs`
    // reported an empty store for every co-hosted agent.
    let snapshots = tools::list_agent_snapshots_from_store_with(false, fs.as_ref())
        .expect("the store should enumerate");
    assert!(
        snapshots.iter().any(|s| s.agent_id == manifest.agent_id),
        "the store should list the job it just prepared; got {:?}",
        snapshots.iter().map(|s| &s.agent_id).collect::<Vec<_>>()
    );
}
