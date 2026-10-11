//! Regression for a real Write tool reporting success without a Nexus write.
//! These calls use the real kernel and the same FsBackend as managed sessions.

use std::sync::Arc;

use kernel::core::agents::registry::AgentDescriptor;
use kernel::kernel::Kernel;
use runtime::{write_file, FsBackend, KernelFsBackend};

mod common;

/// The backend the daemon's trusted service plants, so these tests exercise
/// kernel write outcomes, not the zone authorization the planted-descriptor
/// backends already pass.
fn agent_backend(
    kernel: &Arc<Kernel>,
    owner: &str,
    zone: &str,
    name: &str,
    workspace_root: &str,
) -> KernelFsBackend<Kernel> {
    KernelFsBackend::for_agent_descriptor(
        Arc::clone(kernel),
        &AgentDescriptor {
            pid: format!("pid-{name}"),
            name: name.to_string(),
            owner_id: owner.to_string(),
            zone_id: zone.to_string(),
            ..AgentDescriptor::default()
        },
        workspace_root,
    )
}

#[test]
fn a_write_tool_cannot_report_success_when_no_mount_handled_the_write() {
    let kernel = Arc::new(Kernel::new());
    let fs = agent_backend(&kernel, "owner", "root", "agent", "/proc/p1/workspace");
    let error = write_file(&fs, "missing/proof.txt", "must not disappear")
        .expect_err("an unhandled kernel write must fail the tool");
    assert!(error.to_string().contains("Nexus did not write"));
    assert!(fs.read("missing/proof.txt").is_err());
    assert!(fs
        .append("missing/proof.txt", b"must not disappear")
        .is_err());
}

#[test]
fn a_mount_without_a_rust_writer_is_not_a_completed_file_write() {
    let kernel = Arc::new(Kernel::new());
    kernel
        .vfs_router_arc()
        .add_mount("/workspace", "root", None, false);
    let fs = agent_backend(&kernel, "owner", "root", "agent", "/workspace");
    assert!(write_file(&fs, "proof.txt", "must not disappear").is_err());
    assert!(fs.append("proof.txt", b"must not disappear").is_err());
}

struct DenyAgentRead;

impl kernel::PermissionProvider for DenyAgentRead {
    fn check(
        &self,
        path: &str,
        _route: Option<&kernel::vfs_router::RouteResult>,
        permission: kernel::Permission,
        ctx: &kernel::kernel::OperationContext,
    ) -> Result<(), kernel::kernel::KernelError> {
        if permission == kernel::Permission::Read && ctx.agent_id.as_deref() == Some("agent") {
            Err(kernel::kernel::KernelError::PermissionDenied(
                path.to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn append_preserves_the_original_file_when_reading_it_is_denied() {
    let kernel = Arc::new(Kernel::new());
    common::mount_agent_world(&kernel);
    let fs = agent_backend(&kernel, "owner", "root", "agent", "/agents/agent");
    for path in ["proof", "FileNotFound.txt"] {
        fs.write(path, b"original bytes").unwrap();
    }
    kernel.set_permission_provider(Arc::new(Box::new(DenyAgentRead)));
    let auditor = agent_backend(&kernel, "auditor", "root", "auditor", "/agents/agent");
    for path in ["proof", "FileNotFound.txt"] {
        let error = fs.read(path).expect_err("the agent cannot read this file");
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound, "{path}");
        assert!(fs.append(path, b"replacement").is_err());
        assert_eq!(auditor.read(path).unwrap(), b"original bytes");
    }
}

#[test]
fn a_real_write_tool_round_trips_through_the_managed_repository_alias() {
    for zone in ["root", "edge"] {
        let kernel = Arc::new(Kernel::new());
        common::mount_agent_world(&kernel);
        if zone != "root" {
            kernel.vfs_router_arc().add_mount(
                "/agents",
                zone,
                Some(Arc::new(runtime::test_support::MemObjectStore::default())),
                false,
            );
        }
        let provisioner = agent_backend(&kernel, "owner", "root", "agent", "/");
        let alias = "/proc/p1/workspace/workspace";
        let repository = "/agents/agent/workspaces/s1";
        provisioner.link(alias, repository).unwrap();
        let fs = agent_backend(&kernel, "owner", zone, "agent", "/proc/p1/workspace");
        write_file(&fs, "workspace/nested/proof.txt", "alias bytes").unwrap();
        assert_eq!(
            runtime::read_file(&fs, "workspace/nested/proof.txt", None, None)
                .unwrap()
                .file
                .content,
            "alias bytes"
        );
        assert_eq!(provisioner.read_link(alias).unwrap(), repository);
        assert_eq!(
            fs.read("workspace/nested/proof.txt").unwrap(),
            b"alias bytes"
        );
        assert_eq!(
            fs.read(&format!("{repository}/nested/proof.txt")).unwrap(),
            b"alias bytes"
        );
        fs.append("workspace/nested/proof.txt", b" extended")
            .unwrap();
        assert_eq!(
            fs.read(&format!("{repository}/nested/proof.txt")).unwrap(),
            b"alias bytes extended"
        );
        runtime::edit_file(
            &fs,
            "workspace/nested/proof.txt",
            "alias bytes",
            "edited",
            false,
        )
        .unwrap();
        assert_eq!(
            runtime::read_file(&fs, "workspace/nested/proof.txt", None, None)
                .unwrap()
                .file
                .content,
            "edited extended"
        );
        assert_eq!(
            fs.read(&format!("{repository}/nested/proof.txt")).unwrap(),
            b"edited extended"
        );
    }
}
