//! The standalone host's kernel — one in-process VFS per CLI session.
//!
//! The co-hosted agent's file tools reach a kernel; the CLI's reached the host
//! disk directly. Same engine, same tools, two filesystems — so a hook, a
//! permission gate or an audit row that exists for one host did not exist for
//! the other, and every A2A behaviour had to be tested twice to learn whether
//! it was the engine or the host that made it work.
//!
//! This module closes that: the CLI gets a kernel too. It is a *daemon-free*
//! one — [`Kernel::new`] builds its own runtime and boots with a tempfile
//! metastore — so `scode` still runs with no nexus installed, no cluster, and
//! no network.
//!
//! ## The workspace is mounted, not copied
//!
//! Each root is mounted through `LocalConnectorBackend`, the kernel's
//! reference-mode connector: files stay where they are and remain the single
//! source of truth, so an editor open beside `scode` still edits the same
//! bytes. The kernel is the *access path*, not a second copy.
//!
//! That is also why the metastore stays ephemeral (the boot tempdir
//! `Kernel::new` provides, never `set_metastore_path`): it holds only metadata
//! *about* files whose bytes live on disk, a read that misses it falls through
//! to the connector by path, and persisting it would leave a stale shadow of a
//! tree the user edits from outside.
//!
//! ## What the session can reach
//!
//! Exactly the mounted roots — the workspace, plus any the session declares.
//! An absolute path outside them routes to no mount and is refused, which is
//! the containment the co-hosted agent already has (its workspace-boundary
//! hook) arriving at the CLI through the same mechanism: a mount table.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kernel::kernel::Kernel;
use runtime::{FsBackend, KernelFsBackend};

/// Zone a standalone session's mounts live in.
///
/// A single-process kernel has one zone by construction; naming it `root` is
/// what every other host calls its own, so a path that works here is spelled
/// the same way in the cluster.
const ZONE: &str = "root";

/// A booted single-session kernel and the filesystem the engine drives it by.
///
/// Held for the session's lifetime: the mounts and the metastore live in this
/// `Kernel`, so dropping it drops the session's whole VFS. The engine only
/// ever sees [`Self::fs`].
pub struct LocalKernel {
    /// Kept so the kernel outlives every backend handed out of here — a
    /// `KernelFsBackend` holds an `Arc<Kernel>`, but the mounts are the
    /// session's, not the backend's.
    kernel: Arc<Kernel>,
    fs: Arc<dyn FsBackend>,
    workspace_root: String,
}

impl LocalKernel {
    /// Boot a kernel for a session rooted at `workspace`, also mounting
    /// `extra_roots`.
    ///
    /// `agent_name` becomes the operation context's identity — both principal
    /// and actor, because a standalone session acts for itself, and that is
    /// what the hooks and any audit row attribute the writes to.
    pub fn boot(workspace: &Path, extra_roots: &[PathBuf], agent_name: &str) -> io::Result<Self> {
        let kernel = Arc::new(Kernel::new());
        let workspace_root = mount_host_root(&kernel, workspace)?;
        for root in extra_roots {
            mount_host_root(&kernel, root)?;
        }
        let fs: Arc<dyn FsBackend> = Arc::new(KernelFsBackend::for_agent(
            Arc::clone(&kernel),
            agent_name,
            ZONE,
            agent_name,
            workspace_root.clone(),
        ));
        Ok(Self {
            kernel,
            fs,
            workspace_root,
        })
    }

    /// The filesystem to hand the engine.
    #[must_use]
    pub fn fs(&self) -> Arc<dyn FsBackend> {
        Arc::clone(&self.fs)
    }

    /// The session's workspace as a VFS path — what relative tool paths
    /// resolve against, and what `glob` / `grep` default to.
    #[must_use]
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }

    /// The kernel itself, for a host that needs more than a filesystem.
    #[must_use]
    pub fn kernel(&self) -> Arc<Kernel> {
        Arc::clone(&self.kernel)
    }
}

/// Mount host directory `root` into `kernel` and return its VFS path.
///
/// `follow_symlinks` is on with the connector's own escape detection doing the
/// containment: a symlink out of the root resolves outside it and is refused
/// by the backend, so following one cannot widen what the session reaches.
/// `fsync` is off — the CLI's writes are a developer's working tree, not a
/// replicated log, and paying a flush per tool write would be felt on every
/// edit.
fn mount_host_root(kernel: &Arc<Kernel>, root: &Path) -> io::Result<String> {
    let mount_point = runtime::vfs_path_for_host_path(root)?;
    let backend = backends::storage::local_connector::LocalConnectorBackend::new(root, true, false)?;
    kernel
        .vfs_router_arc()
        .add_mount(&mount_point, ZONE, Some(Arc::new(backend)), false);
    Ok(mount_point)
}
