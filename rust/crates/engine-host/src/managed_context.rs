//! The one value a managed agent's host is built from.
//!
//! A managed agent is bound by its host registry (who it is) and reaches the
//! VFS through exactly one backend (where its effects land). Both are resolved
//! here, once, before any session exists. The session store and its writer
//! lease, the file tools, the mailbox and the model mount all take this
//! backend, so a managed agent has no second path to local disk, a private
//! kernel or a direct model endpoint.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use runtime::fs_backend::FsLock;
use runtime::mailbox::Mailbox;
use runtime::spawn_task::{AgentDescriptor, KernelConvenience};
use runtime::{FsBackend, KernelFsAccess, KernelFsBackend, NexusVfsFsBackend};

/// How long a session lease outlives its last renewal; renewed every 10 s.
pub(crate) const SESSION_LEASE_TTL_SECS: u64 = 60;

/// Takes the exclusive writer lease on a session path: `Ok(None)` when
/// another writer holds it. Errors are rendered for the refusal message.
pub(crate) type SessionLocks =
    Arc<dyn Fn(&str) -> Result<Option<Arc<dyn FsLock>>, String> + Send + Sync>;

/// A managed agent's resolved runtime context: the registry binding plus the
/// single backend every effect goes through.
pub struct ManagedRuntimeContext {
    /// The binding the host registry holds for this agent instance: pid,
    /// name, owner, zone, repository mounts and model label.
    pub(crate) desc: AgentDescriptor,
    /// The one backend for sessions, leases, file tools, mailbox and model.
    pub(crate) fs: Arc<dyn FsBackend>,
    /// The native directory backing the workspace, when it has one; the
    /// shell runs there. `None` gives the agent a host directory of its own.
    pub(crate) shell_root: Option<PathBuf>,
    /// The agent's conversations, over the same backend: its identity, so
    /// every session and the host's receiver share this one instance.
    pub(crate) mailbox: Arc<Mailbox>,
    /// Who fences a session's writers. A co-host takes the lease as the host,
    /// on its own kernel; an out-of-process agent can only take it under its
    /// own credential, on the daemon that stores the session.
    pub(crate) session_locks: SessionLocks,
    /// The first lease release that did not succeed, for the host's teardown
    /// outcome. A failed release keeps the lock until its TTL, which errs
    /// toward exclusivity, but it is still not a confirmed cleanup.
    lease_release_failure: Arc<Mutex<Option<String>>>,
}

/// A co-host's session lease: a kernel lock held by the agent's pid, taken
/// and released as the host, below the agent's permission checks.
struct KernelLease<K: KernelConvenience> {
    kernel: Arc<K>,
    path: String,
    id: String,
    release_failure: Arc<Mutex<Option<String>>>,
}

impl<K: KernelConvenience + Send + Sync> FsLock for KernelLease<K> {
    fn renew(&self) -> std::io::Result<bool> {
        Ok(matches!(
            self.kernel
                .sys_lock(&self.path, &self.id, 1, SESSION_LEASE_TTL_SECS, ""),
            Ok(Some(_))
        ))
    }
}

impl<K: KernelConvenience> Drop for KernelLease<K> {
    fn drop(&mut self) {
        let failure = match self.kernel.sys_unlock(&self.path, &self.id, false) {
            Ok(true) => return,
            Ok(false) => format!("session lease {} was no longer held at release", self.path),
            Err(error) => format!("release session lease {}: {error:?}", self.path),
        };
        self.release_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(failure);
    }
}

impl ManagedRuntimeContext {
    /// An agent co-hosted inside the daemon, on the daemon's kernel.
    ///
    /// File tools reach the kernel, not local disk: a write passes the hooks,
    /// the audit trail and the permission checks a `std::fs` write never sees.
    /// `authenticated_senders` is the daemon's auth posture: when armed, its
    /// stamping hook authenticates every conversation writer.
    pub fn cohost<K>(
        kernel: &Arc<K>,
        desc: &AgentDescriptor,
        authenticated_senders: bool,
    ) -> Result<Self, String>
    where
        K: KernelConvenience + KernelFsAccess + Send + Sync + 'static,
    {
        let primary_repo = desc
            .repos
            .iter()
            .find(|repo| repo.alias == "workspace")
            .or_else(|| desc.repos.first());
        let context = runtime::agent_operation_context(&desc.owner_id, &desc.zone_id, &desc.name);
        let local_directory = primary_repo
            .map(|repo| kernel.local_workspace_directory(&repo.mount_path, &context))
            .transpose()
            .map_err(|error| format!("co-host: resolve project directory: {error:?}"))?
            .flatten();
        let workspace_root = local_directory.as_ref().and(primary_repo).map_or_else(
            || format!("/proc/{}/workspace", desc.pid),
            |repo| repo.mount_path.clone(),
        );
        // Built from the complete planted descriptor so the agent's delegation
        // reference participates in every target authorization the backend
        // makes (zone-scoped ResourceRef checks), rather than trusting
        // caller-supplied owner/zone/name strings.
        let fs = KernelFsBackend::for_agent_descriptor(Arc::clone(kernel), desc, workspace_root);
        let fs: Arc<dyn FsBackend> = Arc::new(match &local_directory {
            Some(directory) => fs
                .with_local_workspace(directory)
                .map_err(|error| format!("co-host: bind project directory: {error}"))?,
            None => fs,
        });
        let lock_kernel = Arc::clone(kernel);
        let pid = desc.pid.clone();
        let lease_release_failure = Arc::new(Mutex::new(None));
        let release_failure = Arc::clone(&lease_release_failure);
        let session_locks: SessionLocks = Arc::new(move |path: &str| {
            let id = lock_kernel
                .sys_lock(path, "", 1, SESSION_LEASE_TTL_SECS, &pid)
                .map_err(|e| format!("{e:?}"))?;
            Ok(id.map(|id| {
                Arc::new(KernelLease {
                    kernel: Arc::clone(&lock_kernel),
                    path: path.to_owned(),
                    id,
                    release_failure: Arc::clone(&release_failure),
                }) as Arc<dyn FsLock>
            }))
        });
        let mut context = Self::assemble(
            desc,
            fs,
            local_directory,
            authenticated_senders,
            session_locks,
        );
        context.lease_release_failure = lease_release_failure;
        Ok(context)
    }

    /// An agent hosted out of process, on `client`: an already-authenticated
    /// connection to the daemon that stores its state, resolved by the host
    /// for this agent instance together with `desc`. `workspace_root` is the
    /// VFS path of the agent's workspace; `shell_root` the native directory
    /// its shell runs in, when the workspace has one.
    ///
    /// Refuses unless the daemon answers for the workspace over this
    /// connection, before any session, file or model effect. The daemon
    /// reports a refused credential as absence, so an unanswered workspace is
    /// a context that cannot be used. There is no fallback to local disk, a
    /// private kernel or a direct model endpoint.
    pub fn remote(
        desc: &AgentDescriptor,
        client: Arc<nexus_vfs_client::NexusVfsClient>,
        workspace_root: &str,
        shell_root: Option<PathBuf>,
        authenticated_senders: bool,
    ) -> Result<Self, String> {
        let fs: Arc<dyn FsBackend> = Arc::new(NexusVfsFsBackend::for_managed_workspace(
            client,
            &desc.zone_id,
            &desc.name,
            workspace_root,
        ));
        match fs.stat(workspace_root) {
            Ok(metadata) if metadata.is_dir => {}
            Ok(_) => {
                return Err(format!(
                    "managed workspace {workspace_root} is not a directory"
                ))
            }
            Err(error) => {
                return Err(format!(
                    "managed connection cannot reach workspace {workspace_root}: {error}"
                ))
            }
        }
        let lock_fs = Arc::clone(&fs);
        let session_locks: SessionLocks = Arc::new(move |path: &str| {
            lock_fs
                .try_lock(path, SESSION_LEASE_TTL_SECS * 1000)
                .map_err(|e| e.to_string())
        });
        Ok(Self::assemble(
            desc,
            fs,
            shell_root,
            authenticated_senders,
            session_locks,
        ))
    }

    fn assemble(
        desc: &AgentDescriptor,
        fs: Arc<dyn FsBackend>,
        shell_root: Option<PathBuf>,
        authenticated_senders: bool,
        session_locks: SessionLocks,
    ) -> Self {
        let mailbox = Arc::new(
            Mailbox::daemon_absolute(Arc::clone(&fs), desc.name.clone())
                .with_authenticated_senders(authenticated_senders),
        );
        Self {
            desc: desc.clone(),
            fs,
            shell_root,
            mailbox,
            session_locks,
            lease_release_failure: Arc::default(),
        }
    }

    /// The agent instance this context is bound to.
    #[must_use]
    pub fn descriptor(&self) -> &AgentDescriptor {
        &self.desc
    }

    /// The agent's conversations: the one mailbox every session sends on
    /// and the host receives on.
    #[must_use]
    pub fn mailbox(&self) -> Arc<Mailbox> {
        Arc::clone(&self.mailbox)
    }

    /// Where an observed lease release failure is recorded. A host reads it
    /// after teardown, because a lease is released when its session engine
    /// ends rather than when the host asks.
    #[must_use]
    pub fn lease_release_failure(&self) -> Arc<Mutex<Option<String>>> {
        Arc::clone(&self.lease_release_failure)
    }
}
