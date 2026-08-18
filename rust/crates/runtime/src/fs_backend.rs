//! Unified filesystem backend trait.
//!
//! Abstracts file operations so sudo-code works identically whether running
//! standalone (`StdFsBackend` → `std::fs`), in-process inside nexusd
//! (`KernelFsBackend` → kernel syscalls), or remote (`NexusVfsClient` → gRPC).
//!
//! Cold paths (session persistence, config loading) use `Arc<dyn FsBackend>`
//! for simplicity. Hot paths (tool-facing file_ops) take `&dyn FsBackend`
//! to avoid the Arc overhead.

use std::io;
use std::sync::Arc;

use crate::workspace_root::current_workspace_root;
use kernel::kernel::syscall::{KernelSyscall, ReaddirOpts};
use kernel::kernel::OperationContext;

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

/// Filesystem metadata returned by [`FsBackend::stat`].
#[derive(Debug, Clone)]
pub struct FsMetadata {
    pub len: u64,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub modified: Option<std::time::SystemTime>,
}

/// Directory entry returned by [`FsBackend::readdir`].
#[derive(Debug, Clone)]
pub struct FsDirEntry {
    pub name: String,
    pub is_dir: bool,
}

// ---------------------------------------------------------------------------
// FsBackend trait
// ---------------------------------------------------------------------------

/// Unified filesystem abstraction.
///
/// Every method mirrors a common `std::fs` operation. Implementations must be
/// thread-safe so a single backend can be shared across the runtime.
pub trait FsBackend: Send + Sync + 'static {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()>;
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()>;
    fn delete(&self, path: &str) -> io::Result<()>;
    fn stat(&self, path: &str) -> io::Result<FsMetadata>;
    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>>;
    fn exists(&self, path: &str) -> io::Result<bool>;
    fn create_dir_all(&self, path: &str) -> io::Result<()>;
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    fn canonicalize(&self, path: &str) -> io::Result<String>;
    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata>;

    /// Convenience: read a file and decode as UTF-8.
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8"))
    }

    /// Atomic write: write to a temporary file then rename into place.
    fn write_atomic(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let temp = format!("{path}.tmp.{}", std::process::id());
        self.write(&temp, data)?;
        self.rename(&temp, path)
    }

    /// The absolute working root that relative paths resolve against and
    /// that `glob` / `grep` fall back to when given no explicit path.
    /// `StdFsBackend` → the process cwd; `KernelFsBackend` → the agent's
    /// VFS workspace; the gRPC backend → the server root.
    fn working_root(&self) -> io::Result<String> {
        Ok(String::from("/"))
    }

    /// Resolve `path` to the absolute, backend-native form the other
    /// methods expect. Relative paths resolve against [`FsBackend::working_root`].
    ///
    /// The default is a pure *lexical* resolution (no host filesystem
    /// touch) suitable for VFS / remote backends whose namespaces are
    /// already canonical and whose paths must NOT be canonicalised against
    /// the host — `StdFsBackend` overrides it with `current_dir` +
    /// `canonicalize` host semantics.
    fn normalize(&self, path: &str) -> io::Result<String> {
        Ok(lexical_join(&self.working_root()?, path))
    }

    /// Like [`FsBackend::normalize`] but tolerates a missing final
    /// component (write targets whose leaf does not yet exist). The lexical
    /// default already tolerates missing paths; `StdFsBackend` overrides
    /// for host canonicalisation that falls back when the leaf is absent.
    fn normalize_allow_missing(&self, path: &str) -> io::Result<String> {
        self.normalize(path)
    }

    /// Join a directory path and a child name in the backend's NATIVE path
    /// form. The default is forward-slash (VFS / remote). `StdFsBackend`
    /// overrides with the host separator — critical on Windows, where a
    /// verbatim `\\?\C:\…` base (what `canonicalize` yields) rejects a
    /// forward slash, so a `/`-joined child would be an unreadable path.
    /// Used by the `readdir`-composed `glob` / `grep` traversal to build
    /// child paths the other methods can then read.
    fn join_path(&self, dir: &str, name: &str) -> String {
        format!("{}/{}", dir.trim_end_matches(['/', '\\']), name)
    }
}

/// Resolve `path` against `root` without touching any filesystem.
///
/// Absolute paths (leading `/`) are taken as-is; relative paths are joined
/// onto `root`. `.` and `..` components are collapsed lexically and the
/// result is emitted with forward-slash separators (VFS-native). A `..`
/// that would escape the root is clamped at `/` (no host traversal). This
/// is the resolution the VFS-backed [`KernelFsBackend`] uses; host paths
/// go through `StdFsBackend`'s `canonicalize` override instead.
fn lexical_join(root: &str, path: &str) -> String {
    let combined = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", root.trim_end_matches('/'), path)
    };
    let mut stack: Vec<&str> = Vec::new();
    for seg in combined.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    format!("/{}", stack.join("/"))
}

// Blanket impl: Arc<dyn FsBackend> delegates to the inner backend.
impl FsBackend for Arc<dyn FsBackend> {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        (**self).read(path)
    }
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        (**self).write(path, data)
    }
    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        (**self).append(path, data)
    }
    fn delete(&self, path: &str) -> io::Result<()> {
        (**self).delete(path)
    }
    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        (**self).stat(path)
    }
    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        (**self).readdir(path)
    }
    fn exists(&self, path: &str) -> io::Result<bool> {
        (**self).exists(path)
    }
    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        (**self).create_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        (**self).rename(from, to)
    }
    fn canonicalize(&self, path: &str) -> io::Result<String> {
        (**self).canonicalize(path)
    }
    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        (**self).symlink_metadata(path)
    }
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        (**self).read_to_string(path)
    }
    fn write_atomic(&self, path: &str, data: &[u8]) -> io::Result<()> {
        (**self).write_atomic(path, data)
    }
    fn working_root(&self) -> io::Result<String> {
        (**self).working_root()
    }
    fn normalize(&self, path: &str) -> io::Result<String> {
        (**self).normalize(path)
    }
    fn normalize_allow_missing(&self, path: &str) -> io::Result<String> {
        (**self).normalize_allow_missing(path)
    }
    fn join_path(&self, dir: &str, name: &str) -> String {
        (**self).join_path(dir, name)
    }
}

// ---------------------------------------------------------------------------
// StdFsBackend — zero-size, compiler inlines all methods
// ---------------------------------------------------------------------------

/// Standard-library filesystem backend for standalone CLI usage.
///
/// Zero-size struct — the compiler monomorphizes and inlines every call
/// down to a direct `std::fs` syscall wrapper with no indirection.
pub struct StdFsBackend;

impl FsBackend for StdFsBackend {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)
    }

    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        file.write_all(data)
    }

    fn delete(&self, path: &str) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        let meta = std::fs::metadata(path)?;
        Ok(FsMetadata {
            len: meta.len(),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            is_symlink: false, // metadata() follows symlinks
            modified: meta.modified().ok(),
        })
    }

    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            entries.push(FsDirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
            });
        }
        Ok(entries)
    }

    fn exists(&self, path: &str) -> io::Result<bool> {
        Ok(std::path::Path::new(path).exists())
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn canonicalize(&self, path: &str) -> io::Result<String> {
        Ok(std::fs::canonicalize(path)?.to_string_lossy().into_owned())
    }

    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(FsMetadata {
            len: meta.len(),
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            is_symlink: meta.is_symlink(),
            modified: meta.modified().ok(),
        })
    }

    fn read_to_string(&self, path: &str) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write_atomic(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let temp = format!("{path}.tmp.{}", std::process::id());
        std::fs::write(&temp, data)?;
        std::fs::rename(&temp, path)
    }

    /// The turn's workspace root (see [`crate::workspace_root`]), which is
    /// the process cwd outside an ACP turn. Relative tool paths resolve
    /// against it, so concurrent sessions in different directories never
    /// see each other's files.
    fn working_root(&self) -> io::Result<String> {
        Ok(current_workspace_root()?.to_string_lossy().into_owned())
    }

    fn normalize(&self, path: &str) -> io::Result<String> {
        let candidate = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            current_workspace_root()?.join(path)
        };
        Ok(candidate.canonicalize()?.to_string_lossy().into_owned())
    }

    fn normalize_allow_missing(&self, path: &str) -> io::Result<String> {
        let candidate = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            current_workspace_root()?.join(path)
        };
        if let Ok(canonical) = candidate.canonicalize() {
            return Ok(canonical.to_string_lossy().into_owned());
        }
        if let Some(parent) = candidate.parent() {
            let canonical_parent = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            if let Some(name) = candidate.file_name() {
                return Ok(canonical_parent.join(name).to_string_lossy().into_owned());
            }
        }
        Ok(candidate.to_string_lossy().into_owned())
    }

    fn join_path(&self, dir: &str, name: &str) -> String {
        std::path::Path::new(dir)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

// ---------------------------------------------------------------------------
// KernelFsBackend — in-process kernel syscalls for nexusd
// ---------------------------------------------------------------------------

/// Kernel-backed filesystem for in-process execution inside nexusd.
///
/// Forwards every operation through the [`KernelSyscall`] trait so managed
/// agents read/write the VFS trie via `sys_read` / `sys_write` /
/// `sys_stat` / `sys_readdir` instead of touching the host
/// filesystem.
pub struct KernelFsBackend<K: KernelSyscall> {
    kernel: Arc<K>,
    ctx: OperationContext,
    /// Absolute VFS path that relative tool paths resolve against and that
    /// `glob` / `grep` default to (e.g. `/proc/{pid}/workspace`).
    workspace_root: String,
}

impl<K: KernelSyscall> KernelFsBackend<K> {
    pub fn new(kernel: Arc<K>, ctx: OperationContext, workspace_root: impl Into<String>) -> Self {
        Self {
            kernel,
            ctx,
            workspace_root: workspace_root.into(),
        }
    }

    /// Build a backend for a managed agent from its descriptor identity.
    /// The [`OperationContext`] is the agent's own (owner / zone / name);
    /// `workspace_root` is the absolute VFS path relative tool paths
    /// resolve against (typically the agent's procfs workspace subtree).
    /// Constructing the context here keeps `kernel`-crate types off the
    /// `tools`-crate factory that calls this.
    pub fn for_agent(
        kernel: Arc<K>,
        owner_id: &str,
        zone_id: &str,
        agent_name: &str,
        workspace_root: impl Into<String>,
    ) -> Self {
        let ctx = OperationContext::new(owner_id, zone_id, false, Some(agent_name), true);
        Self::new(kernel, ctx, workspace_root)
    }
}

/// Map a kernel error to an `io::Error`.
fn kernel_err(e: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{e:?}"))
}

impl<K: KernelSyscall + Send + Sync + 'static> FsBackend for KernelFsBackend<K> {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.kernel
            .sys_read(path, &self.ctx, 0, 0)
            .map_err(kernel_err)
            .and_then(|r| {
                r.data.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("{path}: no data"))
                })
            })
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.kernel
            .sys_write(path, &self.ctx, data, 0)
            .map_err(kernel_err)
            .map(|_| ())
    }

    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        // Compose: read existing content, concatenate, write back.
        // Kernel pipes/streams have append semantics at offset != 0 but
        // regular files don't, so the read-concat-write path is safest.
        let existing = self.read(path).unwrap_or_default();
        let mut combined = existing;
        combined.extend_from_slice(data);
        self.write(path, &combined)
    }

    fn delete(&self, path: &str) -> io::Result<()> {
        self.kernel
            .sys_unlink(path, &self.ctx, false)
            .map_err(kernel_err)
            .map(|_| ())
    }

    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        self.kernel
            .sys_stat(path, &self.ctx.zone_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path}: not found")))
            .map(|s| FsMetadata {
                len: s.size,
                is_dir: s.is_directory,
                is_file: !s.is_directory,
                is_symlink: false,
                modified: s
                    .modified_at_ms
                    .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)),
            })
    }

    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        let zone = &self.ctx.zone_id;
        // `sys_readdir` returns `Vec<(child_GLOBAL_path, entry_type)>` —
        // full paths like `/ws/a.rs`, not basenames. The `FsBackend`
        // contract (matching `StdFsBackend` / `NexusVfsFsBackend`) is the
        // basename, so strip the leading directory. DT_DIR == 1.
        // Single-level (default `ReaddirOpts`): the `FsBackend` contract is a
        // basename listing of direct children, so no recursive descent here.
        let entries = self
            .kernel
            .sys_readdir(path, zone, false, ReaddirOpts::default());
        Ok(entries
            .into_iter()
            .map(|(child, entry_type)| FsDirEntry {
                name: child
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&child)
                    .to_string(),
                is_dir: entry_type == 1, // DT_DIR
            })
            .collect())
    }

    fn exists(&self, path: &str) -> io::Result<bool> {
        Ok(self.kernel.sys_stat(path, &self.ctx.zone_id).is_some())
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        // Writing `/ws/sub/c.rs` creates only the leaf's metastore entry —
        // the intermediate `/ws/sub` dirent is NOT auto-planted, so a later
        // `readdir("/ws")` would not see `sub` and a recursive walk could
        // not descend. Plant an explicit DT_DIR for every intermediate
        // component (idempotent: re-setattr on an existing dir is a no-op).
        let mut prefix = String::new();
        for comp in path.split(['/', '\\']).filter(|c| !c.is_empty()) {
            prefix.push('/');
            prefix.push_str(comp);
            // Skip if it already exists as a directory.
            if self
                .kernel
                .sys_stat(&prefix, &self.ctx.zone_id)
                .is_some_and(|s| s.is_directory)
            {
                continue;
            }
            let _ = self.kernel.sys_setattr(
                &prefix,
                1, // DT_DIR
                "",
                None,
                None,
                None,
                "balanced",
                &self.ctx.zone_id,
                false,
                0,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            );
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        // No native rename syscall — compose from read + write + delete.
        let data = self.read(from)?;
        self.write(to, &data)?;
        self.delete(from)?;
        Ok(())
    }

    fn canonicalize(&self, path: &str) -> io::Result<String> {
        // VFS paths are already canonical — no host symlinks to resolve.
        Ok(path.to_string())
    }

    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        // VFS has no symlinks — delegate to regular stat.
        self.stat(path)
    }

    fn write_atomic(&self, path: &str, data: &[u8]) -> io::Result<()> {
        // Kernel sys_write is atomic per call — no temp file needed.
        self.write(path, data)
    }

    fn working_root(&self) -> io::Result<String> {
        Ok(self.workspace_root.clone())
    }
    // `normalize` / `normalize_allow_missing` use the trait's lexical
    // default: VFS paths are already canonical, and canonicalising them
    // against the host (`StdFsBackend`'s override) would corrupt them.
}

// ---------------------------------------------------------------------------
// NexusVfsClient adapter — standalone CLI fallback when gRPC nexus is
// available but no in-process kernel.
// ---------------------------------------------------------------------------

/// Wraps a [`nexus_vfs_client::NexusVfsClient`] + auth token to implement
/// [`FsBackend`]. Used by the standalone CLI when `NEXUS_VFS_SOCK` is set.
pub struct NexusVfsFsBackend {
    client: nexus_vfs_client::NexusVfsClient,
    auth_token: String,
}

impl NexusVfsFsBackend {
    pub fn new(client: nexus_vfs_client::NexusVfsClient, auth_token: String) -> Self {
        Self { client, auth_token }
    }
}

impl FsBackend for NexusVfsFsBackend {
    fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        self.client.read(path, &self.auth_token)
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.client.write(path, data.to_vec(), &self.auth_token)
    }

    fn append(&self, path: &str, data: &[u8]) -> io::Result<()> {
        let mut existing = self.client.read(path, &self.auth_token).unwrap_or_default();
        existing.extend_from_slice(data);
        self.client.write(path, existing, &self.auth_token)
    }

    fn delete(&self, path: &str) -> io::Result<()> {
        self.client.delete(path, &self.auth_token)
    }

    fn stat(&self, path: &str) -> io::Result<FsMetadata> {
        let stat = self.client.stat(path, &self.auth_token)?;
        Ok(FsMetadata {
            len: stat.size,
            is_dir: stat.is_directory,
            is_file: !stat.is_directory,
            is_symlink: false,
            modified: stat
                .modified_at_ms
                .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)),
        })
    }

    fn readdir(&self, path: &str) -> io::Result<Vec<FsDirEntry>> {
        let entries = self.client.readdir(path, &self.auth_token)?;
        Ok(entries
            .into_iter()
            .map(|e| FsDirEntry {
                name: e.name,
                is_dir: e.is_directory,
            })
            .collect())
    }

    fn exists(&self, path: &str) -> io::Result<bool> {
        Ok(self.client.stat(path, &self.auth_token).is_ok())
    }

    fn create_dir_all(&self, _path: &str) -> io::Result<()> {
        // VFS servers typically auto-create intermediate paths on write.
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let data = self.read(from)?;
        self.write(to, &data)?;
        self.delete(from)?;
        Ok(())
    }

    fn canonicalize(&self, path: &str) -> io::Result<String> {
        // VFS paths are already canonical over gRPC — no host symlinks.
        Ok(path.to_string())
    }

    fn symlink_metadata(&self, path: &str) -> io::Result<FsMetadata> {
        // VFS has no symlinks — delegate to regular stat.
        self.stat(path)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(label: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        format!(
            "{}/fs-backend-{label}-{nanos}",
            std::env::temp_dir().display()
        )
    }

    #[test]
    fn std_backend_read_write_round_trip() {
        let path = temp_path("rw");
        let fs = StdFsBackend;
        fs.write(&path, b"hello world").unwrap();
        let data = fs.read(&path).unwrap();
        assert_eq!(data, b"hello world");
        let text = fs.read_to_string(&path).unwrap();
        assert_eq!(text, "hello world");
        fs.delete(&path).unwrap();
    }

    #[test]
    fn std_backend_append() {
        let path = temp_path("append");
        let fs = StdFsBackend;
        fs.write(&path, b"line1\n").unwrap();
        fs.append(&path, b"line2\n").unwrap();
        let text = fs.read_to_string(&path).unwrap();
        assert_eq!(text, "line1\nline2\n");
        fs.delete(&path).unwrap();
    }

    #[test]
    fn std_backend_stat() {
        let path = temp_path("stat");
        let fs = StdFsBackend;
        fs.write(&path, b"twelve bytes").unwrap();
        let meta = fs.stat(&path).unwrap();
        assert_eq!(meta.len, 12);
        assert!(meta.is_file);
        assert!(!meta.is_dir);
        fs.delete(&path).unwrap();
    }

    #[test]
    fn std_backend_exists() {
        let path = temp_path("exists");
        let fs = StdFsBackend;
        assert!(!fs.exists(&path).unwrap());
        fs.write(&path, b"x").unwrap();
        assert!(fs.exists(&path).unwrap());
        fs.delete(&path).unwrap();
    }

    #[test]
    fn std_backend_create_dir_all_and_readdir() {
        let dir = temp_path("readdir");
        let fs = StdFsBackend;
        fs.create_dir_all(&dir).unwrap();
        fs.write(&format!("{dir}/a.txt"), b"a").unwrap();
        fs.write(&format!("{dir}/b.txt"), b"b").unwrap();
        let entries = fs.readdir(&dir).unwrap();
        assert_eq!(entries.len(), 2);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"b.txt"));
        fs.delete(&format!("{dir}/a.txt")).unwrap();
        fs.delete(&format!("{dir}/b.txt")).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn std_backend_rename() {
        let src = temp_path("rename-src");
        let dst = temp_path("rename-dst");
        let fs = StdFsBackend;
        fs.write(&src, b"moved").unwrap();
        fs.rename(&src, &dst).unwrap();
        assert!(!fs.exists(&src).unwrap());
        assert_eq!(fs.read_to_string(&dst).unwrap(), "moved");
        fs.delete(&dst).unwrap();
    }

    #[test]
    fn std_backend_write_atomic() {
        let path = temp_path("atomic");
        let fs = StdFsBackend;
        fs.write_atomic(&path, b"safe content").unwrap();
        assert_eq!(fs.read_to_string(&path).unwrap(), "safe content");
        fs.delete(&path).unwrap();
    }

    #[test]
    fn arc_dyn_backend_delegates() {
        let path = temp_path("arc");
        let fs: Arc<dyn FsBackend> = Arc::new(StdFsBackend);
        fs.write(&path, b"via arc").unwrap();
        assert_eq!(fs.read_to_string(&path).unwrap(), "via arc");
        fs.delete(&path).unwrap();
    }
}
