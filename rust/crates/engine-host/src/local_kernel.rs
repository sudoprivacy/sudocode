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
//! The whole filesystem root is mounted (the drive root on Windows, `/` on
//! unix), so an absolute path anywhere on disk resolves through the one VFS
//! the file tools drive -- a session is not confined to its launch
//! directory. The workspace and any declared `additionalDirectories` mount ON
//! TOP of it; the router's longest-prefix match means a path under the
//! workspace still hits the workspace mount, and only paths outside fall
//! through to the filesystem-root mount. This is reach, not security: the
//! sandbox is gvisor's, and this kernel is the access path (hooks, audit, the
//! one VFS the co-host also speaks), not a containment boundary.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kernel::kernel::Kernel;
use kernel::kernel::OperationContext;
use runtime::{FsBackend, KernelFsBackend};

/// Zone a standalone session's mounts live in.
///
/// A single-process kernel has one zone by construction; naming it `root` is
/// what every other host calls its own, so a path that works here is spelled
/// the same way in the cluster.
const ZONE: &str = "root";

/// Boot a kernel for a session rooted at `workspace`, also mounting
/// `extra_roots`, and return the filesystem the engine drives it by.
///
/// The kernel itself is not returned and needs no keeper: the backend holds it,
/// so the session's mounts and metastore live exactly as long as the filesystem
/// the engine is using.
///
/// `agent_name` becomes the operation context's identity — both principal and
/// actor, because a standalone session acts for itself, and that is what the
/// hooks and any audit row attribute its writes to.
pub fn boot_session_fs(
    workspace: &Path,
    extra_roots: &[PathBuf],
    agent_name: &str,
) -> io::Result<Arc<dyn FsBackend>> {
    let kernel = Arc::new(Kernel::new());
    // Mount the whole filesystem root (drive root on Windows, / on unix)
    // FIRST, so an absolute path anywhere on disk routes to a mount instead
    // of being refused. Workspace + extra roots mount ON TOP: the router
    // matches the longest prefix, so a path under the workspace still hits
    // the workspace mount, and only paths outside it fall through to the
    // filesystem-root mount. A reach, not a security boundary: gvisor is the
    // sandbox; this kernel is the access path (hooks, audit, the one VFS the
    // co-host also speaks), not the containment.
    if let Some(fs_root) = filesystem_root_of(workspace) {
        mount_host_root(&kernel, &fs_root)?;
    }
    let workspace_root = mount_host_root(&kernel, workspace)?;
    for root in extra_roots {
        mount_host_root(&kernel, root)?;
    }
    // Host spelling: a CLI session's files are host files, so the paths its
    // tools report are the ones its user typed and its `bash` tool can open.
    // The context is SYSTEM: a standalone session acts for itself on a host
    // the user booted, so its zone context is trusted-local (`is_system`),
    // not the fail-closed unverified-delegation source `for_agent` carries —
    // that constructor is for kernel-side agents whose authority must come
    // from a planted descriptor, and would refuse every CLI file operation.
    let ctx = OperationContext::new(agent_name, ZONE, true, Some(agent_name), true);
    Ok(Arc::new(
        KernelFsBackend::new(kernel, ctx, workspace_root)
            .with_host_root(workspace.to_string_lossy().into_owned()),
    ))
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
    let backend =
        backends::storage::local_connector::LocalConnectorBackend::new(root, true, false)?;
    kernel
        .vfs_router_arc()
        .add_mount(&mount_point, ZONE, Some(Arc::new(backend)), false);
    Ok(mount_point)
}

/// The filesystem root that contains path: the drive root on Windows
/// (C:\ for C:\a\b), or / on unix. None when path has no root
/// component to lift (a relative path); the caller then mounts only the
/// workspace and extras as before.
///
/// Mounting this makes every on-disk absolute path reachable through the one
/// VFS a session drives, so the file tools are not confined to the launch
/// directory. Not `cfg(windows)`-gated: `Path::ancestors().last()` yields
/// the root on both platforms, so a unix test exercises the same code.
fn filesystem_root_of(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    path.ancestors().last().map(Path::to_path_buf)
}
