//! Durable session selection and serialized co-host initialization.

use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use runtime::session_control::SessionStore;
use runtime::spawn_task::{AgentDescriptor, KernelConvenience};
use runtime::HostedSessionIdentity;
use runtime::{FsBackend, HookAbortSignal, Session};

/// Serialize initialization. Legacy transcripts retain this admission guard
/// for their lifetime; committed record contexts release it after activation.
pub(crate) struct SessionLease {
    stop: mpsc::Sender<()>,
    renewal: Option<thread::JoinHandle<()>>,
}

impl SessionLease {
    fn acquire<K: KernelConvenience + Send + Sync + 'static>(
        kernel: &Arc<K>,
        path: &str,
        pid: &str,
        abort: HookAbortSignal,
    ) -> Result<Self, String> {
        let lock = kernel
            .sys_lock(path, "", 1, 60, pid)
            .map_err(|e| format!("co-host: acquire session lease: {e:?}"))?
            .ok_or("co-host: session is still running; stop it before resuming")?;
        let (stop, receiver) = mpsc::channel();
        let thread_kernel = Arc::clone(kernel);
        let thread_path = path.to_string();
        let thread_lock = lock.clone();
        let renewal = thread::Builder::new()
            .name("cohost-session-lease".into())
            .spawn(move || {
                while matches!(
                    receiver.recv_timeout(Duration::from_secs(10)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if !matches!(
                        thread_kernel.sys_lock(&thread_path, &thread_lock, 1, 60, ""),
                        Ok(Some(_))
                    ) {
                        abort.abort();
                        let _ = receiver.recv();
                        break;
                    }
                }
                let _ = thread_kernel.sys_unlock(&thread_path, &thread_lock, false);
            })
            .map_err(|e| {
                let _ = kernel.sys_unlock(path, &lock, false);
                format!("co-host: start session lease renewal: {e}")
            })?;
        Ok(Self {
            stop,
            renewal: Some(renewal),
        })
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(renewal) = self.renewal.take() {
            let _ = renewal.join();
        }
    }
}

pub(crate) fn prepare_session<K: KernelConvenience + Send + Sync + 'static>(
    kernel: &Arc<K>,
    desc: &AgentDescriptor,
    fs: Arc<dyn FsBackend>,
    resume_id: Option<&str>,
    model: &str,
    abort: HookAbortSignal,
) -> Result<(Session, Option<SessionLease>), String> {
    let workspace = fs
        .working_root()
        .map_err(|error| format!("co-host: resolve session workspace: {error}"))?;
    let identity = HostedSessionIdentity {
        agent_name: desc.name.clone(),
        owner_id: desc.owner_id.clone(),
        zone_id: desc.zone_id.clone(),
        repos: desc
            .repos
            .iter()
            .map(|repo| (repo.alias.clone(), repo.mount_path.clone()))
            .collect(),
    };
    if identity.repos.len() != desc.repos.len() {
        return Err("co-host: repository aliases must be unique".into());
    }
    let store = SessionStore::from_cwd_with(&workspace, Arc::clone(&fs))
        .map_err(|e| format!("co-host: open session store: {e}"))?
        .with_agent_name(&desc.name)
        .with_identity(identity.clone());
    let mut session = Session::new();
    let id = resume_id.unwrap_or(&session.session_id).to_string();
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("co-host: resume_session_id must be a durable ID, not a path".into());
    }
    let root = store.sessions_dir().to_string_lossy();
    let lease = SessionLease::acquire(kernel, &format!("{root}/{id}"), &desc.pid, abort)?;
    if resume_id.is_some() {
        session = store
            .load_session(&id)
            .map_err(|e| format!("co-host: restore session: {e}"))?
            .session;
        if session.session_id != id {
            return Err("co-host: transcript ID does not match requested session".into());
        }
    }
    if session.identity.is_none() {
        session.identity = Some(identity);
    }
    if session.model.is_none() {
        session.model = Some(model.to_string());
    }
    let handle = store.create_handle(&id);
    let mut session = session
        .with_workspace_root(workspace)
        .with_persistence_path(handle.path.clone())
        .with_fs_backend(fs);
    // Publish even an empty session before returning its ID. Use the native
    // append-log bootstrap, not snapshot replacement (which would create a
    // regular file or append a duplicate snapshot to an existing DT_STREAM).
    // Only the active writer may publish interrupted tool results. Inspection
    // loads keep them in memory; this host already holds the session lease.
    if resume_id.is_none() {
        session
            .initialize_persistence()
            .map_err(|e| format!("co-host: persist session: {e}"))?;
    } else {
        session
            .activate_persistence()
            .map_err(|error| format!("co-host: activate restored context: {error}"))?;
        session
            .repair_orphan_tool_uses()
            .map_err(|error| format!("co-host: persist interrupted tool recovery: {error}"))?;
    }
    let concurrent = session
        .supports_concurrent_resume()
        .map_err(|error| error.to_string())?;
    let lease = if concurrent {
        drop(lease);
        None
    } else {
        Some(lease)
    };
    Ok((session, lease))
}
