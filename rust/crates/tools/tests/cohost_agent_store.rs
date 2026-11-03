//! Cohost task stores and followup conversations use their owning VFS.
//!
//! A real Kernel, the production tool dispatcher and the task receive loop
//! verify saved-task rejection, followup delivery and durable acknowledgement.
//! A native file blocks host-directory writes at the workspace path.

use std::sync::Arc;

use kernel::kernel::Kernel;
use runtime::{FsBackend, KernelFsBackend};

const AGENT: &str = "scode-agent";

struct DeniedTaskSender;

impl kernel::core::dispatch::NativeInterceptHook for DeniedTaskSender {
    fn name(&self) -> &'static str {
        "task-sender-authorization"
    }

    fn mutating_path_suffixes(&self) -> &'static [&'static str] {
        &["/transcript"]
    }

    fn on_pre(
        &self,
        context: &kernel::core::dispatch::HookContext,
    ) -> Result<kernel::core::dispatch::HookOutcome, String> {
        if let kernel::core::dispatch::HookContext::Write(write) = context {
            if write.identity.agent_id == "blocked-sender" {
                return Err("blocked-sender may not write the task transcript".into());
            }
        }
        Ok(kernel::core::dispatch::HookOutcome::Pass)
    }
}

/// A kernel whose `/agents` subtree can hold content, and one co-hosted agent's
/// filesystem over it.
fn cohost_fs(workspace_root: impl Into<String>) -> (Arc<Kernel>, Arc<dyn FsBackend>) {
    let kernel = Arc::new(Kernel::new());
    kernel.vfs_router_arc().add_mount(
        "/",
        "root",
        Some(Arc::new(runtime::test_support::MemObjectStore::default())),
        false,
    );
    let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &kernel::core::agents::registry::AgentDescriptor {
            pid: "pid-7".to_string(),
            name: AGENT.to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..kernel::core::agents::registry::AgentDescriptor::default()
        },
        workspace_root,
    ));
    (kernel, fs)
}

#[test]
fn a_cohosted_agents_subagent_store_is_written_into_its_own_vfs_subtree() {
    let (_kernel, fs) = cohost_fs("/proc/7/workspace");

    let manifest = tools::testing::prepare_agent_job_on(&fs, "general-purpose", "do the thing")
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

    let registry = tools::GlobalToolRegistry::builtin().with_fs(Arc::clone(&fs));
    let send = serde_json::json!({
        "to": manifest.agent_id,
        "message": "must not become a peer message",
        "summary": "saved task",
    });
    let error = registry.execute("send", &send).unwrap_err();
    assert!(error.contains("no active runtime"), "{error}");

    fs.write(&manifest.manifest_file, b"{invalid manifest")
        .unwrap();
    let error = registry.execute("send", &send).unwrap_err();
    assert!(error.contains("invalid agent"), "{error}");
    assert!(!fs
        .exists(&format!(
            "/agents/{AGENT}/conversations/{}",
            manifest.agent_id
        ))
        .unwrap());
}

#[test]
fn a_live_cohost_task_receives_followups_only_through_its_session_filesystem() {
    use runtime::mailbox::{Mailbox, MailboxScope};
    use runtime::{HookAbortSignal, WorkspaceRootScope};
    use std::io::Write;

    let workspace = std::env::temp_dir().join(format!(
        "scode-cohost-task-mailbox-{}-{}.guard",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&workspace)
        .unwrap()
        .write_all(b"host sentinel")
        .unwrap();
    let (kernel, fs) = cohost_fs(workspace.to_string_lossy().into_owned());
    let hook = kernel
        .enlist_hook_only_service("task-sender-authorization")
        .unwrap();
    kernel.register_service_hook(&hook, Box::new(DeniedTaskSender));
    fs.create_dir_all(&workspace.to_string_lossy()).unwrap();
    let _workspace = WorkspaceRootScope::enter(&workspace);
    let _mailbox = MailboxScope::enter(Arc::new(Mailbox::daemon_absolute(
        Arc::clone(&fs),
        AGENT.into(),
    )));
    let task = format!("cohost-followup-{}", std::process::id());
    let abort = HookAbortSignal::new();
    tools::register_agent_runtime(&task, abort.clone(), fs.working_root().unwrap()).unwrap();
    let blocked_fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent_descriptor(
        Arc::clone(&kernel),
        &kernel::core::agents::registry::AgentDescriptor {
            pid: "pid-blocked".to_string(),
            name: "blocked-sender".to_string(),
            owner_id: "test-owner".to_string(),
            zone_id: "root".to_string(),
            ..kernel::core::agents::registry::AgentDescriptor::default()
        },
        "/another-runtime",
    ));
    let denied = tools::GlobalToolRegistry::builtin()
        .with_fs(blocked_fs)
        .execute(
            "send",
            &serde_json::json!({
                "to":task,"message":"must not be delivered","summary":"denied caller",
            }),
        )
        .unwrap_err();
    assert!(denied.contains("blocked-sender"), "{denied}");
    let registry = tools::GlobalToolRegistry::builtin().with_fs(Arc::clone(&fs));
    let mailbox = Mailbox::workspace(Arc::clone(&fs), task.clone()).unwrap();
    let mut prompts = Vec::new();
    let mut positions_during_turn = Vec::new();
    let result = tools::testing::run_multi_turn_loop_in_mailbox_for_test(
        &mailbox,
        &abort,
        "first turn".into(),
        4,
        |prompt| {
            if prompts.is_empty() {
                registry.execute(
                    "send",
                    &serde_json::json!({
                        "to": task, "message": "continue inside VFS", "summary": "followup",
                    }),
                )?;
            } else {
                positions_during_turn.push(mailbox.read_position(AGENT)?.unwrap_or(0));
            }
            prompts.push(prompt);
            Ok(format!("turn {} complete", prompts.len()))
        },
    );
    let consumed = mailbox.read_position(AGENT).unwrap().unwrap_or(0);
    let unread = mailbox.read_pending().unwrap().messages().count();
    let persisted = fs.read(&mailbox.transcript_path(AGENT)).unwrap();
    tools::unregister_agent_abort_signal(&task);
    let sentinel = std::fs::read(&workspace).unwrap();
    std::fs::remove_file(&workspace).unwrap();
    assert_eq!(sentinel, b"host sentinel");
    let persisted = String::from_utf8(persisted).unwrap();
    assert!(persisted.contains("continue inside VFS"));
    assert!(!persisted.contains("must not be delivered"));
    assert_eq!(result.unwrap(), "turn 2 complete");
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("continue inside VFS"));
    assert!(prompts[1].contains(AGENT));
    assert_eq!(positions_during_turn, vec![0]);
    assert!(consumed > 0);
    assert_eq!(unread, 0);
}
